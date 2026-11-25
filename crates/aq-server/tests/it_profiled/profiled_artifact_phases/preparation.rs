use super::*;
type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;
pub(super) struct Preparation {
    attempt: Uuid,
    owner: LaunchOwner,
    operation: Uuid,
    lease: MaterialLease,
    document: TransportDocument,
}
impl Preparation {
    pub(super) fn new(f: &Fixture, attempt: Uuid) -> Self {
        let owner = LaunchOwner {
            guardian_id: Uuid::new_v4(),
            launch_id: Uuid::new_v4(),
            reservation_id: Uuid::new_v4(),
        };
        let operation = Uuid::new_v4();
        let lease = MaterialLease {
            schema_version: 1,
            lease_id: Uuid::new_v4(),
            binding: f.profiled.offer.binding.clone(),
            credential_profile: f.selection.choice.credential_profile.clone(),
            credential_revision: f.selection.credential_revision,
            provider: Label::new(&f.selection.choice.provider).unwrap(),
            account_reference: Label::new(&f.selection.choice.account_reference).unwrap(),
            endpoint_reference: Label::new(&f.selection.choice.endpoint_reference).unwrap(),
            purpose: Label::new("model-authentication").unwrap(),
            material_digest: Digest::of(b"synthetic-only-key"),
            material_bytes: 18,
            issued_at_ms: NOW,
            expires_at_ms: PLAN_EXPIRY,
        };
        let plan = PreparationPlan {
            schema_version: 1,
            operation_id: operation,
            binding: lease.binding.clone(),
            reservation: NoLaunchReservation {
                reservation_id: owner.reservation_id,
                reservation_operation_id: Uuid::new_v4(),
                reservation_journal_digest: Digest::of(b"synthetic reserved journal"),
            },
            guardian_id: owner.guardian_id,
            launch_id: owner.launch_id,
            clock: DeliveryClock::new(NOW, PLAN_EXPIRY).unwrap(),
            materials: vec![lease.clone()],
        };
        let document = TransportDocument::PreparationPlan(plan);
        document.validate().unwrap();
        Self {
            attempt,
            owner,
            operation,
            lease,
            document,
        }
    }
    pub(super) async fn accept(&self, tx: &mut Tx<'_>, f: &Fixture, granted: bool) {
        let attempt = self.attempt;
        let owner = &self.owner;
        let operation = self.operation;
        let document = &self.document;
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(f.node.node_id)
            .execute(&mut **tx)
            .await
            .unwrap();
        sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
            .bind(attempt)
            .execute(&mut **tx)
            .await
            .unwrap();
        let mut hot = HotState::from_compat(&f.profiled).unwrap();
        sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(&mut **tx).await.unwrap();
        hot.status = Status::Preparing;
        hot.owner = Some(self.owner.clone());
        hot.preparation_operation_id = Some(operation);
        sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'accept',$3,$4)").bind(f.node.node_id).bind(operation).bind(Digest::of(b"synthetic phase accept").as_str()).bind(json!(document)).execute(&mut **tx).await.unwrap();
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(hot))
            .execute(&mut **tx)
            .await
            .unwrap();
        journal::record_reserved(
            tx,
            &f.profiled.offer.binding,
            owner,
            1,
            &Digest::of(b"synthetic reserved journal"),
        )
        .await
        .unwrap();
        self.admit_material(tx, f).await;
        if granted {
            self.grant(tx, f, hot).await;
        }
    }
    async fn admit_material(&self, tx: &mut Tx<'_>, f: &Fixture) {
        let lease = &self.lease;
        let attempt = self.attempt;
        let identity = format!(
            "{}@{}",
            lease.credential_profile.id.as_str(),
            lease.credential_profile.version.as_str()
        );
        sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$6,$7,$8,$9,$10,$11,$12,'admitted',$13,$14)").bind(lease.lease_id).bind(Uuid::new_v4()).bind(Digest::of(b"synthetic lease issuance").as_str()).bind(f.node.node_id).bind(f.node.boot_id).bind(attempt).bind(lease.binding.offer_id).bind(identity).bind(lease.credential_revision as i64).bind(lease.purpose.as_str()).bind(json!(lease)).bind(Digest::of(b"synthetic private verifier").as_str()).bind(NOW as i64).bind(PLAN_EXPIRY as i64).execute(&mut **tx).await.unwrap();
        sqlx::query("INSERT INTO profiled_launch_materials(attempt_id,lease_id) VALUES($1,$2)")
            .bind(attempt)
            .bind(lease.lease_id)
            .execute(&mut **tx)
            .await
            .unwrap();
    }
    async fn grant(&self, tx: &mut Tx<'_>, f: &Fixture, mut hot: HotState) {
        let attempt = self.attempt;
        let owner = &self.owner;
        hot.status = Status::Granted;
        hot.execution_deadline_ms = Some(NOW + 20_000);
        hot.grant = Some(aq_session_protocol::ProfiledGrant {
            binding: f.profiled.offer.binding.clone(),
            grant_id: Uuid::new_v4(),
            expires_at_ms: GRANT_EXPIRY,
        });
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(hot))
            .execute(&mut **tx)
            .await
            .unwrap();
        journal::record_prepared(
            tx,
            &f.profiled.offer.binding,
            owner,
            hot.grant.as_ref().unwrap().grant_id,
            2,
            &Digest::of(b"synthetic prepared journal"),
        )
        .await
        .unwrap();
    }
}
