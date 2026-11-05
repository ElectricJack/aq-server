use super::*;
use std::collections::{BTreeMap, BTreeSet};
fn rejected(request: &Value, code: &str) -> Result<Value> {
    let mut body = common(request)?;
    body["state"] = json!("rejected");
    body["effect"] = json!("not_applied");
    body["code"] = json!(code);
    body["retryable"] = json!(false);
    Ok(body)
}
async fn save_status(
    tx: &mut Transaction<'_, Postgres>,
    op: Uuid,
    phase: &str,
    body: &Value,
) -> Result<()> {
    sqlx::query("UPDATE agent_graph_operations SET phase=$2,status=$3 WHERE operation_id=$1")
        .bind(op)
        .bind(phase)
        .bind(body)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Draft reads materialize optional serde defaults in the selected graph v1.
/// Compare the exact ordered plan after removing only declared no-effect defaults;
/// never rewrite the stored request or accept a new non-default mutation field.
fn draft_operations_match(expected: &Value, actual: &Value) -> bool {
    let (Some(expected), Some(actual)) = (expected.as_array(), actual.as_array()) else {
        return false;
    };
    if expected.len() != actual.len() {
        return false;
    }
    for (wanted, got) in expected.iter().zip(actual) {
        let (Some(wanted_object), Some(_)) = (wanted.as_object(), got.as_object()) else {
            return false;
        };
        let mut normalized = got.clone();
        if wanted["op"] == "update_node" {
            let Some(patch) = normalized["patch"].as_object_mut() else {
                return false;
            };
            patch.retain(|key, value| wanted["patch"].get(key).is_some() || !value.is_null());
        }
        let extra: Vec<String> = normalized
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| !wanted_object.contains_key(*key))
            .cloned()
            .collect();
        for key in extra {
            let value = &normalized[&key];
            let default = match wanted["op"].as_str() {
                Some("create_node") => match key.as_str() {
                    "labels" => value == &json!([]),
                    "metadata" => value == &json!({}),
                    "content_type" | "knowledge_type" => value.is_null() || value == "note",
                    "verification" => value.is_null() || value == "unverified",
                    "valid_from" | "valid_until" | "review_at" => value.is_null(),
                    _ => false,
                },
                Some("add_edge") => {
                    matches!(
                        key.as_str(),
                        "description" | "source_revision" | "target_revision"
                    ) && value.is_null()
                }
                _ => false,
            };
            if !default {
                return false;
            }
            normalized.as_object_mut().unwrap().remove(&key);
        }
        if normalized != *wanted {
            return false;
        }
    }
    true
}
fn receipt(request: &Value, mapped: &Value, raw: &Value) -> Result<Value> {
    let workspace = &request["binding"]["workspace_id"];
    if mapped["class"] == "draft_create" {
        if !contract::upstream_valid("Draft", raw)
            || raw["workspace_id"] != *workspace
            || raw["actor_id"] != mapped["service"]["actor"]
            || raw["status"] != "draft"
            || !draft_operations_match(&mapped["payload"]["operations"], &raw["operations"])
            || raw["name"] != mapped["payload"]["name"]
            || !raw["result"].is_null()
            || !raw["applied_at"].is_null()
            || raw["reason"] != request["reason"]
        {
            return Err(Error("overloaded"));
        }
        return Ok(
            json!({"kind":"graph_draft","workspace_id":workspace,"draft_id":raw["id"],"proposal_digest":digest(&mapped["payload"])? ,"applied":false,"task_completion":false}),
        );
    }
    if !contract::upstream_valid("MutationResult", raw) {
        return Err(Error("overloaded"));
    }
    let results = raw["results"].as_array().ok_or(Error("overloaded"))?;
    let ops = mapped["payload"]["operations"]
        .as_array()
        .ok_or(Error("internal"))?;
    if results.len() != ops.len() {
        return Err(Error("overloaded"));
    }
    let mut requested = BTreeSet::new();
    let mut affected = BTreeSet::new();
    for c in request["commands"].as_array().unwrap() {
        requested.insert((
            if c["kind"] == "create_finding" || c["kind"] == "correct_record" {
                "node"
            } else {
                "edge"
            },
            c["id"].as_str().ok_or(Error("internal"))?,
        ));
        affected.insert(
            c.get("source")
                .unwrap_or(&c["id"])
                .as_str()
                .ok_or(Error("internal"))?,
        );
    }
    let mut versions = BTreeMap::new();
    for (op, value) in ops.iter().zip(results) {
        let edge = op["op"] == "add_edge" || op["op"] == "remove_edge";
        let kind = if edge { "edge" } else { "node" };
        if value["id"] != op["id"]
            || value["workspace_id"] != *workspace
            || (op["op"] == "create_node" && value["kind"] != op["kind"])
            || (op["op"] == "create_node" && value["created_by"] != mapped["service"]["actor"])
        {
            return Err(Error("overloaded"));
        }
        let id = value["id"].as_str().ok_or(Error("overloaded"))?;
        if requested.contains(&(kind, id)) {
            let version = value["version"]
                .as_i64()
                .filter(|v| *v > 0)
                .ok_or(Error("overloaded"))?
                .to_string();
            let revision = if edge {
                None
            } else {
                Some(
                    value["content_revision"]
                        .as_i64()
                        .filter(|v| *v > 0)
                        .ok_or(Error("overloaded"))?
                        .to_string(),
                )
            };
            versions.insert(
                (kind, id),
                json!({"kind":kind,"id":id,"version":version,"content_revision":revision}),
            );
        }
    }
    if versions.len() != requested.len() {
        return Err(Error("overloaded"));
    }
    Ok(
        json!({"kind":"graph_mutation","workspace_id":workspace,"change_set_id":raw["change_set_id"],"versions":versions.into_values().collect::<Vec<_>>(),"affected_record_ids":affected,"task_completion":false}),
    )
}
async fn post(rt: &Runtime, mapped: &Value) -> Result<(StatusCode, Value)> {
    let workspace = id(&mapped["service"], "workspace_id")?;
    let route = match mapped["class"].as_str() {
        Some("mutate") => "mutations",
        Some("draft_create") => "changesets",
        _ => return Err(Error("internal")),
    };
    let bytes = canonical(&mapped["payload"])?;
    post_body(rt, workspace, route, bytes).await
}
pub(super) async fn post_mutation_bytes(
    rt: &Runtime,
    workspace: Uuid,
    bytes: Vec<u8>,
) -> Result<(StatusCode, Value)> {
    post_body(rt, workspace, "mutations", bytes).await
}
async fn post_body(
    rt: &Runtime,
    workspace: Uuid,
    route: &str,
    bytes: Vec<u8>,
) -> Result<(StatusCode, Value)> {
    let url = rt
        .url
        .join(&format!("/v1/workspaces/{workspace}/{route}"))
        .map_err(|_| Error("internal"))?;
    let mut response = rt
        .client
        .post(url)
        .bearer_auth(&rt.token)
        .header("X-AQ-Graph-API-Version", "1")
        .header("content-type", "application/json")
        .body(bytes)
        .send()
        .await
        .map_err(|_| Error("overloaded"))?;
    if response
        .headers()
        .get("X-AQ-Graph-API-Version")
        .is_none_or(|v| v != "1")
        || response.content_length().is_some_and(|n| n > 1048576)
    {
        return Err(Error("overloaded"));
    }
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error("overloaded"))? {
        if body.len() + chunk.len() > 1048576 {
            return Err(Error("overloaded"));
        }
        body.extend_from_slice(&chunk);
    }
    // Graph metadata permits finite fractions. Keep duplicate-key rejection,
    // while leaving integer-only StrictValue at the agent request/hash boundary.
    let value = super::super::scheduling::parse::parse(&body).map_err(|_| Error("overloaded"))?;
    Ok((status, value))
}
pub(super) async fn run(state: &AppState, op: Uuid) -> Result<Value> {
    let initial = operation(state, op).await?;
    let attempt: Uuid = initial.get("attempt_id");
    let node: Uuid = initial.get("node_id");
    let mut tx = state.store.pool.begin().await?;
    // Initial dispatch races lifecycle cancellation in the normal authority lock
    // order. Retries reconcile exact admitted possible effects without live agent authority.
    let first = initial.get::<i32, _>("dispatch_count") == 0;
    let authority = if first {
        Some(lock_attempt(state, &mut tx, attempt, node).await)
    } else {
        None
    };
    let row = sqlx::query("SELECT * FROM agent_graph_operations WHERE operation_id=$1 FOR UPDATE")
        .bind(op)
        .fetch_one(&mut *tx)
        .await?;
    let phase: String = row.get("phase");
    let old: Value = row.get("status");
    if !matches!(phase.as_str(), "queued" | "unknown" | "dispatching") {
        tx.commit().await?;
        return Ok(old);
    }
    let request: Value = row.get("request");
    let mapped: Value = row.get("mapped");
    let policy: Value = row.get("policy");
    let count = row.get::<i32, _>("dispatch_count") as u64;
    let at = now(state);
    if row
        .get::<Option<i64>, _>("lease_expires_at_ms")
        .is_some_and(|until| until > at as i64)
    {
        tx.commit().await?;
        return Ok(old);
    }
    if count == 0 {
        let live = authority.unwrap_or(Err(Error("session_revoked")));
        let revoked: Option<bool> =
            sqlx::query_scalar("SELECT revoked FROM agent_launch_bindings WHERE attempt_id=$1")
                .bind(attempt)
                .fetch_optional(&mut *tx)
                .await?;
        if let Err(error) = live {
            if error.0 == "internal" {
                return Err(error);
            }
            let body = rejected(
                &request,
                if error.0 == "session_expired" {
                    "session_expired"
                } else {
                    "session_revoked"
                },
            )?;
            save_status(&mut tx, op, "rejected", &body).await?;
            tx.commit().await?;
            return Ok(body);
        }
        if revoked != Some(false) {
            let body = rejected(&request, "session_revoked")?;
            save_status(&mut tx, op, "rejected", &body).await?;
            tx.commit().await?;
            return Ok(body);
        }
    }
    let elapsed = at.saturating_sub(row.get::<i64, _>("created_at_ms") as u64);
    let mapping =
        runtime(state).and_then(|rt| authorize::identity(rt, &request["binding"]["workspace_id"]));
    let manual = mapping.is_err()
        || mapping.ok().as_ref() != Some(&mapped["service"])
        || count >= policy["max_attempts"].as_u64().unwrap_or(0)
        || elapsed >= policy["max_elapsed_ms"].as_u64().unwrap_or(0);
    if manual {
        let body = pending(&request, &policy, count > 0, count, elapsed, true)?;
        save_status(
            &mut tx,
            op,
            if count > 0 { "unknown" } else { "queued" },
            &body,
        )
        .await?;
        tx.commit().await?;
        return Ok(body);
    }
    if row.get::<i64, _>("next_retry_at_ms") > at as i64 {
        tx.commit().await?;
        return Ok(old);
    }
    let lease = Uuid::new_v4();
    let body = pending(&request, &policy, true, count + 1, elapsed, false)?;
    let delay = policy["initial_delay_ms"]
        .as_u64()
        .unwrap_or(250)
        .saturating_mul(1u64.checked_shl(count.min(8) as u32).unwrap_or(256))
        .min(policy["max_delay_ms"].as_u64().unwrap_or(30000));
    sqlx::query("UPDATE agent_graph_operations SET phase='dispatching',status=$2,dispatch_count=dispatch_count+1,first_dispatch_at_ms=COALESCE(first_dispatch_at_ms,$3),next_retry_at_ms=$4,lease_id=$5,lease_expires_at_ms=$6 WHERE operation_id=$1").bind(op).bind(&body).bind(at as i64).bind(at.saturating_add(delay) as i64).bind(lease).bind(at.saturating_add(15000) as i64).execute(&mut *tx).await?;
    // Recheck time after any row-lock wait before the first possible send.
    if count == 0 {
        lock_attempt(state, &mut tx, attempt, node).await?;
    }
    tx.commit().await?;
    let outcome = post(runtime(state)?, &mapped).await;
    let mut next = pending(
        &request,
        &policy,
        true,
        count + 1,
        now(state).saturating_sub(row.get::<i64, _>("created_at_ms") as u64),
        false,
    )?;
    let mut next_phase = "unknown";
    if let Ok((code, raw)) = outcome {
        if code.is_success() {
            if let Ok(receipt) = receipt(&request, &mapped, &raw) {
                next = common(&request)?;
                next["state"] = json!("confirmed");
                next["effect"] = json!("confirmed");
                next["receipt"] = receipt;
                next_phase = "confirmed";
            }
        } else if code == StatusCode::CONFLICT && raw["error"]["code"] == "conflict" {
            next = common(&request)?;
            next["state"] = json!("conflict");
            next["effect"] = json!("not_applied");
            next["conflicts"] =
                json!([{"kind":"version","subject_id":null,"expected":null,"current":null}]);
            next["retryable"] = json!(false);
            next_phase = "conflict";
        } else if matches!(code.as_u16(), 400 | 401 | 403 | 404 | 422)
            && raw["error"]["code"].is_string()
            && count == 0
        {
            // After an earlier unknown send, a new credential/configuration fault
            // cannot prove the old request was not applied.
            next = rejected(&request, "graph_rejected")?;
            next_phase = "rejected";
        }
    }
    if !contract::valid("OperationStatus", &next) {
        return Err(Error("internal"));
    }
    let mut tx = state.store.pool.begin().await?;
    let winner = sqlx::query(
        "SELECT lease_id,status FROM agent_graph_operations WHERE operation_id=$1 FOR UPDATE",
    )
    .bind(op)
    .fetch_one(&mut *tx)
    .await?;
    if winner.get::<Option<Uuid>, _>("lease_id") != Some(lease) {
        let body = winner.get("status");
        tx.commit().await?;
        return Ok(body);
    }
    sqlx::query("UPDATE agent_graph_operations SET phase=$2,status=$3,lease_id=NULL,lease_expires_at_ms=NULL WHERE operation_id=$1").bind(op).bind(next_phase).bind(&next).execute(&mut *tx).await?;
    if next_phase == "confirmed" {
        sqlx::query("INSERT INTO agent_graph_outbox(event_id,operation_id,kind,payload) VALUES($1,$2,'graph.operation_confirmed',$3) ON CONFLICT(operation_id) DO NOTHING").bind(Uuid::new_v4()).bind(op).bind(json!({"operation_id":op,"binding":request["binding"],"receipt":next["receipt"]})).execute(&mut *tx).await?;
        if mapped["class"] == "mutate" {
            for id in mapped["finding_ids"].as_array().ok_or(Error("internal"))? {
                sqlx::query("INSERT INTO agent_created_records(attempt_id,record_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(attempt).bind(id.as_str().and_then(|s|Uuid::parse_str(s).ok()).ok_or(Error("internal"))?).execute(&mut *tx).await?;
            }
        }
    }
    tx.commit().await?;
    Ok(next)
}

/// A storage failure after dispatch reservation cannot claim no graph effect.
/// Callers supply the immutable intent they have already durably observed.
pub(super) async fn recoverable_run(
    state: &AppState,
    op: Uuid,
    request: &Value,
    policy: &Value,
) -> Result<Value> {
    match run(state, op).await {
        Ok(body) => Ok(body),
        Err(_) => pending(
            request,
            policy,
            true,
            policy["max_attempts"].as_u64().unwrap_or(0),
            policy["max_elapsed_ms"].as_u64().unwrap_or(0),
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn mutation_response_accepts_fractions_but_rejects_duplicate_keys() {
        use axum::{Router, routing::post};
        for (body, valid) in [
            (r#"{"results":[{"metadata":{"weight":0.25}}]}"#, true),
            (
                r#"{"results":[{"metadata":{"weight":0.25,"weight":1}}]}"#,
                false,
            ),
            (r#"{"results":[],"results":[]}"#, false),
            (r#"{"results":[{"metadata":{"weight":1e999}}]}"#, false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new().route(
                        "/v1/workspaces/{workspace}/mutations",
                        post(move || async move { ([("X-AQ-Graph-API-Version", "1")], body) }),
                    ),
                )
                .await
                .unwrap();
            });
            let rt = Runtime {
                mutation_actor: Some("fixture".into()),
                client: reqwest::Client::new(),
                url: format!("http://{address}/").parse().unwrap(),
                token: "fixture-token".into(),
                ttl: 1000,
            };
            let result = post_mutation_bytes(&rt, Uuid::new_v4(), b"{}".to_vec()).await;
            assert_eq!(result.is_ok(), valid, "{body}");
            if valid {
                assert_eq!(result.unwrap().1["results"][0]["metadata"]["weight"], 0.25);
            }
            task.abort();
        }
    }
    #[test]
    fn native_draft_defaults_preserve_exact_plan_but_extra_meaning_is_refused() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/native-draft-normalization.json"
        ))
        .unwrap();
        assert!(contract::upstream_valid("Draft", &fixture["response"]));
        assert!(draft_operations_match(
            &fixture["expected_operations"],
            &fixture["response"]["operations"]
        ));
        for (field, value) in [
            ("verification", json!("verified")),
            ("metadata", json!({"unexpected":"meaning"})),
            ("title", json!("different task")),
        ] {
            let mut response = fixture["response"]["operations"].clone();
            response[2][field] = value;
            assert!(!draft_operations_match(
                &fixture["expected_operations"],
                &response
            ));
        }
        let mut response = fixture["response"]["operations"].clone();
        response[0]["expected_version"] = json!(4);
        assert!(!draft_operations_match(
            &fixture["expected_operations"],
            &response
        ));
    }
}
