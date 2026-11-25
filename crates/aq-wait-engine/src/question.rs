//! A human question and its deterministic wait share the same decision identity.
use super::{AuthorityFence, wire};
use uuid::Uuid;

pub struct QuestionWait {
    pub decision_id: Uuid,
    pub wait_id: Uuid,
    pub subject_revision: String,
    pub question_digest: wire::Digest,
}
impl QuestionWait {
    pub fn new(decision_id: Uuid, binding: &wire::Binding, question_digest: wire::Digest) -> Self {
        Self {
            decision_id,
            wait_id: Uuid::new_v5(&decision_id, b"question-wait"),
            subject_revision: format!(
                "attempt:{}:{}",
                binding.attempt_id, binding.attempt_generation
            ),
            question_digest,
        }
    }

    pub fn registration(
        &self,
        fence: &AuthorityFence,
        policy: wire::ArtifactRef,
    ) -> Result<wire::Request, wire::ContractError> {
        let predicate = wire::Predicate::Single {
            member: wire::Member {
                member_id: Uuid::new_v5(&self.decision_id, b"question-wait-member"),
                condition: wire::Condition::HumanDecision {
                    decision_id: self.decision_id,
                    subject_id: fence.binding.task_id,
                    subject_revision: wire::Text::new(self.subject_revision.clone())?,
                    question_digest: self.question_digest.clone(),
                },
            },
        };
        wire::Request::seal(
            Uuid::new_v5(&self.decision_id, b"question-wait-operation"),
            wire::Command::RegisterWait(wire::WaitRegistration {
                scope: fence.scope.clone(),
                binding: fence.binding.clone(),
                wait_id: self.wait_id,
                wait_generation: 1,
                continuation_generation: 1,
                policy,
                predicate_digest: wire::canonical_digest(&predicate)?,
                predicate,
                preference: wire::WaitPreference::Economical,
            }),
        )
    }
}

/// Route and capability pairs in the agent-questions-v1 protocol.
pub const ROUTES: [(&str, &str); 2] = [
    ("/v1/agent/questions", "question.ask"),
    ("/v1/agent/questions/show", "question.show"),
];
pub const MAX_QUESTION_BYTES: usize = 4096;
/// The mailbox preamble occupies the remainder of the 16 KiB body limit.
pub const MAX_ANSWER_BYTES: usize = 16_000;

#[derive(Clone, Copy)]
pub struct Submission<'a> {
    pub operation_id: Uuid,
    pub text: &'a str,
}
impl<'a> Submission<'a> {
    pub fn new(operation_id: Uuid, text: &'a str) -> Self {
        Self { operation_id, text }
    }
    pub fn valid(&self, limit: usize) -> bool {
        let nonempty = !self.operation_id.is_nil() && !self.text.trim().is_empty();
        nonempty && self.text.len() <= limit
    }
    pub fn receipt_digest(
        &self,
        owner: Uuid,
        decision_id: Uuid,
        revision: i64,
    ) -> Result<String, serde_json::Error> {
        let receipt = serde_json::json!({
            "decision_id":decision_id, "decision_revision":revision,
            "answer_digest":wire::Digest::of(self.text.as_bytes()).as_str(),
            "answered_by":owner, "operation_id":self.operation_id,
        });
        Ok(wire::Digest::of(&serde_json::to_vec(&receipt)?)
            .as_str()
            .to_owned())
    }
}
#[derive(Clone, Copy)]
pub struct DecisionRevision {
    pub id: Uuid,
    pub revision: u64,
}
impl DecisionRevision {
    pub fn new(id: Uuid, revision: u64) -> Self {
        Self { id, revision }
    }
}
pub enum Route {
    Ask,
    Show,
}
pub enum Command {
    Ask {
        operation_id: Uuid,
        question: String,
    },
    Show {
        decision_id: Uuid,
    },
}
impl Route {
    pub fn path(&self) -> &'static str {
        match self {
            Self::Ask => ROUTES[0].0,
            Self::Show => ROUTES[1].0,
        }
    }
    pub fn decode(&self, bytes: &[u8]) -> Result<Command, &'static str> {
        match self {
            Self::Ask => {
                let body: AskBody = parse(bytes, |b: &AskBody| b.api_version)?;
                Ok(Command::Ask {
                    operation_id: body.operation_id,
                    question: body.question,
                })
            }
            Self::Show => {
                let body: ShowBody = parse(bytes, |b: &ShowBody| b.api_version)?;
                Ok(Command::Show {
                    decision_id: body.decision_id,
                })
            }
        }
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AskBody {
    api_version: u32,
    operation_id: Uuid,
    question: String,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowBody {
    api_version: u32,
    decision_id: Uuid,
}
fn parse<T: serde::de::DeserializeOwned>(
    body: &[u8],
    version: impl FnOnce(&T) -> u32,
) -> Result<T, &'static str> {
    let parsed = serde_json::from_slice(body).map_err(|_| "invalid_request")?;
    if version(&parsed) == 1 {
        Ok(parsed)
    } else {
        Err("unsupported_api_version")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_replay_uses_the_persisted_receipt_format() {
        let operation = Uuid::from_u128(1);
        let decision = Uuid::from_u128(2);
        let owner = Uuid::from_u128(3);
        let receipt = Submission::new(operation, "Postgres")
            .receipt_digest(owner, decision, 2)
            .unwrap();
        assert_eq!(
            receipt,
            "da05df1c200ce73a6f1608b3461a363f583ad1245990aff60bd4bd7341ef031a"
        );
    }

    #[test]
    fn question_routes_distinguish_unsupported_versions_from_invalid_payloads() {
        let id = Uuid::from_u128(1);
        let versioned = serde_json::json!({"api_version":2,"decision_id":id});
        let bytes = serde_json::to_vec(&versioned).unwrap();
        assert!(matches!(
            Route::Show.decode(&bytes),
            Err("unsupported_api_version")
        ));
        assert!(matches!(Route::Ask.decode(&bytes), Err("invalid_request")));
    }
}
