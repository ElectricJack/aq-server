//! Borrowed actual-owner B1 -> explicit combined question -> B2/C fixture.
//! No service/process lifecycle, SQL proof insertion, direct Graph mutation,
//! fabricated launch/stop/source receipt, or real-human approval lives here.
#![allow(dead_code)]
use super::actual_git_owner::Owner as GitOwner;
use anyhow::{Context, Result, ensure};
use aq_admission::{ArtifactRef, Digest, TaskRevision, Text};
use aq_review_protocol::{Choice, DecisionReceipt, DecisionRequest, ResolveRequest};
use aq_server::{
    AppState,
    profiled_execution::{
        results,
        settlement::{self, EvaluationDisposition},
    },
    reviews::{
        self,
        combined::{
            CombinedSubject, ExternalDeclaration, ExternalDisposition, ExternalEffect,
            ExternalScope,
        },
    },
    source_retention,
    store::{Role, User},
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

pub const PROOF_PATH: &str = "gate04-proof.txt";
pub const PROOF_BYTES: &[u8] = b"AQ native result fixture proof\n";
// SHA1("blob 31\0" || PROOF_BYTES), a fixed expected-content fixture value,
// NOT a supplied source commit or a fabricated retention acknowledgement.
pub const PROOF_BLOB: &str = "debd0a369a66d685f9d2eca5a0de7684d47bcecc";
const CONCLUSION: &str = "Changes: created gate04-proof.txt; Checks: /usr/bin/cmp -s - gate04-proof.txt exit0; Limits: finite local peer, no provider/check/review claim";

fn stage_error(stage: &'static str, error: aq_server::error::AppError) -> anyhow::Error {
    anyhow::anyhow!(
        "{stage} refused by actual owner (HTTP {})",
        error.status().as_u16()
    )
}
fn subject_reference(subject: &CombinedSubject) -> Result<ArtifactRef> {
    subject
        .artifact()
        .map_err(|e| stage_error("combined subject", e))
}
fn canonical(v: &impl Serialize) -> Result<Vec<u8>> {
    let mut v = serde_json::to_value(v)?;
    v.sort_all_objects();
    Ok(serde_json::to_vec(&v)?)
}
fn reference(id: String, bytes: &[u8]) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new(id).map_err(anyhow::Error::msg)?,
        version: Text::new("1").map_err(anyhow::Error::msg)?,
        digest: Digest::of(bytes),
    })
}
fn now(state: &AppState) -> Result<u64> {
    Ok(u64::try_from((state.clock)().timestamp_millis())?)
}
fn exact_keys(v: &Value, keys: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == keys.len() && keys.iter().all(|k| o.contains_key(*k)))
}
async fn current_owner(pool: &sqlx::PgPool, id: Uuid) -> Result<User> {
    ensure!(!id.is_nil(), "actual owner identity absent");
    let row = sqlx::query("SELECT id,email,role,issuer,subject FROM users WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    ensure!(
        row.get::<String, _>("role") == "owner",
        "fixture current actor is not Owner"
    );
    let user = User {
        id: row.get("id"),
        email: row.get("email"),
        role: Role::Owner,
        issuer: row.get("issuer"),
        subject: row.get("subject"),
    };
    ensure!(
        user.issuer.as_ref().is_some_and(|s| !s.is_empty())
            && user.subject.as_ref().is_some_and(|s| !s.is_empty()),
        "fixture Owner must be actually bound"
    );
    Ok(user)
}

