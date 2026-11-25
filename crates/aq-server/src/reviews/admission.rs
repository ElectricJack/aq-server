use super::*;

impl Service {
    /// Internal admission preparation. Caller separately checks project scope;
    /// eventual creation derives/currently checks owner identity from auth.
    pub async fn prepare(&self, report_id: Uuid, source_operation: Uuid) -> Result<PreparedReview> {
        let mut tx = self.tx().await?;
        let result =
            crate::profiled_execution::results::load_locked(&self.state, &mut tx, report_id)
                .await?;
        if !result.is_current_report_head {
            return Err(conflict());
        }
        let source =
            crate::source_retention::load_confirmed_locked(&mut tx, source_operation, &result)
                .await?;
        let subject = subject(&result, &source)?;
        tx.commit().await.map_err(db)?;
        let retention =
            crate::source_retention::result_retention::inspect_current(&self.state, &source)
                .await?;
        Ok(PreparedReview {
            report_id,
            source_operation,
            subject,
            retention,
        })
    }
    /// Explicit requester is supplied only by authenticated internal adapter or
    /// RequireOwner. Stable requester/operation is immutable and retained.
    pub async fn admit(
        &self,
        actor: Uuid,
        prepared: &PreparedReview,
        request: &DecisionRequest,
    ) -> Result<()> {
        let mut tx = self.tx().await?;
        self.admit_in_tx(&mut tx, actor, prepared.proposal(request))
            .await?;
        tx.commit().await.map_err(db)
    }
    /// Adapter takes this before bridge namespace/grant locks. Reacquiring on
    /// the same transaction is harmless; no permit exists outside that SQL tx.
    pub(crate) async fn lock_budget(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        Self::budget(tx).await?;
        Ok(())
    }
    pub(crate) async fn admit_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: Uuid,
        proposal: Proposal<'_>,
    ) -> Result<()> {
        self.admit_subject_in_tx(tx, ReviewActor::new(actor, None), proposal)
            .await
    }
    pub(super) async fn admit_subject_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        owner: ReviewActor<'_>,
        proposal: Proposal<'_>,
    ) -> Result<()> {
        Admission {
            service: self,
            owner,
            proposal,
        }
        .apply(tx)
        .await
    }
}

pub(crate) struct Proposal<'a> {
    prepared: &'a PreparedReview,
    request: &'a DecisionRequest,
    pub(super) combined: Option<&'a combined::CombinedSubject>,
}
impl PreparedReview {
    pub(crate) fn proposal<'a>(&'a self, request: &'a DecisionRequest) -> Proposal<'a> {
        Proposal {
            prepared: self,
            request,
            combined: None,
        }
    }
}

struct Admission<'a> {
    service: &'a Service,
    owner: ReviewActor<'a>,
    proposal: Proposal<'a>,
}
struct CurrentSubject {
    result: crate::profiled_execution::results::RecordedResult,
    source: crate::source_retention::RecordedSourceConfirmation,
    exact: OwnedSubject,
}
struct AdmissionRecord<'a> {
    exact: &'a OwnedSubject,
    existing: Option<sqlx::postgres::PgRow>,
    request_digest: String,
    subject_digest: String,
    subject_body: Vec<u8>,
    body: Vec<u8>,
}

