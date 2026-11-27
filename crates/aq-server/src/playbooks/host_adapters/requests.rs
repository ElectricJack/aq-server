//! D6 asynchronous host adapters for catalog profile 6 (AQ host catalog
//! 2.0.0): judgment.request, check.request, work.ensure completion and their
//! operation.get reconciliation.
//!
//! Both commands record one durable intent (`playbook_effects` row `accepted`
//! plus one `playbook_host_operations` row `running`) before any provider
//! request or sandbox start, return the operation handle, and complete exactly
//! once by a sealed trusted event (aq.judgment.completed / aq.check.receipted)
//! that commits with the retained completion. A re-dispatch or operation.get
//! only observes the original operation; nothing here ever starts a second
//! check writer or a second provider request for one intent.
//!
//! The sandbox executor and the judgment provider are injected ports. The
//! host never fabricates a receipt: every passed/failed disposition comes from
//! an executor-reported exit, and an absent port is a typed refusal.
use super::*;
use aq_playbook_domain::host_operations::{DecisionRequest, DecisionView, Operation, Operations};
use aq_playbook_host::runner::CancellationResult;

pub use aq_playbook_domain::host_requests::*;

struct Completion<'a> {
    key: OperationKey<'a>,
    result: HostResult,
    note: Option<&'a str>,
}

impl Service {
    fn completion_contract(&self, command: &str) -> Result<(Value, &'static str)> {
        Ok(match command {
            "judgment.request" => (json!("judgment"), "aq.judgment.completed"),
            "judgment.assess" => (json!("judgment"), "aq.judgment.assessed"),
            "check.request" => (json!("check"), "aq.check.receipted"),
            "work.ensure" => (json!("work"), "aq.operation.updated"),
            "decision.request" => (json!("decision"), "aq.decision.completed"),
            name if aq_playbook_domain::review_domain::DELIVERY_COMMANDS.contains(&name) => {
                (json!("delivery"), "aq.delivery.completed")
            }
            #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
            "environment.build" | "environment.admit" => {
                (json!("environment"), "aq.environment.completed")
            }
            _ => return Err(Error::Invalid),
        })
    }

    /// Records the durable intent (accepted effect row and running operation
    /// row) and commits it before any external effect.
    pub(super) async fn record_operation(
        &self,
        mut tx: Transaction<'_, Postgres>,
        effect: EffectIdentity<'_>,
        admission: OperationRequest<'_>,
    ) -> Result<HostResult> {
        let id = effect.grant;
        let intent = effect.intent;
        let operation = self.operation(intent)?;
        let accepted = HostResult {
            outcome: "accepted".into(),
            output: json!({"handle": operation.to_string()}),
        };
        self.record(
            &mut tx,
            effect,
            EffectReceipt::new(&accepted.outcome, &accepted.output),
        )
        .await?;
        Operations(&self.namespace)
            .record(
                &mut tx,
                operation,
                &intent.id,
                &intent.command.name,
                &intent.input["subject"],
                admission.check,
                admission.body,
                self.now()?,
            )
            .await?;
        self.commit_fresh(tx, id, intent).await?;
        Ok(accepted)
    }

    pub(super) async fn load_operation(
        &self,
        db: impl PgExecutor<'_>,
        id: Uuid,
    ) -> Result<Option<Operation>> {
        Operations(&self.namespace).load(db, id).await
    }

