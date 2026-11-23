//! Attribute Graph writes to the AQ owner who submitted them.
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// QA N-04: Graph attributes every AQ write to the one configured mutation
/// identity. Show the AQ account whose operation created (operation ID = node
/// ID) or last verified the record instead; other attributions are unchanged.
pub async fn records(
    pool: &PgPool,
    scope: (Uuid, Uuid),
    service: Option<&str>,
    data: &mut Value,
) -> Result<(), sqlx::Error> {
    let mut found = Vec::new();
    collect_records(data, &mut found);
    attribute_tasks(pool, scope, &mut found).await?;
    if let Some(service) = service {
        attribute_service(pool, service, found).await?;
    }
    Ok(())
}

type Record = serde_json::Map<String, Value>;

fn collect_records<'a>(v: &'a mut Value, out: &mut Vec<&'a mut Record>) {
    match v {
        Value::Array(items) => items.iter_mut().for_each(|i| collect_records(i, out)),
        Value::Object(o) => {
            if o.contains_key("created_by") && o.get("id").is_some_and(Value::is_string) {
                out.push(o)
            } else {
                o.values_mut().for_each(|i| collect_records(i, out))
            }
        }
        _ => {}
    }
}

async fn attribute_tasks(
    pool: &PgPool,
    scope: (Uuid, Uuid),
    found: &mut [&mut Record],
) -> Result<(), sqlx::Error> {
    let tasks: Vec<Uuid> = found
        .iter()
        .filter(|node| node["kind"] == "task")
        .filter_map(|node| node["id"].as_str()?.parse().ok())
        .collect();
    if !tasks.is_empty() {
        let authors = crate::task_git_author::for_tasks(pool, scope.0, scope.1, &tasks).await?;
        for node in found.iter_mut().filter(|node| node["kind"] == "task") {
            let author = authors.iter().find(|(id, ..)| node["id"] == json!(id));
            if let Some((_, name, email, parent)) = author {
                node.insert("requested_by".into(), json!({"name":name,"email":email}));
                if parent.is_none() {
                    node.insert("created_by".into(), json!(email));
                }
            }
        }
    }
    Ok(())
}

async fn attribute_service(
    pool: &PgPool,
    service: &str,
    mut found: Vec<&mut Record>,
) -> Result<(), sqlx::Error> {
    found.retain(|o| {
        o.get("created_by").and_then(Value::as_str) == Some(service)
            || o.get("last_verified_by").and_then(Value::as_str) == Some(service)
    });
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
        let Some((_, creator, verifier)) = rows.iter().find(|row| o["id"] == json!(row.0)) else {
            continue;
        };
        if o["kind"] != "task" {
            replace_service(o, "created_by", service, creator.as_deref());
        }
        replace_service(o, "last_verified_by", service, verifier.as_deref());
    }
    Ok(())
}

fn replace_service(record: &mut Record, key: &str, service: &str, human: Option<&str>) {
    if let Some(email) = human.filter(|_| record.get(key).and_then(Value::as_str) == Some(service))
    {
        record.insert(key.into(), json!(email));
    }
}
/// QA P1-07: Graph names AQ's one mutation identity as every AQ write's actor.
/// An entry in a change set that an owner's journalled operation confirmed
/// names that owner's email when the retained receipt matches this project and workspace.
pub async fn history(
    pool: &PgPool,
    service: Option<&str>,
    scope: (Uuid, Uuid),
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
    let authors: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT c.cs,u.email FROM unnest($1::uuid[]) AS c(cs) JOIN users u ON u.id=COALESCE(
          (SELECT d.owner_id FROM dashboard_operations d JOIN dashboard_graph_intents i USING(operation_id)
           WHERE d.disposition='confirmed' AND i.project_id=$2 AND i.workspace_id=$3
             AND d.response->'receipt_refs' @> jsonb_build_array(jsonb_build_object('type','graph_change_set','id',c.cs::text)) LIMIT 1),
          (SELECT h.actor_id FROM human_graph_operations h WHERE h.project_id=$2 AND h.workspace_id=$3
             AND h.raw_receipt->>'change_set_id'=c.cs::text LIMIT 1))",
    ).bind(&sets).bind(scope.0).bind(scope.1).fetch_all(pool).await?;
    for (h, actor) in rows.iter().zip(out.iter_mut()).filter(|(h, _)| ours(h)) {
        if let Some((_, email)) = authors
            .iter()
            .find(|(id, _)| h["change_set_id"] == json!(id))
        {
            *actor = json!(email);
        }
    }
    Ok(out)
}