impl Admission<'_> {
    async fn apply(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        let Proposal {
            prepared,
            request,
            combined,
        } = self.proposal;
        request.validate().map_err(|_| invalid())?;
        let wanted = match combined {
            Some(s) => OwnedSubject::SourceAndEffects(Box::new(s.clone())),
            None => OwnedSubject::Code(Box::new(prepared.subject.clone())),
        };
        if request.subject != wanted.artifact()? {
            return Err(conflict());
        }
        let digest = hash(request)?;
        let budget = Service::budget(tx).await?;
        let current = self.lock_current(tx).await?;
        let subject_digest = current
            .exact
            .artifact()
            .map_err(|_| invalid())?
            .digest
            .as_str()
            .to_owned();
        let existing =
            sqlx::query("SELECT * FROM review_subjects WHERE subject_digest=$1 FOR UPDATE")
                .bind(&subject_digest)
                .fetch_optional(&mut **tx)
                .await
                .map_err(db)?;
        if self.replay(tx, &digest).await? {
            return Ok(());
        }
        self.authorize(tx, &current).await?;
        let body = encode(request)?;
        let subject_body = encode(&current.exact)?;
        let storage = AdmissionRecord {
            request_digest: digest,
            exact: &current.exact,
            existing,
            subject_digest,
            subject_body,
            body,
        };
        let used = self.lineage_usage(tx, &storage).await?;
        self.check_combined_quota(tx, storage.exact).await?;
        let charge = storage.charge(used, &budget)?;
        self.persist(tx, &storage, charge).await
    }

    async fn lock_current(&self, tx: &mut Transaction<'_, Postgres>) -> Result<CurrentSubject> {
        let Proposal {
            prepared, combined, ..
        } = self.proposal;
        // Global budget lock serializes new admissions and duplicate IDs before
        // common authority locks. No decision row is locked before an attempt.
        let result = crate::profiled_execution::results::load_locked(
            &self.service.state,
            tx,
            prepared.report_id,
        )
        .await?;
        let source =
            crate::source_retention::load_confirmed_locked(tx, prepared.source_operation, &result)
                .await?;
        let code = subject(&result, &source)?;
        if code != prepared.subject {
            return Err(conflict());
        }
        let exact = if let Some(combined) = combined {
            combined.validate()?;
            let ledger = crate::profiled_execution::settlement::effects::inspect_aq_effects_locked(
                &self.service.state,
                tx,
                &result,
            )
            .await?;
            if combined.code != code || !combined.matches_ledger(&result, &ledger) {
                return Err(conflict());
            }
            OwnedSubject::SourceAndEffects(Box::new(combined.clone()))
        } else {
            OwnedSubject::Code(Box::new(code))
        };
        Ok(CurrentSubject {
            result,
            source,
            exact,
        })
    }
    async fn replay(&self, tx: &mut Transaction<'_, Postgres>, digest: &str) -> Result<bool> {
        let actor = self.owner.id;
        let request = self.proposal.request;
        let old=sqlx::query("SELECT * FROM human_decision_requests WHERE (requester_id=$1 AND operation_id=$2) OR decision_id=$3 FOR UPDATE").bind(actor).bind(request.operation_id).bind(request.decision_id).fetch_all(&mut **tx).await.map_err(db)?;
        match old.as_slice() {
            [] => Ok(false),
            [row] => {
                if row.get::<Uuid, _>("requester_id") != actor
                    || row.get::<String, _>("request_digest") != digest
                {
                    return Err(conflict());
                }
                if self.service.decode::<DecisionRequest>(
                    "request",
                    &request.decision_id.to_string(),
                    row,
                )? != *request
                {
                    return Err(conflict());
                }
                self.owner.authorize(tx).await?;
                Ok(true)
            }
            _ => Err(conflict()),
        }
    }
    async fn authorize(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        current: &CurrentSubject,
    ) -> Result<()> {
        let request = self.proposal.request;
        if !current.result.is_current_report_head {
            return Err(conflict());
        }
        self.owner.authorize(tx).await?;
        // Stable sorted order avoids inversions when eligible sets overlap.
        let mut owners = request.eligible_owners.clone();
        owners.sort();
        for owner in owners {
            Service::owner(tx, owner).await?;
        }
        self.service
            .project(tx, &current.exact, request.eligibility_project_revision)
            .await?;
        self.proposal
            .prepared
            .retention
            .validate_current(&self.service.state, &current.source)?;
        if request.expires_at_ms <= now(&self.service.state)?
            || request.review_policy != current.exact.validation_policy
        {
            return Err(conflict());
        }
        Ok(())
    }
    async fn lineage_usage(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        storage: &AdmissionRecord<'_>,
    ) -> Result<i64> {
        let exact = storage.exact;
        sqlx::query("INSERT INTO review_lineage_budget VALUES($1,$2,$3,0) ON CONFLICT DO NOTHING")
            .bind(exact.project_id)
            .bind(exact.task.workspace_id)
            .bind(exact.task.task_id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        let used:i64=sqlx::query_scalar("SELECT retained_operations FROM review_lineage_budget WHERE project_id=$1 AND workspace_id=$2 AND task_id=$3 FOR UPDATE").bind(exact.project_id).bind(exact.task.workspace_id).bind(exact.task.task_id).fetch_one(&mut **tx).await.map_err(db)?;
        if let Some(row) = &storage.existing {
            let stored = self.service.decode_subject(&storage.subject_digest, row)?;
            if encode(&stored)? != encode(exact)? || row.get::<i64, _>("decision_count") >= 16 {
                return Err(conflict());
            }
        }
        Ok(used)
    }
    async fn check_combined_quota(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        exact: &OwnedSubject,
    ) -> Result<()> {
        if exact.profile() == SubjectProfile::SourceAndEffects {
            let questions: i64 = sqlx::query_scalar("SELECT count(*) FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) JOIN profiled_result_reports r ON r.report_id=s.report_id WHERE r.attempt_id=$1 AND s.subject_profile='source_and_effects_review_v1'")
                .bind(exact.binding.attempt_id).fetch_one(&mut **tx).await.map_err(db)?;
            if questions >= 16 {
                return Err(AppError::PayloadTooLarge(
                    "combined review attempt quota exhausted".into(),
                ));
            }
        }
        Ok(())
    }
    async fn persist(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        storage: &AdmissionRecord<'_>,
        charge: usize,
    ) -> Result<()> {
        let exact = storage.exact;
        let request = self.proposal.request;
        if exact.profile() == SubjectProfile::SourceAndEffects {
            sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
                .execute(&mut **tx)
                .await
                .map_err(db)?;
        }
        if storage.existing.is_none() {
            let aad_kind = if exact.profile() == SubjectProfile::Code {
                "subject"
            } else {
                "combined-subject"
            };
            let s = self
                .service
                .seal(aad_kind, &storage.subject_digest, &storage.subject_body)?;
            sqlx::query("INSERT INTO review_subjects(subject_digest,report_id,project_id,workspace_id,task_id,sealed_version,key_id,nonce,ciphertext,subject_profile) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)").bind(&storage.subject_digest).bind(self.proposal.prepared.report_id).bind(exact.project_id).bind(exact.task.workspace_id).bind(exact.task.task_id).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).bind(exact.profile().name()).execute(&mut **tx).await.map_err(db)?;
        }
        sqlx::query(
            "UPDATE review_subjects SET decision_count=decision_count+1 WHERE subject_digest=$1",
        )
        .bind(&storage.subject_digest)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        let s = self
            .service
            .seal("request", &request.decision_id.to_string(), &storage.body)?;
        sqlx::query("INSERT INTO human_decision_requests(decision_id,requester_id,operation_id,request_digest,subject_digest,decision_revision,state,expires_at_ms,sealed_version,key_id,nonce,ciphertext) VALUES($1,$2,$3,$4,$5,1,'pending',$6,$7,$8,$9,$10)").bind(request.decision_id).bind(self.owner.id).bind(request.operation_id).bind(&storage.request_digest).bind(&storage.subject_digest).bind(request.expires_at_ms as i64).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).execute(&mut **tx).await.map_err(db)?;
        sqlx::query(
            "INSERT INTO human_decision_submissions(decision_id,submitted_at_ms) VALUES($1,$2)",
        )
        .bind(request.decision_id)
        .bind(now(&self.service.state)? as i64)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        sqlx::query("UPDATE review_lineage_budget SET retained_operations=retained_operations+1 WHERE project_id=$1 AND workspace_id=$2 AND task_id=$3").bind(exact.project_id).bind(exact.task.workspace_id).bind(exact.task.task_id).execute(&mut **tx).await.map_err(db)?;
        sqlx::query("UPDATE review_owner_budget SET retained_operations=retained_operations+1,retained_bytes=retained_bytes+$1 WHERE singleton").bind(charge as i64).execute(&mut **tx).await.map_err(db)?;
        Ok(())
    }
}
impl AdmissionRecord<'_> {
    fn charge(&self, used: i64, budget: &sqlx::postgres::PgRow) -> Result<usize> {
        // Reserve final receipt+outbox envelope capacity at initial admission;
        // finality never needs to exceed the retained global payload budget.
        let charge = self.body.len()
            + if self.existing.is_none() {
                self.subject_body.len()
            } else {
                0
            }
            + 2 * MAX_BYTES
            + 256;
        let too_many = used >= 256 || budget.get::<i64, _>("retained_operations") >= 10000;
        let exceeds_bytes = budget
            .get::<i64, _>("retained_bytes")
            .checked_add(charge as i64)
            .is_none_or(|n| n > 67108864);
        if too_many || exceeds_bytes {
            return Err(AppError::PayloadTooLarge(
                "review retained budget exhausted".into(),
            ));
        }
        Ok(charge)
    }
}
