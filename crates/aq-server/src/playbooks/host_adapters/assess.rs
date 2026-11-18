//! Host9 (catalog 2.2.0) `judgment.assess`: several labelled questions and
//! yes/no flags answered at once. Like `judgment.request`, one durable
//! intent (an `accepted` effect row and one running operation) is recorded
//! before the provider is asked, and the operation completes exactly once by
//! a sealed `aq.judgment.assessed` event. An accepted judgment that has no
//! answer when its deadline passes fails `deadline_exceeded`
//! ([`Service::expire_assessments`]), also after a restart.
use super::*;
use aq_playbook_domain::host9;

/// One structured judgment for the provider. No secret crosses this boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct AssessRequest {
    pub operation: Uuid,
    /// The internal work purpose of the source the playbook was authored for.
    pub purpose: aq_internal_work::Purpose,
    pub instructions: String,
    pub input: String,
    /// JSON Schema of the answer.
    pub schema: Value,
    pub deadline_ms: u64,
    pub max_output_bytes: u64,
}

/// The provider's answer (an object of the request's schema) and its usage.
#[derive(Clone, Debug, PartialEq)]
pub struct AssessReply {
    pub answer: Value,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

fn ended(outcome: &str, reference: &str, subject: &Value, reason: &str) -> HostResult {
    HostResult {
        outcome: outcome.into(),
        output: host9::assess_ended(reference, subject, reason),
    }
}

impl Service {
    pub(super) async fn judgment_assess(
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
                // Lost response: the original request is never sent again.
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Conflict),
            Stage::Fresh => {}
        }
        let input = &intent.input;
        let (subject, reference) = (&input["subject"], format!("judgment:{operation}"));
        // The host's caps are internal work's: its longest deadline and its
        // largest output, at four bytes a token.
        let max_deadline = *aq_internal_work::DEADLINE_MS.end();
        let max_tokens = *aq_internal_work::MAX_OUTPUT_BYTES.end() / 4;
        if let Some(reason) = host9::assess_refusal(input, max_deadline, max_tokens) {
            let refused = ended("refused", &reference, subject, reason);
            return self
                .settle_fresh(tx, id, revision, intent, &identity, refused)
                .await;
        }
        // The purpose is the one of the source this grant was made for.
        let purpose: Option<String> = sqlx::query_scalar("SELECT s.judgment_purpose FROM playbook_source_grants g JOIN playbook_sources s ON s.id=g.source_id WHERE g.grant_id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let purpose = purpose.and_then(|p| serde_json::from_value(Value::String(p)).ok());
        let budget = |name: &str| input["budget"][name].as_u64().ok_or(Error::Invalid);
        let (instructions, context) = host9::assess_prompt(input);
        let request = AssessRequest {
            operation,
            purpose: purpose.unwrap_or(aq_internal_work::Purpose::PlaybookCall),
            instructions,
            input: context,
            schema: host9::assess_schema(input),
            deadline_ms: budget("deadline_ms")?,
            max_output_bytes: budget("max_output_tokens")?.saturating_mul(4),
        };
        let deadline_at_ms = self.now()? + request.deadline_ms as i64;
        let record = json!({"subject": subject, "deadline_at_ms": deadline_at_ms});
        let accepted = self
            .record_operation(tx, id, revision, intent, &identity, None, &record)
            .await?;
        let (service, provider) = (self.clone(), ports.judgments.clone());
        let (intent_id, input) = (intent.id.clone(), input.clone());
        tokio::spawn(async move {
            let _ = (service.run_assessment(provider, request, &intent_id, &input)).await;
        });
        Ok(accepted)
    }

    /// Asks the provider, bounded by the budget deadline, and completes the
    /// operation. A completion that cannot be recorded leaves the operation
    /// running; it fails at its deadline.
    async fn run_assessment(
        &self,
        provider: Option<Arc<dyn JudgmentProvider>>,
        request: AssessRequest,
        intent_id: &str,
        input: &Value,
    ) -> Result<HostResult> {
        let (subject, reference) = (&input["subject"], format!("judgment:{}", request.operation));
        let reply = match &provider {
            Some(provider) => {
                let limit = Duration::from_millis(request.deadline_ms);
                let reply = tokio::time::timeout(limit, provider.assess(&request)).await;
                reply.unwrap_or(Err("deadline_exceeded"))
            }
            None => Err("provider_unavailable"),
        };
        let assessed = reply.and_then(|reply| {
            let named = |name: &str, max| !name.is_empty() && name.len() <= max;
            (host9::assess_answer(&reply.answer, input).ok())
                .filter(|_| named(&reply.provider, 128) && named(&reply.model, 256))
                .map(|answer| (reply, answer))
                .ok_or("indeterminate")
        });
        let result = match assessed {
            Err(reason) => ended("failed", &reference, subject, reason),
            Ok((reply, (labels, flags, rationale))) => {
                let (workspace, project) = (self.namespace.workspace_id, self.namespace.project_id);
                let credential = provider.and_then(|p| p.credential_profile(workspace, project));
                let prompt =
                    digest(&("aq.judgment.assess", &request.instructions, &request.schema))?;
                let provenance = json!({"provider": reply.provider, "model": reply.model,
                    "credential_profile": credential.unwrap_or_default(), "prompt_digest": prompt,
                    "context_digest": digest(&("aq.judgment.context", &input["context"]))?,
                    "output_digest": digest(&("aq.judgment.assess.output", &labels, &flags, &rationale))?,
                    "usage": {"input_tokens": reply.input_tokens, "output_tokens": reply.output_tokens,
                        "cost_microunits": 0}});
                HostResult {
                    outcome: "assessed".into(),
                    output: json!({"labels": labels, "flags": flags, "rationale": rationale,
                        "provenance": provenance, "reference": reference, "subject": subject}),
                }
            }
        };
        self.complete_operation(request.operation, "judgment.assess", intent_id, result)
            .await
    }

    /// Every accepted judgment gets exactly one final answer: one still
    /// running after its deadline (and the reconcile grace) fails
    /// `deadline_exceeded`. Returns how many were failed.
    pub async fn expire_assessments(&self, owner: &RuntimeOwner, limit: u32) -> Result<usize> {
        if !self.profile.has_routing() || self.host.is_none() {
            return Ok(0);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let due: Vec<(Uuid, String, Value)> = sqlx::query_as("SELECT operation_id,intent_id,request FROM playbook_host_operations WHERE namespace_id=$1 AND generation=$2 AND command_name='judgment.assess' AND status='running' AND (request->>'deadline_at_ms')::bigint<=$3 ORDER BY created_at_ms,operation_id LIMIT $4")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(self.now()?.saturating_sub(JUDGMENT_RECONCILE_GRACE_MS))
            .bind(i64::from(limit))
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        for (operation, intent_id, request) in &due {
            let reference = format!("judgment:{operation}");
            let failed = ended(
                "failed",
                &reference,
                &request["subject"],
                "deadline_exceeded",
            );
            self.complete_operation(*operation, "judgment.assess", intent_id, failed)
                .await?;
        }
        Ok(due.len())
    }
}
