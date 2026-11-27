//! Sanitized synthetic wire claims. No actual retained source/human/check proof.
use aq_review_protocol::*;
use serde_json::{Value, json};
use uuid::Uuid;
fn reference(id: &str, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    }
}
pub fn fixtures() -> Value {
    let subject = subject();
    let (request, resolve, receipt) = decision(&subject);
    let check = check_receipt(&subject);
    json!({"subject":subject,"subject_artifact":subject.artifact().unwrap(),"decision_request":request,"resolve_request":resolve,"decision_receipt":receipt,"decision_artifact":receipt.artifact().unwrap(),"check_receipt":check,"check_artifact":check.artifact().unwrap(),"qualification":"synthetic wire claims only; no source/human/check authority"})
}
fn subject() -> ReviewSubject {
    let upstream: Value = serde_json::from_str(include_str!(
        "../../artifacts/upstream-result-record-v1.fixture.json"
    ))
    .unwrap();
    let outcome: Value = serde_json::from_str(include_str!(
        "../../artifacts/upstream-outcome-v1.fixture.json"
    ))
    .unwrap();
    let report_id: Uuid = serde_json::from_value(upstream["report"]["report_id"].clone()).unwrap();
    let binding: LaunchBinding =
        serde_json::from_value(upstream["report"]["binding"].clone()).unwrap();
    let subject = ReviewSubject {
        schema_version: 1,
        project_id: serde_json::from_value(outcome["project_id"].clone()).unwrap(),
        original_project_revision: outcome["project_revision"].as_u64().unwrap(),
        task: serde_json::from_value(outcome["task"].clone()).unwrap(),
        binding: binding.clone(),
        execution_spec: ArtifactRef {
            id: Text::new("aq.profiled-execution-spec").unwrap(),
            version: Text::new("1").unwrap(),
            digest: binding.execution_spec_digest,
        },
        expected_outcome: serde_json::from_value(upstream["receipt"]["expected_outcome"].clone())
            .unwrap(),
        task_observation: serde_json::from_value(outcome["task_observation"].clone()).unwrap(),
        task_context: serde_json::from_value(outcome["task_context"].clone()).unwrap(),
        validation_policy: serde_json::from_value(outcome["validation_policy"].clone()).unwrap(),
        result: ResultPin {
            report_id,
            report_digest: serde_json::from_value(upstream["receipt"]["report_digest"].clone())
                .unwrap(),
            record_receipt: ArtifactRef {
                id: Text::new(format!("aq.result-record/{report_id}")).unwrap(),
                version: Text::new("1").unwrap(),
                digest: digest(&upstream["receipt"]).unwrap(),
            },
            stopped_receipt: serde_json::from_value(upstream["receipt"]["stopped_receipt"].clone())
                .unwrap(),
        },
        source: source_pin(&outcome["source"]),
        criteria: outcome["acceptance_criteria"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| Text::new(c["id"].as_str().unwrap()).unwrap())
            .collect(),
    };
    subject.validate().unwrap();
    subject
}
fn source_pin(source: &Value) -> SourcePin {
    SourcePin {
        repository_id: serde_json::from_value(source["repository_id"].clone()).unwrap(),
        binding_generation: source["binding_generation"].as_u64().unwrap(),
        base: serde_json::from_value(source["base"].clone()).unwrap(),
        resulting_object: ObjectId {
            format: ObjectFormat::Sha1,
            hex: "2".repeat(40),
        },
        input_artifact: serde_json::from_value(source["artifact"].clone()).unwrap(),
        resulting_artifact: reference(
            "aq.synthetic-resulting-source",
            b"synthetic resulting source; not retained",
        ),
        source_receipt_id: Uuid::from_u128(900),
        retention_receipt: reference(
            "aq.synthetic-source-owner-receipt",
            b"synthetic receipt; not source confirmation",
        ),
    }
}

fn decision(subject: &ReviewSubject) -> (DecisionRequest, ResolveRequest, DecisionReceipt) {
    let request = DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::from_u128(901),
        decision_id: Uuid::from_u128(902),
        subject: subject.artifact().unwrap(),
        eligible_owners: vec![Uuid::from_u128(903)],
        eligibility_project_revision: 1,
        review_policy: subject.validation_policy.clone(),
        expires_at_ms: 2_000_000,
    };
    request.validate().unwrap();
    let resolve = ResolveRequest {
        schema_version: 1,
        operation_id: Uuid::from_u128(904),
        decision_id: request.decision_id,
        expected_decision_revision: 1,
        choice: Choice::Approve,
        comment: Some("Synthetic revisión ✓; no actual human authentication".into()),
    };
    resolve.validate().unwrap();
    let receipt = DecisionReceipt {
        schema_version: 1,
        decision_id: request.decision_id,
        decision_revision: 2,
        subject: subject.artifact().unwrap(),
        request_digest: digest(&request).unwrap(),
        eligibility_project_revision: request.eligibility_project_revision,
        review_policy: request.review_policy.clone(),
        recorded_at_ms: 1_000_001,
        finality: DecisionFinality::HumanAnswer {
            owner_user_id: request.eligible_owners[0],
            operation_id: resolve.operation_id,
            choice: resolve.choice,
            comment_digest: resolve.comment.as_ref().map(|c| Digest::of(c.as_bytes())),
        },
    };
    receipt.validate_for(&request).unwrap();
    (request, resolve, receipt)
}
fn check_receipt(subject: &ReviewSubject) -> CheckReceipt {
    let validator = reference(
        "aq.check.source-artifact-integrity",
        b"synthetic validator pin; no executor registration",
    );
    let check = CheckReceipt {
        schema_version: 1,
        operation_id: Uuid::from_u128(905),
        check_id: Text::new("source-integrity").unwrap(),
        subject: subject.artifact().unwrap(),
        validator: validator.clone(),
        recorded_at_ms: 1_000_002,
        disposition: CheckDisposition::Unknown,
        executor: ExecutorEvidence::Incomplete {
            definition: validator,
            evidence: vec![],
        },
    };
    check.validate_for_subject(subject).unwrap();
    check
}
