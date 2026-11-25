use super::*;
use aq_playbook_domain::manual_admission::{EncodedAdmission, replay_admission, validate_receipt};

impl Service {
    pub async fn prepare(&self, actor: Uuid, request: PrepareRequest) -> Result<AdmissionStatus> {
        #[cfg(feature = "delivery_playbook")]
        if self.profile.is_delivery()
            && request.binding == crate::playbooks::domain::host10::DELIVERY_BINDING
        {
            return Err(Error::Forbidden);
        }
        if actor != self.namespace.owner_user_id {
            return Err(Error::Invalid);
        }
        request.validate()?;
        let key = stable_id(
            "aq.playbook.manual.admission",
            self.namespace.namespace_id,
            self.namespace.generation,
            &request.request_id.to_string(),
        )?
        .to_string();
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let scope = self
            .grant_locked(
                &mut tx,
                GrantRevision::new(request.grant_id, request.grant_revision),
                false,
            )
            .await?;
        let binding = request.binding.clone();
        let engine_request = aq_playbook_host::runner::KeyedAdmissionRequest {
            key: key.clone(),
            binding: request.binding,
            expected_activation_revision: request.expected_activation_revision,
            entry: request.entry,
            input: request.input,
            authority: self.grant_authority(request.grant_id, request.grant_revision, &scope)?,
        };
        let EncodedAdmission { bytes, digest } = EncodedAdmission::new(&engine_request)?;
        let retained=sqlx::query("SELECT request_digest,status,receipt FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND request_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(request.request_id).fetch_optional(&mut *tx).await?;
        if let Some(row) = retained {
            let status = replay_admission(&row, request.request_id, key, &digest)?;
            tx.commit().await?;
            return Ok(status);
        }
        self.grant_locked(
            &mut tx,
            GrantRevision::new(request.grant_id, request.grant_revision),
            true,
        )
        .await?;
        let sealed = self
            .keys
            .seal(
                &self.aad("aq.playbooks.admission", request.request_id, &digest)?,
                &bytes,
            )
            .map_err(|_| Error::Unknown)?;
        sqlx::query("INSERT INTO playbook_admissions(namespace_id,generation,request_id,request_digest,engine_key,grant_id,grant_revision,expected_activation_revision,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,binding) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,'pending',$14)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(request.request_id).bind(digest).bind(&key).bind(request.grant_id).bind(request.grant_revision).bind(request.expected_activation_revision as i64).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(self.now()?).bind(&binding).execute(&mut *tx).await?;
        self.check_expiry(&mut tx, request.grant_id).await?;
        tx.commit().await?;
        Ok(AdmissionStatus {
            request_id: request.request_id,
            engine_key: key,
            status: "pending".into(),
            receipt: None,
        })
    }
    pub async fn admission(&self, actor: Uuid, id: Uuid) -> Result<RetainedAdmission> {
        if actor != self.namespace.owner_user_id || id.is_nil() {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, actor).await?;
        let row=sqlx::query("SELECT * FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND request_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::Forbidden)?;
        let digest: String = row.get("request_digest");
        let sealed = Sealed {
            version: row.get::<i32, _>("sealed_version") as u32,
            key_id: row.get("key_id"),
            nonce: row.get("nonce"),
            ciphertext: row.get("ciphertext"),
        };
        let bytes = self
            .keys
            .open(&self.aad("aq.playbooks.admission", id, &digest)?, &sealed)
            .map_err(|_| Error::Unknown)?;
        let request: aq_playbook_host::runner::KeyedAdmissionRequest =
            serde_json::from_slice(&bytes)?;
        let key: String = row.get("engine_key");
        let grant_id: Uuid = row.get("grant_id");
        let revision: i64 = row.get("grant_revision");
        let scope = self
            .grant_locked(&mut tx, GrantRevision::new(grant_id, revision), false)
            .await?;
        let digest_matches = crate::playbooks::domain::digest(&request)? == digest;
        if !digest_matches || request.key != key {
            return Err(Error::Conflict);
        }
        if request.authority != self.grant_authority(grant_id, revision, &scope)? {
            return Err(Error::Conflict);
        }
        let receipt: Option<Value> = row.get("receipt");
        let status = AdmissionStatus {
            request_id: id,
            engine_key: key,
            status: row.get("status"),
            receipt: receipt.map(serde_json::from_value).transpose()?,
        };
        tx.commit().await?;
        Ok(RetainedAdmission {
            status,
            request,
            grant_id,
            grant_revision: revision,
        })
    }
    pub async fn acknowledge_admission(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        receipt: aq_playbook_host::runner::KeyedAdmissionReceipt,
    ) -> Result<AdmissionStatus> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let row=sqlx::query("SELECT request_digest,engine_key,expected_activation_revision,receipt,status FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND request_id=$3 FOR UPDATE").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::Forbidden)?;
        validate_receipt(&row, &receipt)?;
        if let Some(old) = row.get::<Option<Value>, _>("receipt")
            && old != json!(receipt)
        {
            return Err(Error::Conflict);
        }
        sqlx::query("UPDATE playbook_admissions SET status='engine_accepted',held_reason=NULL,receipt=$4 WHERE namespace_id=$1 AND generation=$2 AND request_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).bind(json!(receipt)).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(AdmissionStatus {
            request_id: id,
            engine_key: receipt.key.clone(),
            status: "engine_accepted".into(),
            receipt: Some(receipt),
        })
    }
    pub async fn pending_admissions(&self, owner: &RuntimeOwner, limit: u32) -> Result<Vec<Uuid>> {
        if !(1..=16).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let rows=sqlx::query_scalar("SELECT request_id FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND status='pending' ORDER BY held_reason IS NOT NULL,created_at_ms,request_id LIMIT $3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(limit as i64).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows)
    }
}
