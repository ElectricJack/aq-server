//! A read-only review has an explicit assignment and a verdict in its own
//! immutable result report. The reviewed result and its central Git head must
//! belong to a different task in the same project.
use crate::{Result, delivery_store::Latest};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub(crate) fn designated(node: &Value) -> bool {
    node["metadata"]["aq:task_type"] == "review-only"
        || node["labels"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|label| label == "review-only"))
}

fn target(body: &str) -> Option<(Uuid, &str)> {
    let mut pins = body
        .lines()
        .filter_map(|line| line.strip_prefix("Review target: "));
    let (id, revision) = pins.next()?.split_once('@')?;
    if pins.next().is_some()
        || revision.len() != 64
        || !revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some((id.parse().ok()?, revision))
}

fn verdict(conclusion: &str, report: Uuid, revision: &str, head: &str) -> Option<&'static str> {
    let rest = conclusion.strip_prefix("Outcome: done; Changes: none; Copied: none; ")?;
    let (runner, message) = rest.split_once("; Result: Review verdict: ")?;
    if !matches!(runner, "Claude exit 0" | "Codex exit 0") {
        return None;
    }
    let (decision, evidence) = message.split_once(' ')?;
    let mut words = evidence.split_whitespace();
    if words.next()? != format!("{report}@{revision}") || words.next()? != head {
        return None;
    }
    match decision {
        "approved" => Some("review_approved"),
        "changes_requested" => Some("review_changes_requested"),
        _ => None,
    }
}

pub(crate) async fn outcome(
    db: &mut PgConnection,
    node: &Value,
    task: Uuid,
    version: i64,
    latest: &Latest,
) -> Result<&'static str> {
    let incomplete = "review_incomplete";
    let Some((report, revision)) = node["body"].as_str().and_then(target) else {
        return Ok(incomplete);
    };
    if latest.outcome != "success" || latest.changed || latest.head.is_empty() {
        return Ok(incomplete);
    }
    let evidence: Option<(String, String)> = sqlx::query_as(
        "SELECT convert_from(r.report_bytes,'UTF8')::jsonb->>'conclusion', p.head
         FROM profiled_result_reports r JOIN profiled_launches l USING(attempt_id)
         JOIN result_revision_pins target ON target.report_id=$1 AND target.report_revision=$2
         JOIN delivery_pushed_results p ON p.report_id=target.report_id AND p.head IS NOT NULL
         WHERE r.report_id=$3 AND r.attempt_id=$4 AND target.project_id=$5 AND target.task_id<>$6
           AND (l.execution_spec#>>'{task,version}')::bigint=$7",
    )
    .bind(report)
    .bind(revision)
    .bind(latest.result_id)
    .bind(latest.attempt_id)
    .bind(latest.project_id)
    .bind(task)
    .bind(version)
    .fetch_optional(db)
    .await?;
    Ok(evidence
        .and_then(|(summary, head)| verdict(&summary, report, revision, &head))
        .unwrap_or(incomplete))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_one_pin_and_an_explicit_verdict_on_the_same_source() {
        let report = Uuid::new_v4();
        let revision = "1".repeat(64);
        let head = "2".repeat(40);
        let assignment = format!("Review target: {report}@{revision}");
        assert_eq!(target(&assignment), Some((report, revision.as_str())));
        assert!(target(&format!("{assignment}\n{assignment}")).is_none());
        let summary = format!(
            "Outcome: done; Changes: none; Copied: none; Codex exit 0; Result: Review verdict: approved {report}@{revision} {head} Tests and findings follow."
        );
        assert_eq!(
            verdict(&summary, report, &revision, &head),
            Some("review_approved")
        );
        assert_eq!(
            verdict(
                &summary.replace("approved", "changes_requested"),
                report,
                &revision,
                &head
            ),
            Some("review_changes_requested")
        );
        assert!(verdict(&summary, report, &revision, &"3".repeat(40)).is_none());
        assert!(
            verdict(
                &summary.replace("done", "blocked"),
                report,
                &revision,
                &head
            )
            .is_none()
        );
        assert!(
            verdict(
                &summary.replace("approved", "pending"),
                report,
                &revision,
                &head
            )
            .is_none()
        );
    }
}