    /// Completes a running operation exactly once: the retained completion and
    /// its sealed completion event commit together. A completion that lost the
    /// race returns the retained one.
    pub(super) async fn complete_operation(
        &self,
        key: OperationKey<'_>,
        result: HostResult,
        note: Option<&str>,
    ) -> Result<HostResult> {
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let result = self
            .retain_completion(&mut tx, Completion { key, result, note })
            .await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn retain_completion(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        completion: Completion<'_>,
    ) -> Result<HostResult> {
        let Completion { key, result, note } = completion;
        let OperationKey {
            operation,
            command,
            intent_id,
        } = key;
        let (_, contract) = self.completion_contract(command)?;
        let payload = json!({"outcome": result.outcome, "output": result.output});
        self.registry()
            .commands
            .get(command)
            .and_then(|c| c.asynchronous.as_ref())
            .ok_or(Error::Invalid)?
            .completion
            .validate(&payload)
            .map_err(|_| Error::Unknown)?;
        if !Operations(&self.namespace)
            .complete(tx, operation, &result, self.now()?, note)
            .await?
        {
            return self
                .load_operation(&mut **tx, operation)
                .await?
                .ok_or(Error::Unknown)?
                .retained();
        }
        let event_id = stable_id(
            &format!("aq.playbook.{command}.completion"),
            self.namespace.namespace_id,
            self.namespace.generation,
            intent_id,
        )?;
        self.seal_event(
            tx,
            HostEvent {
                contract,
                event_id,
                operation,
                causation: intent_id,
                subject: &result.output["subject"],
                payload,
            },
        )
        .await?;
        Ok(result)
    }
<<<<<<< HEAD

    // ------------------------------------------------------- work completion

    /// Tracks one catalog profile 7 ensured work node as a running operation,
    /// in the tx of its accepted effect row. It completes exactly once, by
    /// aq.operation.updated, when the graph task reaches a terminal state.
    pub(super) async fn track_work(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        work: &Value,
    ) -> Result<()> {
        let subject = &work["subject"];
        // `motivation` is the ensure input subject (for review work, the
        // reviewed candidate); D11 publication evidence binds to it.
        let request = json!({"kind": text(&intent.input, "kind")?,
            "reference": work["reference"], "subject": subject,
            "motivation": intent.input["subject"]});
        Operations(&self.namespace)
            .record(
                tx,
                self.operation(intent)?,
                &intent.id,
                "work.ensure",
                subject,
                None,
                &request,
                self.now()?,
            )
            .await
    }

    /// Reads the tracked work node and, once terminal, completes the
    /// operation (exactly once; a lost race returns the retained completion).
    /// `None` while the work is still open.
    pub(super) async fn observe_work(&self, operation: Uuid) -> Result<Option<HostResult>> {
        let Some(op) = self.load_operation(&self.pool, operation).await? else {
            return Ok(None);
        };
        if op.command != "work.ensure" {
            return Err(Error::Invalid);
        }
        if op.status != "running" {
            return op.retained().map(Some);
        }
        let task =
            Uuid::parse_str(text(&op.request["subject"], "id")?).map_err(|_| Error::Unknown)?;
        let node = self
            .ports()?
            .graph
            .node(self.namespace.workspace_id, task)
            .await?;
        let Some(result) = work_completion(&op.request, node.as_ref())? else {
            return Ok(None);
        };
        self.complete_operation(operation, &op.command, &op.intent_id, result, None)
            .await
            .map(Some)
    }

    /// Bounded round-robin over running catalog profile 7 work operations:
    /// each pass observes the least recently observed ones and completes those
    /// whose graph task is terminal. Returns the number completed. A graph
    /// read failure leaves that operation running for a later pass.
    pub async fn collect_work_completions(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !self.profile.is_host2() || self.host.is_none() {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let operations = Operations(&self.namespace)
            .observe_work(&mut tx, limit, self.now()?)
            .await?;
        tx.commit().await?;
        let mut completed = 0;
        for operation in operations {
            match self.observe_work(operation).await {
                Ok(Some(_)) => completed += 1,
                Ok(None) => {}
                Err(Error::Overloaded) => return Err(Error::Overloaded),
                Err(_) => {}
            }
        }
        Ok(completed)
    }

    // ------------------------------------------------------------- check.request

    pub(super) async fn check_request(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
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
        // A runner cancellation confirmed before admission admits no attempt.
        if let Some(cancel) = sqlx::query_scalar::<_, String>("SELECT request_digest FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .fetch_optional(&mut *tx)
            .await?
        {
            return Err(if cancel == identity {
                Error::Forbidden
            } else {
                Error::Conflict
            });
        }
        let input = &intent.input;
        let subject = &input["subject"];
        let check = &input["check"];
        let check_id = text(check, "id")?;
        let workspace = self.namespace.workspace_id;
        let project = self.namespace.project_id;
        // Candidate subjects run on the prepared candidate's OID, never the
        // caller's input.source (see `check_source`). Host7 (2.0.0) has no
        // prepared source output, so it substitutes the OID; Host8 (2.1.0)
        // policies bind `candidate.source`, so a different input.source is a
        // consistency violation and is refused `invalid_request`.
        let source = if check_id.is_empty() || !source_valid(&input["source"]) {
            None
        } else {
            match self.check_source(&mut tx, text(subject, "id")?).await? {
                CheckSource::Input => Some(Some(input["source"].clone())),
                CheckSource::Candidate(oid)
                    if self.profile.has_prepared_source() && oid != input["source"] =>
                {
                    None
                }
                CheckSource::Candidate(oid) => Some(Some(oid)),
                CheckSource::NotPrepared => Some(None),
            }
        };
        let echoed = match &source {
            Some(Some(bound)) => bound,
            _ => &input["source"],
        };
        let refusal = |disposition: Value, reason: &str| HostResult {
            outcome: "refused".into(),
            output: json!({"check": check, "disposition": disposition, "reason": reason,
                "reference": format!("check:{operation}"), "source": echoed, "subject": subject}),
        };
        let refused = if source.is_none() {
            Some(refusal(Value::Null, "invalid_request"))
        } else if source == Some(None) {
            Some(refusal(json!("stale"), "stale_subject"))
        } else if let Some(checks) = &ports.checks {
            let validator = pin(&check["validator"])?;
            match checks
                .pinned(workspace, project, text(subject, "id")?)
                .await?
            {
                None => Some(refusal(Value::Null, "not_required")),
                Some(p) if p.revision != revision_of(subject)? => {
                    Some(refusal(json!("stale"), "stale_subject"))
                }
                Some(p)
                    if p.required_checks.len() > 32
                        || !p
                            .required_checks
                            .iter()
                            .any(|c| c.id == check_id && c.validator == validator) =>
                {
                    Some(refusal(Value::Null, "not_required"))
                }
                Some(_) => None,
            }
        } else {
            Some(refusal(Value::Null, "unavailable"))
        };
        if let Some(result) = refused {
            return self
                .settle_fresh(tx, id, revision, intent, &identity, result)
                .await;
        }
        let checks = ports.checks.as_ref().ok_or(Error::Unknown)?;
        let source = source.flatten().ok_or(Error::Unknown)?;
        let request = json!({"check": check, "source": source, "subject": subject,
            "deadline_at_ms": intent.deadline_at_ms, "execution_profile": CHECK_EXECUTION_PROFILE});
        let accepted = self
            .record_operation(
                tx,
                id,
                revision,
                intent,
                &identity,
                Some(check_id),
                &request,
            )
            .await?;
        let attempt = CheckAttempt {
            operation,
            workspace,
            project,
            subject_id: text(subject, "id")?.into(),
            subject_revision: revision_of(subject)?,
            check_id: check_id.into(),
            validator: pin(&check["validator"])?,
            source_format: text(&source, "format")?.into(),
            source_hex: text(&source, "hex")?.into(),
            execution_profile: CHECK_EXECUTION_PROFILE,
        };
        // An ambiguous start leaves the operation running; reconciliation
        // observes it and never issues a second start.
        // The handle is durable from here: an observation failure leaves the
        // operation running for operation.get.
        if checks.start(&attempt).await.is_ok() {
            let _ = self.observe_check(operation).await;
        }
        Ok(accepted)
    }

    /// Observes a running check once and completes it when the executor has
    /// stopped it. Returns the completion if there is one.
    async fn observe_check(&self, operation: Uuid) -> Result<Option<HostResult>> {
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
        self.complete_operation(operation, &op.command, &op.intent_id, result, None)
            .await
            .map(Some)
    }

    /// The cancel plan of the check operation `id` (judgments are not cancellable), read in
    /// the caller's tx: it holds the namespace lock, so a pool read here could deadlock.
    pub(super) async fn check_plan(&self, c: &mut PgConnection, id: Uuid) -> Result<Option<Value>> {
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
    pub(super) async fn cancel_check(&self, operation: Uuid) -> Result<&'static str> {
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
        let scope = self.grant_locked(&mut tx, id, revision, false).await?;
        self.intent_authority(intent, id, revision, &scope)?;
        if let Some(row) = sqlx::query("SELECT request_digest,disposition FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND cancellation_key=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?
        {
            if row.get::<String, _>("request_digest") != identity {
                return Err(Error::Conflict);
            }
            return Ok(if row.get::<String, _>("disposition") == "confirmed" {
                CancellationResult::Confirmed
            } else {
                CancellationResult::TooLate
            });
        }
        let effect = sqlx::query("SELECT request_digest,status FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(effect) = effect else {
            sqlx::query("INSERT INTO playbook_cancellations(namespace_id,generation,cancellation_key,intent_id,request_digest,disposition,created_at_ms) VALUES($1,$2,$3,$4,$5,'confirmed',$6)")
                .bind(self.namespace.namespace_id)
                .bind(self.namespace.generation)
                .bind(&key)
                .bind(&intent.id)
                .bind(&identity)
                .bind(self.now()?)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(CancellationResult::Confirmed);
        };
        if effect.get::<String, _>("request_digest") != identity {
            return Err(Error::Conflict);
        }
        tx.commit().await?;
        // A synchronously settled request (a typed refusal) has no operation.
        if effect.get::<String, _>("status") != "accepted" {
            return Ok(CancellationResult::TooLate);
        }
        Ok(match self.cancel_check(self.operation(intent)?).await? {
            "cancelled" => CancellationResult::Confirmed,
            "accepted" => CancellationResult::Accepted,
            _ => CancellationResult::TooLate,
        })
    }

    // ---------------------------------------------------------- judgment.request

    pub(super) async fn judgment_request(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let (tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        let operation = self.operation(intent)?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                // Lost response: the original request is never re-sent.
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Conflict),
            Stage::Fresh => {}
        }
        let input = &intent.input;
        let subject = &input["subject"];
        let reference = format!("judgment:{operation}");
        let refusal = |provenance: Value, reason: &str| HostResult {
            outcome: "refused".into(),
            output: json!({"provenance": provenance, "reason": reason, "reference": reference, "subject": subject}),
        };
        let workspace = self.namespace.workspace_id;
        let project = self.namespace.project_id;
        let choices: Vec<String> = serde_json::from_value(input["output"]["choices"].clone())
            .map_err(|_| Error::Invalid)?;
        let stale = match Uuid::parse_str(text(subject, "id")?) {
            Ok(node) => match ports.graph.node(workspace, node).await? {
                Some(n) => node_version(&n)? != revision_of(subject)?,
                None => false,
            },
            Err(_) => false,
        };
        let profile = ports
            .judgments
            .as_ref()
            .and_then(|j| j.credential_profile(workspace, project).map(|p| (j, p)));
        let refused = if choices.is_empty() || text(input, "question")?.is_empty() {
            Some(refusal(Value::Null, "invalid_request"))
        } else if profile.is_none() {
            Some(refusal(Value::Null, "no_permitted_profile"))
        } else if stale {
            Some(refusal(Value::Null, "stale_subject"))
        } else {
            None
        };
        if let Some(result) = refused {
            return self
                .settle_fresh(tx, id, revision, intent, &identity, result)
                .await;
        }
        let (provider, credential_profile) = profile.ok_or(Error::Unknown)?;
        let provider = provider.clone();
        let budget = &input["budget"];
        let field = |name: &str| budget[name].as_u64().ok_or(Error::Invalid);
        // The provider runs off the dispatch path, so its deadline is the
        // declared budget, not what is left of the dispatch deadline.
        let request = JudgmentRequest {
            operation,
            credential_profile: credential_profile.clone(),
            question: text(input, "question")?.into(),
            context_text: text(&input["context"], "text")?.into(),
            context_artifacts: serde_json::from_value(input["context"]["artifacts"].clone())
                .map_err(|_| Error::Invalid)?,
            choices: choices.clone(),
            rationale_max_bytes: input["output"]["rationale_max_bytes"]
                .as_u64()
                .ok_or(Error::Invalid)?,
            max_input_tokens: field("max_input_tokens")?,
            max_output_tokens: field("max_output_tokens")?,
            max_cost_microunits: field("max_cost_microunits")?,
            deadline_ms: field("deadline_ms")?,
        };
        // Pre-call estimate: a request expected to exceed the input budget is
        // refused before any operation is recorded or anything is sent.
        let estimated_input_tokens = provider.estimate_input_tokens(&request);
        if estimated_input_tokens > request.max_input_tokens {
            let result = refusal(Value::Null, "budget_exceeded");
            return self
                .settle_fresh(tx, id, revision, intent, &identity, result)
                .await;
        }
        let prompt_digest = digest(&(
            "aq.judgment.prompt",
            &request.question,
            &request.choices,
            request.rationale_max_bytes,
        ))?;
        let context_digest = digest(&("aq.judgment.context", &input["context"]))?;
        let record = json!({"subject": subject, "credential_profile": credential_profile,
            "prompt_digest": prompt_digest, "context_digest": context_digest,
            "estimated_input_tokens": estimated_input_tokens,
            "deadline_at_ms": self.now()? + request.deadline_ms as i64});
        let accepted = self
            .record_operation(tx, id, revision, intent, &identity, None, &record)
            .await?;
        // The intent is durable: the provider request runs in the background
        // and completes the operation by its sealed aq.judgment.completed
        // event. A completion that fails to commit leaves the operation
        // running; it reconciles to indeterminate and is never re-sent.
        let job = JudgmentJob {
            operation,
            command: intent.command.name.clone(),
            intent_id: intent.id.clone(),
            reference: reference.clone(),
            subject: subject.clone(),
            credential_profile,
            prompt_digest,
            context_digest,
            request,
        };
        let service = self.clone();
        tokio::spawn(async move {
            let _ = service.run_judgment(provider, job).await;
        });
        Ok(accepted)
    }

    /// Sends one accepted judgment's provider request, bounded by its budget
    /// deadline, and completes the operation exactly once.
    async fn run_judgment(
        &self,
        provider: Arc<dyn JudgmentProvider>,
        job: JudgmentJob,
    ) -> Result<HostResult> {
        let reply = tokio::time::timeout(
            Duration::from_millis(job.request.deadline_ms),
            provider.complete(&job.request),
        )
        .await;
        let result = job.completion(reply.ok())?;
        self.complete_operation(job.operation, &job.command, &job.intent_id, result, None)
            .await
    }

    // ---------------------------------------------------------- decision.request

    /// D11 Host7 decision.request: one durable question (accepted effect row
    /// plus one running `playbook_host_operations` row) answered once by a
    /// listed responder through [`Service::answer_decision`], or expired at
    /// `expires_at_ms`. The completion seals `aq.decision.completed`. Nothing
    /// is ever auto-approved.
    pub(super) async fn host_decision_request(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        let operation = self.operation(intent)?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Conflict),
            Stage::Fresh => {}
        }
        let now = self.now()?;
        let decision = serde_json::from_value::<DecisionRequest>(intent.input.clone());
        if !decision.is_ok_and(|request| request.valid(now)) {
            let result = HostResult {
                outcome: "refused".into(),
                output: json!({"reference": format!("decision:{operation}"), "subject": intent.input["subject"]}),
            };
            return self
                .settle_fresh(tx, id, revision, intent, &identity, result)
                .await;
        }
        self.record_operation(tx, id, revision, intent, &identity, None, &intent.input)
            .await
    }

    async fn observe_decision(
        &self,
        operation: Uuid,
        op: &Operation,
    ) -> Result<Option<HostResult>> {
        let request: DecisionRequest = serde_json::from_value(op.request.clone())?;
        if self.now()? < request.expires_at_ms {
            return Ok(None);
        }
        self.complete_operation(
            operation,
            &op.command,
            &op.intent_id,
            HostResult {
                outcome: "expired".into(),
                output: request.output(operation),
            },
            None,
        )
        .await
        .map(Some)
    }

    pub(in crate::playbooks) async fn decisions(
        &self,
        actor: Uuid,
        handle: Option<Uuid>,
    ) -> Result<Vec<DecisionView>> {
        let ops = Operations(&self.namespace);
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        ops.require_project_workspace(&mut *tx).await?;
        let views = ops.decisions(&mut *tx, self.now()?, handle).await?;
        tx.commit().await?;
        Ok(views)
    }

    /// Accept one answer from a listed responder and retain its completion.
    /// An answer retry returns the original outcome.
    pub async fn answer_decision(
        &self,
        actor: Uuid,
        handle: Uuid,
        choice: &str,
        note: Option<&str>,
    ) -> Result<HostResult> {
        let ops = Operations(&self.namespace);
        ops.require_project_workspace(&self.pool).await?;
        let op = self
            .load_operation(&self.pool, handle)
            .await?
            .filter(|op| self.profile.is_host2() && op.command == "decision.request")
            .ok_or(Error::Forbidden)?;
        let request: DecisionRequest = serde_json::from_value(op.request.clone())?;
        if !request.permits(actor) {
            return Err(Error::Forbidden);
        }
        if op.status != "running" {
            return op.retained();
        }
        if self.now()? >= request.expires_at_ms {
            return Err(Error::Conflict);
        }
        if !request.choices.iter().any(|c| c == choice)
            || note.is_some_and(|s| {
                s.len() > 2048
                    || (!s.trim().is_empty() && aq_task_comments::validate_body(s).is_err())
                    || s.contains('\0')
            })
        {
            return Err(Error::Invalid);
        }
        let task = match (request.subject_id(), note.filter(|s| !s.trim().is_empty())) {
            (Some(id), Some(body))
                if self
                    .ports()?
                    .graph
                    .node(self.namespace.workspace_id, id)
                    .await?
                    .is_some_and(|node| node["kind"] == "task" && node["deleted"] != true) =>
            {
                Some((id, body))
            }
            _ => None,
        };
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        ops.require_project_workspace(&mut *tx).await?;
        self.namespace_locked(&mut tx).await?;
        let current = self
            .load_operation(&mut *tx, handle)
            .await?
            .ok_or(Error::Forbidden)?;
        if current.status != "running" {
            return current.retained();
        }
        let now = self.now()?;
        if now >= request.expires_at_ms {
            return Err(Error::Conflict);
        }
        let mut output = request.output(handle);
        if self.profile == super::super::review_domain::CatalogProfile::Host11 {
            output["note"] = json!(note.unwrap_or_default());
        }
        let result = self
            .retain_completion(
                &mut tx,
                handle,
                &op.command,
                &op.intent_id,
                HostResult {
                    outcome: choice.into(),
                    output,
                },
                note,
            )
            .await?;
        if let Some((task_id, body)) = task {
            let id = stable_id(
                "aq.playbook.decision.comment",
                self.namespace.namespace_id,
                self.namespace.generation,
                &op.intent_id,
            )?;
            aq_task_comments::add_in_tx(
                &mut tx,
                id,
                aq_task_comments::Thread {
                    project_id: self.namespace.project_id,
                    task_id,
                },
                &aq_task_comments::Author::Owner { user_id: actor },
                body,
                chrono::DateTime::from_timestamp_millis(now).ok_or(Error::Unknown)?,
            )
            .await
            .map_err(|_| Error::Unknown)?;
        }
        tx.commit().await?;
        Ok(result)
    }

    // ------------------------------------------------------------ operation.get

    /// Observation only: reads (and, for a stopped check, completes) the
    /// operation named by the handle in this namespace generation.
    pub(super) async fn host_operation_get(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (tx, stage, _) = self.begin(Some(owner), id, revision, intent).await?;
        tx.commit().await?;
        if !matches!(stage, Some(Stage::Fresh)) {
            return Err(Error::Conflict);
        }
        let handle = text(&intent.input, "handle")?;
        let missing = || HostResult {
            outcome: "refused".into(),
            output: json!({"reference": handle, "subject": subject(handle, 0)}),
        };
        if let Some(result) = self.integration_get(handle).await? {
            self.validated(intent, &result.outcome, &result.output)?;
            return Ok(result);
        }
        let Ok(operation) = Uuid::parse_str(handle) else {
            return Ok(missing());
        };
        let Some(op) = self.load_operation(&self.pool, operation).await? else {
            return Ok(missing());
        };
        // The contract of `operation.get` has no `completed` shape for the
        // delivery outcomes (`synced`, `merged`, ...): a delivery operation is
        // reported while it runs, and once it has completed its answer is the
        // one `aq.delivery.completed` event, which the join already holds.
        if aq_playbook_domain::review_domain::DELIVERY_COMMANDS.contains(&op.command.as_str()) {
            let result = HostResult {
                outcome: if op.status == "running" {
                    "running"
                } else {
                    "refused"
                }
                .into(),
                output: json!({"reference": format!("delivery:{operation}"), "subject": op.request["subject"]}),
            };
            self.validated(intent, &result.outcome, &result.output)?;
            return Ok(result);
        }
        let completion = if op.status != "running" {
            Some(HostResult {
                outcome: op.status.clone(),
                output: op.output.clone().ok_or(Error::Unknown)?,
            })
        } else if op.command == "check.request" {
            self.observe_check(operation).await?
        } else if op.command == "work.ensure" {
            self.observe_work(operation).await?
        } else if op.command == "decision.request" {
            self.observe_decision(operation, &op).await?
        } else if op.command == "judgment.assess" {
            // Answered through its join; it fails at its deadline by itself.
            None
        } else if self.now()?
            >= op.request["deadline_at_ms"]
                .as_i64()
                .ok_or(Error::Unknown)?
                .saturating_add(JUDGMENT_RECONCILE_GRACE_MS)
        {
            // The provider task outlived its deadline and grace without a
            // completion (process loss): indeterminate, never re-sent.
            let output = json!({"provenance": null, "reason": "indeterminate",
                "reference": format!("judgment:{operation}"), "subject": op.request["subject"]});
            Some(
                self.complete_operation(
                    operation,
                    &op.command,
                    &op.intent_id,
                    HostResult {
                        outcome: "failed".into(),
                        output,
                    },
                    None,
                )
                .await?,
            )
        } else {
            None
        };
        let result = match completion {
            Some(done) => HostResult {
                outcome: "completed".into(),
                output: json!({"outcome": done.outcome, "output": done.output}),
            },
            None => {
                let (kind, _) = self.completion_contract(&op.command)?;
                HostResult {
                    outcome: "running".into(),
                    output: json!({"reference": format!("{}:{operation}", kind.as_str().unwrap_or("operation")),
                        "subject": op.request["subject"]}),
                }
            }
        };
        self.validated(intent, &result.outcome, &result.output)?;
        Ok(result)
    }
=======
>>>>>>> refs/remotes/central/main
}

mod checks;
mod decisions;
mod judgments;
mod observation;
mod work;
