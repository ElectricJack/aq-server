use super::*;
use aq_playbook_domain::check_admission::{BoundSource, RequestedCheck};
use aq_playbook_domain::check_cancellations::{CancelStage, Cancellations};

impl Service {
    pub(in crate::playbooks::host_adapters) async fn check_request(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let ports = self.ports()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        let operation = self.operation(intent)?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                // Lost response: observe the original attempt, never restart.
                // An observation failure leaves the retained handle intact.
                if result.outcome == "accepted" {
                    let _ = self.observe_check(operation).await;
                }
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Conflict),
            Stage::Fresh => {}
        }
        let cancellations = Cancellations::new(&self.namespace, &intent.id, &identity);
        cancellations.require_open(&mut tx).await?;
        let check = RequestedCheck::new(operation, &self.namespace, &intent.input)?;
        let source = self.bound_check_source(&mut tx, &check).await?;
        let source = match check.admit(ports.checks.as_deref(), source).await? {
            Ok(source) => source,
            Err(result) => {
                return self
                    .settle_fresh(tx, command.retained(&identity), result)
                    .await;
            }
        };
        let checks = ports.checks.as_ref().ok_or(Error::Unknown)?;
        let request = check.request(&source, intent.deadline_at_ms);
        let accepted = self
            .record_operation(
                tx,
                command.retained(&identity),
                OperationRequest::new(Some(check.id), &request),
            )
            .await?;
        let attempt = check.attempt(&source)?;
        // An ambiguous start leaves the operation running; reconciliation
        // observes it and never issues a second start.
        // The handle is durable from here: an observation failure leaves the
        // operation running for operation.get.
        if checks.start(&attempt).await.is_ok() {
            let _ = self.observe_check(operation).await;
        }
        Ok(accepted)
    }

    async fn bound_check_source(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        request: &RequestedCheck<'_>,
    ) -> Result<BoundSource> {
        let (check_id, input, subject) = (request.id, request.input, request.subject);
        // Candidate subjects run on the prepared candidate's OID, never the
        // caller's input.source (see `check_source`). Host7 (2.0.0) has no
        // prepared source output, so it substitutes the OID; Host8 (2.1.0)
        // policies bind `candidate.source`, so a different input.source is a
        // consistency violation and is refused `invalid_request`.
        Ok(if check_id.is_empty() || !source_valid(&input["source"]) {
            BoundSource::Invalid
        } else {
            match self.check_source(tx, text(subject, "id")?).await? {
                CheckSource::Input => BoundSource::Prepared(input["source"].clone()),
                CheckSource::Candidate(oid)
                    if self.profile.has_prepared_source() && oid != input["source"] =>
                {
                    BoundSource::Invalid
                }
                CheckSource::Candidate(oid) => BoundSource::Prepared(oid),
                CheckSource::NotPrepared => BoundSource::Stale,
            }
        })
    }
    /// Observes a running check once and completes it when the executor has
    /// stopped it. Returns the completion if there is one.
    pub(super) async fn observe_check(&self, operation: Uuid) -> Result<Option<HostResult>> {
        let Some(op) = self.load_operation(&self.pool, operation).await? else {
            return Ok(None);
        };
        if op.status != "running" {
            return op.retained().map(Some);
        }
        let checks = self.ports()?.checks.as_ref().ok_or(Error::Unknown)?;
        let observed = checks.observe(operation).await?;
        let Some(result) = check_completion(operation, &op.request, observed, self.now()?)? else {
            return Ok(None);
        };
        self.complete_operation(
            OperationKey::new(operation, &op.command, &op.intent_id),
            result,
            None,
        )
        .await
        .map(Some)
    }

    /// The cancel plan of the check operation `id` (judgments are not cancellable), read in
    /// the caller's tx: it holds the namespace lock, so a pool read here could deadlock.
    pub(in crate::playbooks::host_adapters) async fn check_plan(
        &self,
        c: &mut PgConnection,
        id: Uuid,
    ) -> Result<Option<Value>> {
        Ok(match self.load_operation(c, id).await? {
            Some(op) if op.command == "check.request" => {
                let s = &op.request["subject"];
                let at = subject(text(s, "id")?, revision_of(s)?);
                Some(json!({"check": id, "subject": at}))
            }
            _ => None,
        })
    }

    /// Asks the executor to stop a running check, then observes it once. The
    /// operation completes as cancelled only from an executor-reported
    /// `CheckStop::Cancelled`; a check that stopped otherwise is too_late and
    /// one still running is accepted. Idempotent: repeating it re-asks and
    /// re-observes, never completing twice.
    pub(in crate::playbooks::host_adapters) async fn cancel_check(
        &self,
        operation: Uuid,
    ) -> Result<&'static str> {
        let Some(op) = self.load_operation(&self.pool, operation).await? else {
            return Err(Error::Unknown);
        };
        if op.command != "check.request" {
            return Err(Error::Invalid);
        }
        if op.status == "running" {
            let checks = self.ports()?.checks.as_ref().ok_or(Error::Unknown)?;
            checks.cancel(operation).await.map_err(|_| Error::Unknown)?;
        }
        Ok(match self.observe_check(operation).await? {
            None => "accepted",
            Some(done) if done.output["disposition"] == "cancelled" => "cancelled",
            Some(_) => "too_late",
        })
    }

    /// Runner cancellation of a check.request intent (catalog 2.0.0 declares
    /// `cancellation: true`). Before admission the cancellation is recorded
    /// and confirmed, so no attempt is ever started; an admitted attempt is
    /// confirmed only once the executor reports it cancelled.
    pub async fn check_cancel(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<CancellationResult> {
        if !self.profile.is_host2() || intent.command.name != "check.request" {
            return Err(Error::Invalid);
        }
        let identity = self.identity(intent)?;
        let key = aq_playbook_host::runner::cancellation_key(intent).map_err(|_| Error::Invalid)?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self
            .grant_locked(&mut tx, GrantRevision::new(id, revision), false)
            .await?;
        self.intent_authority(intent, GrantRevision::new(id, revision), &scope)?;
        let cancellations = Cancellations::new(&self.namespace, &intent.id, &identity);
        match cancellations.stage(&mut tx, &key, || self.now()).await? {
            CancelStage::Retained(disposition) => return Ok(disposition),
            CancelStage::Settled(disposition) => {
                tx.commit().await?;
                return Ok(disposition);
            }
            CancelStage::Admitted => tx.commit().await?,
        }
        Ok(match self.cancel_check(self.operation(intent)?).await? {
            "cancelled" => CancellationResult::Confirmed,
            "accepted" => CancellationResult::Accepted,
            _ => CancellationResult::TooLate,
        })
    }
}