/// Data IDs captured ONCE. Reuse them and the returned immutable question on
/// recovery; do not restamp a question's expiry or invent new answer/eval IDs.
#[derive(Clone, Debug, Serialize)]
pub struct StableIds {
    pub question_operation: Uuid,
    pub decision: Uuid,
    pub answer_operation: Uuid,
    pub evaluation_operation: Uuid,
    pub question_expires_at_ms: u64,
}
impl StableIds {
    pub fn capture(state: &AppState) -> Result<Self> {
        Ok(Self {
            question_operation: Uuid::new_v4(),
            decision: Uuid::new_v4(),
            answer_operation: Uuid::new_v4(),
            evaluation_operation: Uuid::new_v4(),
            question_expires_at_ms: now(state)?
                .checked_add(300_000)
                .context("question deadline overflow")?,
        })
    }
    fn validate(&self) -> Result<()> {
        let ids = [
            self.question_operation,
            self.decision,
            self.answer_operation,
            self.evaluation_operation,
        ];
        ensure!(
            ids.iter().all(|id| !id.is_nil())
                && ids.iter().collect::<std::collections::BTreeSet<_>>().len() == 4,
            "stable fixture IDs must be distinct"
        );
        ensure!(
            self.question_expires_at_ms > 0 && self.question_expires_at_ms <= i64::MAX as u64,
            "question timestamp bound"
        );
        Ok(())
    }
}
/// Caller supplies only actual readonly PrivateState.read output and the finite
/// peer's actual captured-request validator output. These are observations, NOT
/// authority or passed-check DTOs. The raw binding nonce is never returned/logged.
pub struct Observations<'a> {
    pub original_local_binding: &'a Value,
    pub validated_tool_output: &'a Value,
    pub worktree_member: &'a str,
    pub private_file_bytes: &'a [u8],
}

pub struct PreparedQuestion {
    owner: User,
    ids: StableIds,
    report_id: Uuid,
    source_operation: Uuid,
    subject: CombinedSubject,
    question: DecisionRequest,
    inspection: Value,
    observations: ObservationRefs,
    original_task: TaskRevision,
    checkpoint: aq_worker_protocol::ObjectId,
}
#[derive(Clone, Debug, Serialize)]
pub struct ObservationRefs {
    pub actual_tool_output: ArtifactRef,
    pub actual_private_file: ArtifactRef,
    pub actual_retained_tree: ArtifactRef,
}
impl PreparedQuestion {
    pub fn subject(&self) -> &CombinedSubject {
        &self.subject
    }
    pub fn question(&self) -> &DecisionRequest {
        &self.question
    }
    pub fn inspection(&self) -> &Value {
        &self.inspection
    }
    pub fn observation_refs(&self) -> &ObservationRefs {
        &self.observations
    }
    /// Preparation-only handoff for an actual later human interface. No answer,
    /// evaluation or completion is executed by this data-only view.
    pub fn human_handoff(&self) -> Result<Value> {
        Ok(
            json!({"profile":"actual_fixture_combined_review_handoff_v1","owner_choice":"not_made","question":self.question,"subject":self.subject,"subject_artifact":subject_reference(&self.subject)?,"observations":self.observations,"original_task":self.original_task,"checkpoint":self.checkpoint}),
        )
    }
}

fn validate_retained_tree(tree: &Value) -> Result<()> {
    ensure!(
        canonical(tree)?.len() <= 65536 && tree["truncated"] == false,
        "actual retained source tree must be bounded/complete"
    );
    let entries = tree["entries"]
        .as_array()
        .context("actual retained tree entries absent")?;
    ensure!(entries.len() <= 100, "actual retained tree entry bound");
    let files: Vec<_> = entries
        .iter()
        .filter(|e| {
            (e["path"]["encoding"] == "utf8" && e["path"]["value"] == PROOF_PATH)
                || (e["path"]["encoding"] == "hex"
                    && e["path"]["value"] == "6761746530342d70726f6f662e747874")
        })
        .collect();
    ensure!(
        files.len() == 1
            && files[0]["mode"] == "100644"
            && files[0]["type"] == "blob"
            && files[0]["oid"] == json!({"algorithm":"sha1","hex":PROOF_BLOB}),
        "actual retained checkpoint does not contain exact checked proof file"
    );
    Ok(())
}

