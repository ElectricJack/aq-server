use super::*;

pub(super) fn rework(log: &[(String, Value)]) {
    let work: Vec<_> = log
        .iter()
        .filter(|(c, _)| c == "work.ensure")
        .map(|(_, v)| v)
        .collect();
    let kinds: Vec<_> = work.iter().map(|v| v["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["implementation", "review", "repair", "review"]);
    // The rework attempt carries the rejecting review's evidence reference.
    assert_eq!(work[2]["context"], "aq.review.decision/r1");
    assert_eq!(work[2]["logical_key"], "task-1/a1/work");
    // Review is cross-family and keyed per attempt.
    assert!(
        work[1]["context"]
            .as_str()
            .unwrap()
            .contains("reviewer_family=openai")
    );
    assert_ne!(work[1]["logical_key"], work[3]["logical_key"]);
    // The repaired change is a new candidate: checks run again on it.
    let checks: Vec<_> = log
        .iter()
        .filter(|(c, _)| c == "check.request")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(checks.len(), 2);
    assert_ne!(checks[0]["subject"], checks[1]["subject"]);
    // Each check binds the prepared candidate's source, not the request's.
    assert_eq!(checks[0]["source"], candidate_source(1));
    assert_eq!(checks[1]["source"], candidate_source(2));
    // Publish only once, with the check receipt and the review as evidence.
    let publish: Vec<_> = log
        .iter()
        .filter(|(c, _)| c == "candidate.publish")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(publish.len(), 1);
    let evidence: Vec<_> = publish[0]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(evidence, ["check-receipt:2", "aq.review.decision/r2"]);
    assert_eq!(publish[0]["candidate"], checks[1]["subject"]);
}

pub(super) fn decision_gate(log: &[(String, Value)]) {
    let steps: Vec<String> = log
        .iter()
        .filter(|(c, _)| c != "check.request" && c != "candidate.prepare")
        .map(|(c, v)| match c.as_str() {
            "work.ensure" => v["kind"].as_str().unwrap().to_string(),
            other => other.to_string(),
        })
        .collect();
    assert_eq!(
        steps,
        [
            "implementation",
            "review",
            "repair",
            "review",
            "decision.request",
            "repair",
            "review",
            "decision.request",
            "candidate.publish",
        ]
    );
    let of = |name: &str| -> Vec<Value> {
        log.iter()
            .filter(|(c, _)| c == name)
            .map(|(_, v)| v.clone())
            .collect()
    };
    let (decisions, reviews) = (of("decision.request"), of("work.ensure"));
    // Each decision is on the reviewed candidate, with the policy's question.
    assert_eq!(decisions[0]["subject"], reviews[3]["subject"]);
    assert_eq!(decisions[1]["subject"], reviews[5]["subject"]);
    assert_eq!(decisions[0]["question"], "Ship this reviewed change?");
    assert_eq!(decisions[0]["responders"], json!(["user:owner"]));
    assert_eq!(decisions[0]["choices"], json!(["approved", "rejected"]));
    assert_eq!(decisions[0]["expires_at_ms"], 86_400_000);
    // The human rejection is the next repair's evidence.
    assert_eq!(reviews[4]["context"], "aq.decision/d1");
    // Publish carries the check receipt, the review and the decision.
    let publish = of("candidate.publish");
    assert_eq!(publish.len(), 1);
    let evidence: Vec<_> = publish[0]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        evidence,
        ["check-receipt:3", "aq.review.decision/r3", "aq.decision/d2"]
    );
    assert_eq!(publish[0]["candidate"], decisions[1]["subject"]);
}
