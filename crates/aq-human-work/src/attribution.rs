//! Attribute Graph writes to the AQ owner who submitted them.
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// QA N-04: Graph attributes every AQ write to the one configured mutation
/// identity. Show the AQ account whose operation created (operation ID = node
/// ID) or last verified the record instead; other attributions are unchanged.
pub async fn records(
    pool: &PgPool,
    service: Option<&str>,
    data: &mut Value,
) -> Result<(), sqlx::Error> {
    fn records<'a>(v: &'a mut Value, out: &mut Vec<&'a mut serde_json::Map<String, Value>>) {
        match v {
            Value::Array(items) => items.iter_mut().for_each(|i| records(i, out)),
            Value::Object(o) => {
                if o.contains_key("created_by") && o.get("id").is_some_and(Value::is_string) {
                    out.push(o)
                } else {
                    o.values_mut().for_each(|i| records(i, out))
                }
            }
            _ => {}
        }
    }
    let Some(service) = service else {
        return Ok(());
    };
    let mut found = Vec::new();
    records(data, &mut found);
    let by_service =
        |o: &serde_json::Map<String, Value>, k: &str| o.get(k) == Some(&json!(service));
    found.retain(|o| by_service(o, "created_by") || by_service(o, "last_verified_by"));
    let ids: Vec<Uuid> = found
        .iter()
        .filter_map(|o| o["id"].as_str()?.parse().ok())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(Uuid, Option<String>, Option<String>)> = sqlx::query_as("SELECT n.id,(SELECT u.email FROM users u WHERE u.id=COALESCE((SELECT d.owner_id FROM dashboard_operations d WHERE d.operation_id=n.id),(SELECT h.actor_id FROM human_graph_operations h WHERE h.operation_id=n.id))),(SELECT u.email FROM dashboard_operations d JOIN users u ON u.id=d.owner_id WHERE d.kind='knowledge.verify' AND d.disposition IN ('confirmed','unknown') AND d.response->'subject_refs'->0->>'id'=n.id::text ORDER BY d.updated_at DESC LIMIT 1) FROM unnest($1::uuid[]) AS n(id)")
        .bind(&ids).fetch_all(pool).await?;
    for o in found {
        let row = rows.iter().find(|row| o["id"] == json!(row.0));
        for (key, human) in [
            ("created_by", row.and_then(|r| r.1.as_ref())),
            ("last_verified_by", row.and_then(|r| r.2.as_ref())),
        ] {
            if let Some(email) = human.filter(|_| by_service(o, key)) {
                o.insert(key.into(), json!(email));
            }
        }
    }
    Ok(())
}
/// QA P1-07: Graph names AQ's one mutation identity as every AQ write's actor.
/// An entry in a change set that an owner's journalled operation confirmed
/// (dashboard action or task create), or the creation of a task an owner's
/// operation created (#97), names that owner's email instead.
pub async fn history(
    pool: &PgPool,
    service: Option<&str>,
    task: Uuid,
    rows: &[Value],
) -> Result<Vec<Value>, sqlx::Error> {
    let mut out: Vec<Value> = rows.iter().map(|h| h["actor_id"].clone()).collect();
    let Some(service) = service else {
        return Ok(out);
    };
    let ours = |h: &Value| h["actor_id"] == json!(service);
    let sets: Vec<Uuid> = rows
        .iter()
        .filter(|h| ours(h))
        .filter_map(|h| h["change_set_id"].as_str()?.parse().ok())
        .collect();
    if sets.is_empty() {
        return Ok(out);
    }
    let found: Vec<(Option<Uuid>, String)> = sqlx::query_as("SELECT c.cs,u.email FROM unnest($1::uuid[]) AS c(cs) JOIN users u ON u.id=COALESCE((SELECT d.owner_id FROM dashboard_operations d WHERE d.disposition='confirmed' AND d.response->'receipt_refs' @> jsonb_build_array(jsonb_build_object('type','graph_change_set','id',c.cs::text)) LIMIT 1),(SELECT h.actor_id FROM human_graph_operations h WHERE h.raw_receipt->>'change_set_id'=c.cs::text LIMIT 1)) UNION ALL SELECT NULL,u.email FROM users u WHERE u.id=COALESCE((SELECT d.owner_id FROM dashboard_operations d WHERE d.operation_id=$2),(SELECT h.actor_id FROM human_graph_operations h WHERE h.operation_id=$2))")
        .bind(&sets).bind(task).fetch_all(pool).await?;
    for (h, actor) in rows.iter().zip(out.iter_mut()).filter(|(h, _)| ours(h)) {
        let set = found
            .iter()
            .find(|f| f.0.is_some() && h["change_set_id"] == json!(f.0));
        let created = found
            .iter()
            .find(|f| f.0.is_none() && h["operation"] == "created" && h["category"] != "edge");
        if let Some((_, email)) = set.or(created) {
            *actor = json!(email);
        }
    }
    Ok(out)
}
