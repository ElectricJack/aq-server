//! Optional Graph first-ready evidence, outside scheduling's node identity.
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub(super) struct Evidence {
    task: Uuid,
    at_ms: i64,
    class: Option<String>,
}

/// Remove the opt-in fields before comparing the ready node with its context.
/// The caller still validates the full node page and workspace before writing.
pub(super) fn take(page: &mut Value, received_ms: u64) -> Vec<Evidence> {
    page.get_mut("items")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let node = node.as_object_mut()?;
            let at = node.remove("first_ready_at");
            let class = node.remove("first_ready_work_class");
            let at_ms = parse_ms(&at?)?;
            let created_ms = parse_ms(node.get("created_at")?)?;
            if at_ms <= 0 || at_ms < created_ms || (at_ms as u64) > received_ms {
                return None;
            }
            let task = node.get("id")?.as_str()?.parse().ok()?;
            let class = match class {
                None => None,
                Some(class) => {
                    let class = class.as_str()?;
                    if !(1..=128).contains(&class.chars().count())
                        || class.chars().any(char::is_control)
                    {
                        return None;
                    }
                    Some(class.to_owned())
                }
            };
            Some(Evidence { task, at_ms, class })
        })
        .collect()
}

fn parse_ms(value: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|at| at.timestamp_millis())
}

pub(super) async fn record(
    pool: &PgPool,
    project: Uuid,
    workspace: Uuid,
    evidence: Vec<Evidence>,
) -> sqlx::Result<()> {
    if evidence.is_empty() {
        return Ok(());
    }
    let tasks: Vec<_> = evidence.iter().map(|row| row.task).collect();
    let times: Vec<_> = evidence.iter().map(|row| row.at_ms).collect();
    let classes: Vec<_> = evidence.into_iter().map(|row| row.class).collect();
    sqlx::query(
        "INSERT INTO performance_task_first_ready(project_id, task_id, ready_at_ms, work_class)
         SELECT $1, evidence.task_id, evidence.ready_at_ms, evidence.work_class
         FROM unnest($3::uuid[], $4::bigint[], $5::text[]) AS evidence(task_id, ready_at_ms, work_class)
         CROSS JOIN LATERAL (
             SELECT record->'binding'->>'graph_workspace_id' AS graph_workspace
             FROM project_execution_binding_history
             WHERE project_id=$1 AND created_at_ms <= evidence.ready_at_ms
             ORDER BY created_at_ms DESC, revision DESC
             LIMIT 1
         ) intake_binding
         WHERE intake_binding.graph_workspace = $2
         ON CONFLICT (project_id, task_id) DO NOTHING",
    )
    .bind(project)
    .bind(workspace.to_string())
    .bind(tasks)
    .bind(times)
    .bind(classes)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_explicit_credible_graph_clocks_survive_and_node_identity_is_preserved() {
        let id = Uuid::new_v4();
        let original = json!({
            "id": id,
            "created_at": "2026-11-24T09:00:00Z",
            "metadata": {"work_class": "current"}
        });
        let receive = parse_ms(&json!("2026-11-24T09:05:00Z")).unwrap() as u64;
        let mut legacy = json!({"items": [original.clone()]});
        assert!(take(&mut legacy, receive).is_empty());
        for malformed in ["unexpected", "2026-11-24T09:06:00Z", "2026-11-24T08:59:59Z"] {
            let mut node = original.clone();
            node["first_ready_at"] = json!(malformed);
            let mut page = json!({"items": [node]});
            assert!(take(&mut page, receive).is_empty());
            assert_eq!(page, legacy);
        }
        let mut node = original.clone();
        node["first_ready_at"] = json!("2026-11-24T09:02:00Z");
        node["first_ready_work_class"] = json!("intake");
        let mut page = json!({"items": [node]});
        let evidence = take(&mut page, receive);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].task, id);
        assert_eq!(evidence[0].at_ms, receive as i64 - 180_000);
        assert_eq!(evidence[0].class.as_deref(), Some("intake"));
        assert_eq!(page, legacy);
    }
}
