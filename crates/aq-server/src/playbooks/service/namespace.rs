use super::*;
use aq_playbook_domain::namespace_store::NamespaceRecord;

impl Service {
    pub(in crate::playbooks) fn now(&self) -> Result<i64> {
        let at = (self.clock)().timestamp_millis();
        if at < 0 {
            return Err(Error::Invalid);
        }
        Ok(at)
    }
    pub(in crate::playbooks) async fn tx(&self) -> Result<Transaction<'_, Postgres>> {
        aq_playbook_domain::bridge_tx(&self.pool).await
    }
    pub(in crate::playbooks) async fn owner(
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
    ) -> Result<()> {
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
        if role.as_deref() != Some("owner") {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub(in crate::playbooks) async fn namespace_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<sqlx::postgres::PgRow> {
        NamespaceRecord::new(&self.namespace, self.profile, SOURCE)
            .lock(tx)
            .await
    }
    /// Rolling deletion (architecture 0006): one bridge table's finished rows
    /// beyond the newest `keep_finished`, at most `delete_batch` of them.
    pub async fn prune(
        &self,
        owner: &RuntimeOwner,
        retention: &aq_playbook_host::retention::Retention,
        table: usize,
    ) -> Result<u64> {
        let (keep, batch) = retention.bounds().ok_or(Error::Invalid)?;
        let tables = aq_playbook_host::retention::TABLES;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let (namespace, generation) = (self.namespace.namespace_id, self.namespace.generation);
        let deleted = sqlx::query(tables[table % tables.len()].1)
            .bind(namespace)
            .bind(generation)
            .bind(keep)
            .bind(batch)
            .bind(self.now()?)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(deleted)
    }
    pub async fn initialize(&self, actor: Uuid) -> Result<()> {
        if actor != self.namespace.owner_user_id {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        aq_playbook_domain::host_operations::Operations(&self.namespace)
            .require_project_workspace(&mut *tx)
            .await?;
        NamespaceRecord::new(&self.namespace, self.profile, SOURCE)
            .initialize(&mut tx, actor)
            .await?;
        self.namespace_locked(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn runtime_revision(&self) -> Result<i64> {
        let mut tx = self.tx().await?;
        let row = self.namespace_locked(&mut tx).await?;
        let r = row.get("runtime_revision");
        tx.commit().await?;
        Ok(r)
    }
    /// Expected revision is captured before engine ownership acquisition. A delayed
    /// initializer may only replay this exact incarnation/revision, never refresh it.
    pub async fn adopt_runtime(&self, incarnation: Uuid, expected: i64) -> Result<RuntimeOwner> {
        if incarnation.is_nil() || !(0..i64::MAX - 1).contains(&expected) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let row = self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let revision: i64 = row.get("runtime_revision");
        let old: Option<Uuid> = row.get("runtime_incarnation");
        let replay = revision == expected + 1 && old == Some(incarnation);
        if revision != expected && !replay {
            return Err(Error::Fenced);
        }
        if revision == expected {
            sqlx::query("UPDATE playbook_namespaces SET runtime_revision=$2,runtime_incarnation=$3 WHERE namespace_id=$1").bind(self.namespace.namespace_id).bind(expected+1).bind(incarnation).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(RuntimeOwner {
            namespace_id: self.namespace.namespace_id,
            generation: self.namespace.generation,
            incarnation,
            revision: expected + 1,
        })
    }
    pub(in crate::playbooks) fn runtime(
        &self,
        row: &sqlx::postgres::PgRow,
        owner: &RuntimeOwner,
    ) -> Result<()> {
        let namespace_matches = (owner.namespace_id, owner.generation)
            == (self.namespace.namespace_id, self.namespace.generation);
        if !namespace_matches {
            return Err(Error::Fenced);
        }
        let runtime = (
            row.get::<Option<Uuid>, _>("runtime_incarnation"),
            row.get::<i64, _>("runtime_revision"),
        );
        if runtime != (Some(owner.incarnation), owner.revision) {
            return Err(Error::Fenced);
        }
        Ok(())
    }
    pub(in crate::playbooks) fn aad(
        &self,
        domain: &str,
        id: Uuid,
        digest: &str,
    ) -> Result<Vec<u8>> {
        canonical_bytes(&(
            domain,
            1,
            self.namespace.namespace_id,
            self.namespace.generation,
            id,
            digest,
        ))
        .map_err(|_| Error::Invalid)
    }
    pub async fn retire_runtime(&self, owner: &RuntimeOwner) -> Result<()> {
        self.retire(owner, true).await
    }
    pub(crate) async fn retire_for_shutdown(&self, owner: &RuntimeOwner) -> Result<()> {
        self.retire(owner, false).await
    }
    async fn retire(&self, owner: &RuntimeOwner, current_user: bool) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        let next = owner.revision.checked_add(1).ok_or(Error::Overloaded)?;
        if ns.get::<Option<Uuid>, _>("runtime_incarnation").is_none()
            && ns.get::<i64, _>("runtime_revision") == next
        {
            tx.commit().await?;
            return Ok(());
        }
        self.runtime(&ns, owner)?;
        if current_user {
            Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        }
        sqlx::query("UPDATE playbook_namespaces SET runtime_revision=$2,runtime_incarnation=NULL WHERE namespace_id=$1").bind(self.namespace.namespace_id).bind(next).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub(in crate::playbooks) fn current_time(&self) -> i64 {
        (self.clock)().timestamp_millis()
    }
}
