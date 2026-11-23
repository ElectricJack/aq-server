use super::*;
use aq_server::playbooks::host_adapters::{
    AssessReply, AssessRequest, JudgmentReply, JudgmentRequest, ProviderFailure,
};

/// Provider fake: one scripted answer; `held` makes it wait for `release`.
#[derive(Default)]
pub(super) struct FakeAssessor {
    answer: Mutex<Option<std::result::Result<Value, &'static str>>>,
    pub(super) requests: Mutex<Vec<AssessRequest>>,
    pub(super) held: std::sync::atomic::AtomicBool,
    pub(super) release: tokio::sync::Notify,
}
impl FakeAssessor {
    pub(super) fn answering(answer: std::result::Result<Value, &'static str>) -> Arc<Self> {
        let fake = Self::default();
        *fake.answer.lock().unwrap() = Some(answer);
        Arc::new(fake)
    }
}
#[async_trait::async_trait]
impl JudgmentProvider for FakeAssessor {
    fn credential_profile(&self, _: Uuid, _: Uuid) -> Option<String> {
        Some("profile/judgment-default".into())
    }
    async fn complete(
        &self,
        _: &JudgmentRequest,
    ) -> std::result::Result<JudgmentReply, ProviderFailure> {
        Err(ProviderFailure::Unavailable)
    }
    async fn assess(
        &self,
        request: &AssessRequest,
    ) -> std::result::Result<AssessReply, &'static str> {
        self.requests.lock().unwrap().push(request.clone());
        if self.held.load(std::sync::atomic::Ordering::SeqCst) {
            self.release.notified().await;
        }
        let answer = self.answer.lock().unwrap().clone().unwrap()?;
        Ok(AssessReply {
            answer,
            provider: "fixture".into(),
            model: "model-a".into(),
            input_tokens: 40,
            output_tokens: 9,
        })
    }
}

pub(super) fn assess_input(task: Uuid) -> Value {
    json!({"subject": {"id": task.to_string(), "revision": 3},
        "question": "Classify this task for routing.",
        "context": {"items": [{"label": "Title", "text": "Fix billing rounding"}]},
        "fields": [
            {"id": "risk", "description": "Risk of the change.", "choices": ["low", "medium", "high", "unknown"]},
            {"id": "kind", "description": "What the task is.", "choices": ["bugfix", "feature"]}],
        "flags": [{"id": "narrow", "description": "Touches one module."},
            {"id": "hard", "description": "Unusually hard reasoning."}],
        "rationale_max_bytes": 200,
        "budget": {"deadline_ms": 120000, "max_output_tokens": 2048}})
}
impl Host {
    /// (status, output) of the operation behind a handle.
    pub(super) async fn operation(&self, handle: &str) -> (String, Option<Value>) {
        sqlx::query_as("SELECT status,output FROM playbook_host_operations WHERE operation_id=$1")
            .bind(Uuid::parse_str(handle).unwrap())
            .fetch_one(&self.f.db.pool().await)
            .await
            .unwrap()
    }
    pub(super) async fn completed(&self, handle: &str) -> (String, Value) {
        for _ in 0..400 {
            let (status, output) = self.operation(handle).await;
            if status != "running" {
                return (status, output.unwrap());
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("judgment {handle} did not complete");
    }
    /// The sealed `aq.judgment.assessed` events queued for the engine.
    pub(super) async fn events(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM playbook_outbox WHERE namespace_id=$1")
            .bind(self.service.namespace.namespace_id)
            .fetch_one(&self.f.db.pool().await)
            .await
            .unwrap()
    }
    pub(super) async fn assess(&self, id: &str, input: Value) -> String {
        let accepted = self.run(id, "judgment.assess", input).await;
        assert_eq!(accepted.outcome, "accepted", "{accepted:?}");
        accepted.output["handle"].as_str().unwrap().to_owned()
    }
}
/// What the catalog says the completion event carries for an outcome.
pub(super) fn completion_valid(outcome: &str, output: &Value) {
    let registry = CatalogProfile::Host9.registry();
    let contract = registry.commands["judgment.assess"]
        .asynchronous
        .clone()
        .unwrap();
    (contract
        .completion
        .validate(&json!({"outcome": outcome, "output": output})))
    .unwrap();
    registry.commands["judgment.assess"].outcomes[outcome]
        .validate(output)
        .unwrap();
}

pub(super) struct LowRisk;
#[async_trait::async_trait]
impl JudgmentProvider for LowRisk {
    fn credential_profile(&self, _: Uuid, _: Uuid) -> Option<String> {
        Some("profile/judge".into())
    }
    async fn complete(
        &self,
        _: &JudgmentRequest,
    ) -> std::result::Result<JudgmentReply, ProviderFailure> {
        Err(ProviderFailure::Unavailable)
    }
    async fn assess(&self, r: &AssessRequest) -> std::result::Result<AssessReply, &'static str> {
        let properties = |part: &str| {
            r.schema["properties"][part]["properties"]
                .as_object()
                .unwrap()
                .clone()
        };
        let labels: serde_json::Map<String, Value> = (properties("labels").into_iter())
            .map(|(id, field)| {
                let choices = field["enum"].as_array().unwrap();
                let low = choices.iter().find(|c| *c == "low").unwrap_or(&choices[0]);
                (id, low.clone())
            })
            .collect();
        let flags: serde_json::Map<String, Value> = (properties("flags").into_iter())
            .map(|(id, _)| (id, json!(false)))
            .collect();
        Ok(AssessReply {
            answer: json!({"labels": labels, "flags": flags, "rationale": "Only documentation changes."}),
            provider: "fixture".into(),
            model: "model-a".into(),
            input_tokens: 1,
            output_tokens: 1,
        })
    }
}