pub struct B1Request<'a> {
    pub pool: &'a sqlx::PgPool,
    pub actor: Uuid,
    pub report_id: Uuid,
    pub source_operation: Uuid,
    pub original_task: &'a TaskRevision,
    pub ids: &'a StableIds,
}
pub async fn prepare_question(
    state: &AppState,
    reviews: &reviews::Service,
    git: &GitOwner,
    request: B1Request<'_>,
    observed: Observations<'_>,
) -> Result<PreparedQuestion> {
    let B1Request {
        pool,
        actor,
        report_id,
        source_operation,
        original_task,
        ids,
    } = request;
    ids.validate()?;
    ensure!(
        report_id != Uuid::nil() && source_operation != Uuid::nil(),
        "actual B1/source IDs absent"
    );
    ensure!(
        observed.worktree_member == PROOF_PATH && observed.private_file_bytes == PROOF_BYTES,
        "actual private fixed file differs"
    );
    let local = observed.original_local_binding;
    let tools = observed.validated_tool_output;
    ensure!(
        canonical(local)?.len() <= 16384 && canonical(tools)?.len() <= 16384,
        "bounded actual tool/binding observations"
    );
    ensure!(
        exact_keys(
            local,
            &[
                "schema_version",
                "launch",
                "execution_record_digest",
                "expected_outcome",
                "operation_id",
                "report_id",
                "nonce"
            ]
        ) && local["schema_version"] == 1,
        "original actual local binding shape"
    );
    ensure!(
        exact_keys(
            tools,
            &[
                "operation_id",
                "report_id",
                "binding_digest",
                "submission_digest",
                "phase",
                "projection",
                "completion"
            ]
        ),
        "actual tool validator shape"
    );
    ensure!(
        tools["projection"] == "local_result_data_only"
            && matches!(
                tools["phase"].as_str(),
                Some("accepted" | "finalizing" | "recorded")
            ),
        "tool client output is not a retained local result acknowledgement"
    );
    ensure!(
        tools["binding_digest"] == json!(Digest::of(&canonical(local)?))
            && tools["operation_id"] == local["operation_id"]
            && tools["report_id"] == local["report_id"]
            && local["report_id"] == json!(report_id),
        "actual tool output/local binding cross-identity mismatch"
    );
    let _: Digest = serde_json::from_value(local["execution_record_digest"].clone())?;
    ensure!(
        tools["completion"]
            == "AQ finite tool sequence ended; worker finalization remains separate."
            && tools["submission_digest"]
                == json!(Digest::of(&canonical(
                    &json!({"schema_version":1,"binding":local,"conclusion":CONCLUSION})
                )?)),
        "actual tool validation differs from the fixed original claim"
    );
    let owner = current_owner(pool, actor).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2000ms',true)").execute(&mut *tx).await?;
    let record = results::load_locked(state, &mut tx, report_id)
        .await
        .map_err(|e| stage_error("B1 read", e))?;
    ensure!(
        record.is_current_report_head && record.spec.task == *original_task,
        "actual B1 must retain current original task pin"
    );
    ensure!(
        !matches!(record.origin, results::RecordedOrigin::LegacyPrepareV1),
        "actual native phase B1 required; no legacy fallback"
    );
    ensure!(
        local["launch"] == serde_json::to_value(&record.report.binding)?
            && local["expected_outcome"] == serde_json::to_value(&record.receipt.expected_outcome)?
            && local["operation_id"] == json!(record.report.operation_id),
        "actual B1 differs from original tool/result binding"
    );
    ensure!(
        record.report.conclusion.as_str() == CONCLUSION,
        "actual B1 conclusion differs from fixed tool claim"
    );
    let source = source_retention::load_confirmed_locked(&mut tx, source_operation, &record)
        .await
        .map_err(|e| stage_error("retained source read", e))?;
    ensure!(
        source.receipt.binding_generation == 1
            && source.receipt.source != source.manifest.source.base,
        "actual changed retained checkpoint at original repository generation required"
    );
    tx.commit().await?;
    // Read immutable actual central source, not another workspace or a supplied
    // blob/commit receipt. Parent keeps this existing Git owner alive throughout.
    let tree = git
        .call(
            source.receipt.repository_id,
            "tree",
            json!({"oid":source.receipt.source,"limit":100,"recursive":true}),
        )
        .await;
    validate_retained_tree(&tree)?;
    let refs = ObservationRefs {
        actual_tool_output: reference(
            format!("aq.fixture-observed-result-tools/{report_id}"),
            &canonical(tools)?,
        )?,
        actual_private_file: reference(
            format!("aq.fixture-private-file/{PROOF_PATH}"),
            observed.private_file_bytes,
        )?,
        actual_retained_tree: reference(
            format!("aq.fixture-retained-tree/{}", source.receipt.source.hex),
            &canonical(&tree)?,
        )?,
    };
    let declaration=ExternalDeclaration{scope:ExternalScope::TrustedNodeSourceAndAqReadsOnly,worktree_attempt_id:record.report.binding.attempt_id,known_effects:vec![ExternalEffect{resource:format!("worktree:{}/{PROOF_PATH}",record.report.binding.attempt_id),disposition:ExternalDisposition::Reconciled,reason:"Actual fixed tool cmp, readonly private file bytes and central retained checkpoint tree agree. This is fixture observation, not an AQ required-check receipt.".into(),evidence:vec![refs.actual_tool_output.clone(),refs.actual_private_file.clone(),refs.actual_retained_tree.clone()]}],reason:"Explicit trusted-node fixture source/file scope plus actual exhaustive AQ ledger; no shell/network-absence inference.".into(),residual_uncertainty:"Finite local provider peer; preparation asserts no human choice, paid model, universal external-effect isolation or general check-executor qualification.".into()};
    let prepared =
        Box::pin(reviews.prepare_combined_as(&owner, report_id, source_operation, declaration))
            .await
            .map_err(|e| stage_error("prepare combined review", e))?;
    let subject = prepared.subject().clone();
    ensure!(
        subject.code.task.task_id == original_task.task_id
            && subject.code.result.record_receipt == record.receipt_artifact,
        "actual combined owner rebound the question"
    );
    let question = DecisionRequest {
        schema_version: 1,
        operation_id: ids.question_operation,
        decision_id: ids.decision,
        subject: subject_reference(&subject)?,
        eligible_owners: vec![owner.id],
        eligibility_project_revision: subject.code.original_project_revision,
        review_policy: subject.code.validation_policy.clone(),
        expires_at_ms: ids.question_expires_at_ms,
    };
    Box::pin(reviews.admit_combined(owner.id, &prepared, &question))
        .await
        .map_err(|e| stage_error("admit combined question", e))?;
    let inspection = Box::pin(reviews.inspect_combined_as(&owner, question.decision_id))
        .await
        .map_err(|e| stage_error("inspect exact question", e))?;
    ensure!(
        inspection["request"] == serde_json::to_value(&question)?
            && inspection["subject"] == serde_json::to_value(&subject)?
            && inspection["state"] == "pending",
        "actual question owner did not retain exact pending combined subject"
    );
    Ok(PreparedQuestion {
        owner,
        ids: ids.clone(),
        report_id,
        source_operation,
        subject,
        question,
        inspection,
        observations: refs,
        original_task: original_task.clone(),
        checkpoint: source.receipt.source,
    })
}

