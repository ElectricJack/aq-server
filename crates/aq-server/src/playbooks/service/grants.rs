use super::*;

impl Service {
    fn validate_new_scope(&self, scope: &GrantScope) -> Result<()> {
        self.profile
            .validate_scope_with_extensions(scope, |name| self.plugins.get(name).is_some())
    }
    pub(in crate::playbooks) fn grant_authority(
        &self,
        id: Uuid,
        revision: i64,
        scope: &GrantScope,
    ) -> Result<aq_playbook_host::runner::Authority> {
        // Grants in storage were approved against the installed extensions at
        // issuance. Keep their authority identity after plugin removal.
        self.profile
            .authority_with_extensions(id, revision, scope, |_| true)
    }
    fn scoped_grant(&self, id: Uuid, scope: &GrantScope) -> bool {
        let expected = (self.namespace.project_id, self.namespace.workspace_id);
        !id.is_nil() && (scope.project_id, scope.workspace_id) == expected
    }
    pub async fn grant(
        &self,
        actor: Uuid,
        id: Uuid,
        scope: GrantScope,
        expires: i64,
    ) -> Result<()> {
        self.grant_for(actor, id, scope, expires, None).await
    }
    /// A grant, and with `resume` the approval to resume that delivery
    /// decision under it; both commit together.
    pub(in crate::playbooks) async fn grant_for(
        &self,
        actor: Uuid,
        id: Uuid,
        scope: GrantScope,
        expires: i64,
        resume: Option<Uuid>,
    ) -> Result<()> {
        self.validate_new_scope(&scope)?;
        if actor != self.namespace.owner_user_id {
            return Err(Error::Forbidden);
        }
        if !self.scoped_grant(id, &scope) {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, actor).await?;
        aq_playbook_domain::grants::retain(
            &mut tx,
            &self.namespace,
            id,
            &scope,
            expires,
            self.now()?,
            resume,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    /// The grant the server holds for a playbook source on the owner's
    /// behalf (spec 0013 ruling R2): written as the namespace owner, and it
    /// never expires. [`Self::grant`] keeps its 24 h cap.
    pub async fn source_grant(&self, id: Uuid, scope: GrantScope) -> Result<()> {
        self.validate_new_scope(&scope)?;
        let (ns, actor) = (&self.namespace, self.namespace.owner_user_id);
        if !self.scoped_grant(id, &scope) {
            return Err(Error::Forbidden);
        }
        let expires = aq_playbook_authoring::SOURCE_GRANT_EXPIRES_AT_MS;
        let payload = digest(&(ns.namespace_id, ns.generation, id, &scope, expires, actor))?;
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, actor).await?;
        sqlx::query("INSERT INTO playbook_grants(grant_id,namespace_id,generation,revision,owner_user_id,scope,expires_at_ms,payload_digest) VALUES($1,$2,$3,1,$4,$5,$6,$7) ON CONFLICT(grant_id) DO NOTHING").bind(id).bind(ns.namespace_id).bind(ns.generation).bind(actor).bind(json!(scope)).bind(expires).bind(payload).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn revoke(&self, actor: Uuid, id: Uuid) -> Result<()> {
        if actor != self.namespace.owner_user_id {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, actor).await?;
        let changed=sqlx::query("UPDATE playbook_grants SET revoked_at_ms=COALESCE(revoked_at_ms,$4) WHERE grant_id=$1 AND namespace_id=$2 AND generation=$3").bind(id).bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(self.now()?).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(Error::Forbidden);
        }
        tx.commit().await?;
        Ok(())
    }
    pub(in crate::playbooks) async fn grant_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        grant: GrantRevision,
        live: bool,
    ) -> Result<GrantScope> {
        self.grant_record_locked(tx, grant, GrantCheck::owner(live))
            .await
    }
    pub(super) async fn grant_record_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        grant: GrantRevision,
        check: GrantCheck,
    ) -> Result<GrantScope> {
        let GrantRevision { id, revision } = grant;
        let row=sqlx::query("SELECT * FROM playbook_grants WHERE grant_id=$1 AND namespace_id=$2 AND generation=$3 FOR UPDATE").bind(id).bind(self.namespace.namespace_id).bind(self.namespace.generation).fetch_optional(&mut **tx).await?.ok_or(Error::Forbidden)?;
        if row.get::<i64, _>("revision") != revision
            || row.get::<Uuid, _>("owner_user_id") != self.namespace.owner_user_id
        {
            return Err(Error::Forbidden);
        }
        if check.owner {
            Self::owner(tx, self.namespace.owner_user_id).await?;
        }
        if check.live {
            if row.get::<Option<i64>, _>("revoked_at_ms").is_some() {
                return Err(Error::Forbidden);
            }
            if row.get::<i64, _>("expires_at_ms") <= self.now()? {
                return Err(Error::Forbidden);
            }
        }
        let scope: GrantScope = serde_json::from_value(row.get("scope"))?;
        self.grant_authority(id, revision, &scope)?;
        Ok(scope)
    }
    pub(in crate::playbooks) fn intent_authority(
        &self,
        intent: &Intent,
        grant: GrantRevision,
        scope: &GrantScope,
    ) -> Result<()> {
        if intent.authority != self.grant_authority(grant.id, grant.revision, scope)? {
            return Err(Error::Forbidden);
        }
        let contract = &intent.command;
        if !self.known_contract(contract) || !contract.capabilities.is_subset(&scope.capabilities) {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub(in crate::playbooks) async fn authorize_contract(
        &self,
        owner: &RuntimeOwner,
        request: ContractAuthorization<'_>,
    ) -> Result<()> {
        let ContractAuthorization {
            grant,
            contract,
            authority,
            live,
        } = request;
        let GrantRevision { id, revision } = grant;
        if !self.known_contract(contract) {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self
            .grant_locked(&mut tx, GrantRevision::new(id, revision), live)
            .await?;
        if self.grant_authority(id, revision, &scope)? != *authority
            || !contract.capabilities.is_subset(&scope.capabilities)
        {
            return Err(Error::Forbidden);
        }
        if live {
            self.check_expiry(&mut tx, id).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub(in crate::playbooks) async fn active_authority(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
    ) -> Result<aq_playbook_host::runner::Authority> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self
            .grant_locked(&mut tx, GrantRevision::new(id, revision), true)
            .await?;
        self.check_expiry(&mut tx, id).await?;
        let authority = self.grant_authority(id, revision, &scope)?;
        tx.commit().await?;
        Ok(authority)
    }
    pub(in crate::playbooks) fn identity(&self, intent: &Intent) -> Result<String> {
        digest(&(
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
            &intent.run,
            &intent.step,
            intent.invocation,
            intent.deadline_at_ms,
            &intent.command,
            &intent.input_digest,
            &intent.authority,
        ))
    }
    pub(in crate::playbooks) async fn check_expiry(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
    ) -> Result<()> {
        let expiry: i64 =
            sqlx::query_scalar("SELECT expires_at_ms FROM playbook_grants WHERE grant_id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if expiry <= self.now()? {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub async fn authorize(&self, owner: &RuntimeOwner, id: Uuid, revision: i64) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        self.grant_locked(&mut tx, GrantRevision::new(id, revision), true)
            .await?;
        self.check_expiry(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }
}