/// Explicit test-controller action, never a human/protocol DTO or agent choice.
pub enum FixtureOwnerDecision {
    ApproveAfterExactProof,
}
#[derive(Serialize)]
pub struct FixtureSettlement {
    pub owner_choice: &'static str,
    pub subject: ArtifactRef,
    pub observations: ObservationRefs,
    pub answer: DecisionReceipt,
    pub evaluation: settlement::EvaluationReceipt,
    pub completion: settlement::CompletionStatus,
    pub original_task: TaskRevision,
    pub checkpoint: aq_worker_protocol::ObjectId,
}
pub async fn finish_fixture_choice(
    state: &AppState,
    reviews: &reviews::Service,
    prepared: &PreparedQuestion,
    choice: FixtureOwnerDecision,
) -> Result<FixtureSettlement> {
    let FixtureOwnerDecision::ApproveAfterExactProof = choice;
    let answer=ResolveRequest{schema_version:1,operation_id:prepared.ids.answer_operation,decision_id:prepared.question.decision_id,expected_decision_revision:1,choice:Choice::Approve,comment:Some("SIMULATED FIXTURE OWNER: exact observed fixed cmp/file/B1/retained tree and declared scope only; not real human approval.".into())};
    let subject = subject_reference(&prepared.subject)?;
    let answer = Box::pin(reviews.resolve_combined_as(&prepared.owner, &answer, &subject))
        .await
        .map_err(|e| stage_error("explicit fixture answer", e))?;
    settle_after_answer(
        state,
        reviews,
        prepared,
        answer,
        "simulated_fixture_approval_NOT_real_human",
    )
    .await
}
/// Resume ONLY after an existing protected Owner answer, without creating or
/// changing an answer. Its origin is not inferred to be a real person here.
pub async fn settle_recorded_answer(
    state: &AppState,
    reviews: &reviews::Service,
    prepared: &PreparedQuestion,
) -> Result<FixtureSettlement> {
    let view =
        Box::pin(reviews.inspect_combined_as(&prepared.owner, prepared.question.decision_id))
            .await
            .map_err(|e| stage_error("inspect recorded answer", e))?;
    let answer: DecisionReceipt = serde_json::from_value(view["receipt"].clone())
        .context("protected Owner answer is not recorded")?;
    ensure!(
        answer.subject == subject_reference(&prepared.subject)?
            && matches!(
                answer.finality,
                aq_review_protocol::DecisionFinality::HumanAnswer {
                    choice: Choice::Approve,
                    ..
                }
            ),
        "protected answer has not approved this exact combined subject"
    );
    settle_after_answer(
        state,
        reviews,
        prepared,
        answer,
        "existing_protected_owner_answer_origin_NOT_inferred",
    )
    .await
}
async fn settle_after_answer(
    state: &AppState,
    reviews: &reviews::Service,
    prepared: &PreparedQuestion,
    answer: DecisionReceipt,
    owner_choice: &'static str,
) -> Result<FixtureSettlement> {
    let subject = subject_reference(&prepared.subject)?;
    let service = settlement::Service::new(state.clone(), reviews.clone());
    let request = settlement::EvaluateRequest {
        schema_version: 1,
        operation_id: prepared.ids.evaluation_operation,
        report_id: prepared.report_id,
        source_operation_id: prepared.source_operation,
        decision_id: prepared.question.decision_id,
        subject: subject.clone(),
    };
    let evaluation = Box::pin(service.evaluate(&prepared.owner, &request))
        .await
        .map_err(|e| stage_error("actual eligibility", e))?;
    ensure!(
        evaluation.disposition == EvaluationDisposition::Accepted,
        "actual eligibility refused: {:?} {:?}",
        evaluation.disposition,
        evaluation.reason
    );
    let completion = Box::pin(
        service.complete(
            &prepared.owner,
            evaluation
                .completion_operation_id
                .context("accepted evaluation omitted completion identity")?,
        ),
    )
    .await
    .map_err(|e| stage_error("original completion effect", e))?;
    if let Some(ref receipt) = completion.receipt {
        ensure!(
            completion.state == "confirmed"
                && receipt.workspace_id == prepared.original_task.workspace_id
                && receipt.task_id == prepared.original_task.task_id
                && Some(receipt.version) == prepared.original_task.version.checked_add(1)
                && Some(receipt.content_revision)
                    == prepared.original_task.content_revision.checked_add(1),
            "actual completion receipt differs from original expected-version task"
        );
    }
    Ok(FixtureSettlement {
        owner_choice,
        subject,
        observations: prepared.observations.clone(),
        answer,
        evaluation,
        completion,
        original_task: prepared.original_task.clone(),
        checkpoint: prepared.checkpoint.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof_entry(encoding: &str, path: &str) -> Value {
        json!({"path":{"encoding":encoding,"value":path},"mode":"100644",
            "type":"blob","oid":{"algorithm":"sha1","hex":PROOF_BLOB}})
    }

    #[test]
    fn retained_tree_accepts_exact_utf8_or_hex_proof_but_never_duplicate_members() {
        let utf8 = proof_entry("utf8", PROOF_PATH);
        let hex = proof_entry("hex", &hex::encode(PROOF_PATH));
        for entry in [&utf8, &hex] {
            validate_retained_tree(&json!({"truncated":false,"entries":[entry]})).unwrap();
        }
        assert!(validate_retained_tree(&json!({"truncated":false,"entries":[utf8,hex]})).is_err());
    }

    #[test]
    fn retained_tree_refuses_incomplete_or_changed_proof() {
        let entry = proof_entry("utf8", PROOF_PATH);
        for tree in [
            json!({"truncated":true,"entries":[entry]}),
            json!({"entries":[entry]}),
            json!({"truncated":false,"entries":[]}),
            json!({"truncated":false,"entries":vec![entry.clone();101]}),
        ] {
            assert!(validate_retained_tree(&tree).is_err());
        }
        for (key, value) in [
            ("mode", json!("100755")),
            ("type", json!("tree")),
            ("oid", json!({"algorithm":"sha1","hex":"0".repeat(40)})),
            ("path", json!({"encoding":"utf8","value":"other.txt"})),
        ] {
            let mut changed = entry.clone();
            changed[key] = value;
            assert!(
                validate_retained_tree(&json!({"truncated":false,"entries":[changed]})).is_err()
            );
        }
    }
}
