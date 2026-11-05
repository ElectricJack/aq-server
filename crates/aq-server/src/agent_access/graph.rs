use super::*;
fn coverage(reason: Option<&str>) -> Value {
    json!({"status":if reason.is_some(){"partial"}else{"complete"},"complete_for":"authorized_projection","reasons":reason.into_iter().collect::<Vec<_>>()})
}
fn list<'a>(v: &'a Value, k: &str) -> Result<&'a Vec<Value>> {
    v[k].as_array().ok_or(Error("overloaded"))
}
fn projection(raw: &Value, fields: &[&str]) -> Value {
    let mut map = serde_json::Map::new();
    for &k in fields {
        map.insert(k.to_owned(), raw[k].clone());
    }
    Value::Object(map)
}
fn node(p: &Principal, raw: &Value) -> Result<Value> {
    if raw["workspace_id"] != p.binding["workspace_id"] {
        return Err(Error("overloaded"));
    }
    p.require(&raw["id"])?;
    let mut n = projection(
        raw,
        &[
            "id",
            "kind",
            "title",
            "body",
            "acceptance_criteria",
            "labels",
            "task_state",
            "verification",
            "content_type",
            "retired",
            "deleted",
        ],
    );
    for k in ["version", "content_revision"] {
        n[k] = json!(
            raw[k]
                .as_i64()
                .filter(|i| *i > 0)
                .ok_or(Error("overloaded"))?
                .to_string()
        );
    }
    if !contract::valid("ScopedNode", &n) {
        return Err(Error("overloaded"));
    }
    Ok(n)
}
fn edge(p: &Principal, raw: &Value) -> Result<Option<Value>> {
    if raw["workspace_id"] != p.binding["workspace_id"] {
        return Err(Error("overloaded"));
    }
    if !p.readable(&raw["source"])
        || !p.readable(&raw["target"])
        || !p.profile["read_scope"]["edge_types"]
            .as_array()
            .is_some_and(|a| a.contains(&raw["edge_type"]))
    {
        return Ok(None);
    }
    let mut e = projection(raw, &["id", "source", "target", "edge_type", "description"]);
    for k in ["version", "source_revision", "target_revision"] {
        e[k] = if raw[k].is_null() && k != "version" {
            Value::Null
        } else {
            json!(
                raw[k]
                    .as_i64()
                    .filter(|i| *i > 0)
                    .ok_or(Error("overloaded"))?
                    .to_string()
            )
        };
    }
    if !contract::valid("ScopedEdge", &e) {
        return Err(Error("overloaded"));
    }
    Ok(Some(e))
}
pub(super) fn validate_launch(p: &Principal) -> Result<()> {
    for id in list(&p.context, "root_ids")? {
        p.require(id)?;
    }
    for k in ["nodes", "pinned_revisions"] {
        for n in list(&p.context, k)? {
            p.require(&n["id"])?;
        }
    }
    for e in list(&p.context, "edges")? {
        p.require(&e["source"])?;
        p.require(&e["target"])?;
        if !p.profile["read_scope"]["edge_types"]
            .as_array()
            .is_some_and(|a| a.contains(&e["edge_type"]))
        {
            return Err(Error("scope_denied"));
        }
    }
    if canonical(&p.context)?.len() > p.profile["max_response_bytes"].as_u64().unwrap_or(0) as usize
    {
        return Err(Error("response_too_large"));
    }
    Ok(())
}
impl Runtime {
    pub(super) async fn get(&self, path: &str, params: &[(String, String)]) -> Result<Value> {
        let url = self.url.join(path).map_err(|_| Error("internal"))?;
        let mut response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("X-AQ-Graph-API-Version", "1")
            .query(params)
            .send()
            .await
            .map_err(|_| Error("overloaded"))?;
        if response
            .headers()
            .get("X-AQ-Graph-API-Version")
            .is_none_or(|v| v != "1")
        {
            return Err(Error("overloaded"));
        }
        if response.status() == StatusCode::NOT_FOUND {
            return Err(Error("not_found"));
        }
        if !response.status().is_success() {
            return Err(Error("overloaded"));
        }
        if response.content_length().is_some_and(|n| n > 1_048_576) {
            return Err(Error("response_too_large"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error("overloaded"))? {
            if bytes.len() + chunk.len() > 1_048_576 {
                return Err(Error("response_too_large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error("overloaded"))
    }
    async fn get_node(&self, p: &Principal, id: &Value) -> Result<Value> {
        // UUID validation before interpolation prevents graph route selection.
        let uuid = id
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(Error("overloaded"))?;
        p.require(id)?;
        let raw = self
            .get(
                &format!(
                    "/v1/workspaces/{}/nodes/{uuid}",
                    p.binding["workspace_id"]
                        .as_str()
                        .ok_or(Error("internal"))?
                ),
                &[],
            )
            .await?;
        if raw["id"] != *id {
            return Err(Error("overloaded"));
        }
        node(p, &raw)
    }
    async fn checked_edges(
        &self,
        p: &Principal,
        raw: &[Value],
        root: Option<&Value>,
    ) -> Result<(Vec<Value>, bool)> {
        let mut endpoints = std::collections::BTreeSet::new();
        let mut result = Vec::new();
        let mut partial = false;
        for raw in raw {
            let Some(e) = edge(p, raw)? else { continue };
            if root.is_some_and(|id| e["source"] != *id && e["target"] != *id) {
                return Err(Error("overloaded"));
            }
            for k in ["source", "target"] {
                if endpoints.len() >= 64
                    && !endpoints.contains(e[k].as_str().ok_or(Error("overloaded"))?)
                {
                    partial = true;
                    break;
                }
                let id = e[k].as_str().ok_or(Error("overloaded"))?.to_owned();
                if endpoints.insert(id) {
                    self.get_node(p, &e[k]).await?;
                }
            }
            if partial {
                break;
            }
            result.push(e);
        }
        Ok((result, partial))
    }
}
fn query_key(q: &Value) -> Result<String> {
    let mut q = q.clone();
    if q.get("cursor").is_some() {
        q["cursor"] = Value::Null;
    }
    digest(&q)
}
async fn cursor_load(state: &AppState, p: &Principal, q: &Value) -> Result<Option<String>> {
    let Some(cursor) = q["cursor"].as_str() else {
        return Ok(None);
    };
    let result:Option<String>=sqlx::query_scalar("SELECT upstream_cursor FROM agent_read_cursors WHERE verifier=$1 AND attempt_id=$2 AND credential_version=$3 AND authority_revision=$4 AND query_digest=$5 AND expires_at_ms>$6").bind(crate::tokens::verifier(cursor)).bind(id(&p.binding,"attempt_id")?).bind(p.version).bind(p.revision).bind(query_key(q)?).bind(now(state) as i64).fetch_optional(&state.store.pool).await?;
    result.map(Some).ok_or(Error("cursor_invalid"))
}
async fn cursor_save(
    state: &AppState,
    p: &Principal,
    q: &Value,
    upstream: Option<String>,
) -> Result<Value> {
    let Some(upstream) = upstream else {
        return Ok(Value::Null);
    };
    if upstream.len() > 8192 {
        return Err(Error("overloaded"));
    }
    let cursor = secret();
    // A bounded per-attempt cursor cache prevents unbounded server-side token storage.
    let mut tx = state.store.pool.begin().await?;
    sqlx::query("SELECT attempt_id FROM agent_launch_bindings WHERE attempt_id=$1 FOR UPDATE")
        .bind(id(&p.binding, "attempt_id")?)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM agent_read_cursors WHERE attempt_id=$1 AND expires_at_ms<=$2")
        .bind(id(&p.binding, "attempt_id")?)
        .bind(now(state) as i64)
        .execute(&mut *tx)
        .await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_read_cursors WHERE attempt_id=$1")
            .bind(id(&p.binding, "attempt_id")?)
            .fetch_one(&mut *tx)
            .await?;
    if count >= 128 {
        return Err(Error("overloaded"));
    }
    sqlx::query("INSERT INTO agent_read_cursors(verifier,attempt_id,credential_version,authority_revision,query_digest,upstream_cursor,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(crate::tokens::verifier(&cursor)).bind(id(&p.binding,"attempt_id")?).bind(p.version).bind(p.revision).bind(query_key(q)?).bind(upstream).bind(p.expires as i64).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(json!(cursor))
}
pub(super) async fn read(state: &AppState, p: &Principal, q: &Value) -> Result<Value> {
    let rt = runtime(state)?;
    let workspace = p.binding["workspace_id"]
        .as_str()
        .ok_or(Error("internal"))?;
    let base = format!("/v1/workspaces/{workspace}");
    let kind = q["kind"].as_str().ok_or(Error("invalid_request"))?;
    match kind {
        "get" => Ok(json!({"kind":"get","node":rt.get_node(p,&q["node_id"]).await?})),
        "revision" => {
            let raw = rt
                .get(
                    &format!(
                        "{base}/nodes/{}/revisions/{}",
                        q["node_id"].as_str().unwrap(),
                        q["revision"].as_str().unwrap()
                    ),
                    &[],
                )
                .await?;
            if raw["node"]["id"] != q["node_id"]
                || raw["node"]["content_revision"]
                    .as_i64()
                    .map(|i| i.to_string())
                    .as_deref()
                    != q["revision"].as_str()
            {
                return Err(Error("overloaded"));
            }
            let n = node(p, &raw["node"])?;
            let available = raw["relationships_available"]
                .as_bool()
                .ok_or(Error("overloaded"))?;
            let (links, partial) = if available {
                let (e, partial) = rt
                    .checked_edges(p, list(&raw, "links")?, Some(&q["node_id"]))
                    .await?;
                (json!(e), partial)
            } else {
                if !raw["links"].is_null() {
                    return Err(Error("overloaded"));
                }
                (Value::Null, false)
            };
            Ok(
                json!({"kind":"revision","node":n,"links":links,"relationships_available":available,"coverage":coverage(partial.then_some("scan_budget"))}),
            )
        }
        "context" | "relationships" => {
            if kind == "relationships"
                && !list(q, "edge_types")?.iter().all(|t| {
                    p.profile["read_scope"]["edge_types"]
                        .as_array()
                        .is_some_and(|a| a.contains(t))
                })
            {
                return Err(Error("scope_denied"));
            }
            let depth = if kind == "relationships" {
                1
            } else {
                q["depth"].as_u64().unwrap()
            };
            let raw = rt
                .get(
                    &format!("{base}/nodes/{}/context", q["node_id"].as_str().unwrap()),
                    &[
                        ("depth".into(), depth.to_string()),
                        ("direction".into(), q["direction"].as_str().unwrap().into()),
                        ("limit".into(), "200".into()),
                        ("max_bytes".into(), "1048576".into()),
                    ],
                )
                .await?;
            if raw["node"]["id"] != q["node_id"] {
                return Err(Error("overloaded"));
            }
            let root = node(p, &raw["node"])?;
            for k in ["nodes", "pinned_revisions"] {
                for n in list(&raw, k)? {
                    if n["workspace_id"] != p.binding["workspace_id"] {
                        return Err(Error("overloaded"));
                    }
                }
            }
            let mut partial = raw["truncated"].as_bool().ok_or(Error("overloaded"))?;
            let mut nodes = Vec::new();
            let max = q["max_nodes"].as_u64().unwrap_or(65).saturating_sub(1) as usize;
            for n in list(&raw, "nodes")? {
                if p.readable(&n["id"]) {
                    if nodes.len() == max {
                        partial = true;
                        break;
                    }
                    nodes.push(node(p, n)?);
                }
            }
            let returned: Vec<Value> = std::iter::once(root["id"].clone())
                .chain(nodes.iter().map(|n| n["id"].clone()))
                .collect();
            let (all_edges, scan_partial) = rt.checked_edges(p, list(&raw, "edges")?, None).await?;
            partial |= scan_partial;
            let mut edges = Vec::new();
            let edge_max = q["max_edges"].as_u64().or(q["limit"].as_u64()).unwrap() as usize;
            for e in all_edges {
                let allowed = if kind == "context" {
                    returned.contains(&e["source"]) && returned.contains(&e["target"])
                } else {
                    list(q, "edge_types")?.contains(&e["edge_type"])
                        && match q["direction"].as_str().unwrap() {
                            "incoming" => e["target"] == q["node_id"],
                            "outgoing" => e["source"] == q["node_id"],
                            _ => e["source"] == q["node_id"] || e["target"] == q["node_id"],
                        }
                };
                if allowed {
                    if edges.len() == edge_max {
                        partial = true;
                        break;
                    }
                    edges.push(e);
                }
            }
            if kind == "relationships" {
                if !q["cursor"].is_null() {
                    return Err(Error("cursor_invalid"));
                }
                return Ok(
                    json!({"kind":"relationships","edges":edges,"next_cursor":null,"coverage":coverage(partial.then_some("scan_budget"))}),
                );
            }
            let mut pins = Vec::new();
            for n in list(&raw, "pinned_revisions")? {
                if p.readable(&n["id"])
                    && edges.iter().any(|e| {
                        ["source", "target"].iter().any(|k| {
                            e[*k] == n["id"]
                                && e[format!("{k}_revision")].as_str()
                                    == n["content_revision"]
                                        .as_i64()
                                        .map(|i| i.to_string())
                                        .as_deref()
                        })
                    })
                {
                    pins.push(node(p, n)?);
                }
            }
            Ok(
                json!({"kind":"context","node":root,"nodes":nodes,"edges":edges,"pinned_revisions":pins,"coverage":coverage(partial.then_some("scan_budget"))}),
            )
        }
        "history" | "search" => {
            if kind == "history" {
                rt.get_node(p, &q["node_id"]).await?;
            }
            let mut upstream = cursor_load(state, p, q).await?;
            let mut values = Vec::new();
            let limit = q["limit"].as_u64().unwrap() as usize;
            let mut exhausted = false;
            for _ in 0..8 {
                let mut params = vec![("limit".into(), (limit - values.len()).to_string())];
                if let Some(c) = upstream.as_ref() {
                    params.push(("cursor".into(), c.clone()));
                }
                let path = if kind == "search" {
                    params.push(("q".into(), q["text"].as_str().unwrap().into()));
                    if let Some(k) = q["kind_filter"].as_str() {
                        params.push(("kind".into(), k.into()));
                    }
                    format!("{base}/search")
                } else {
                    format!("{base}/nodes/{}/history", q["node_id"].as_str().unwrap())
                };
                let raw = rt.get(&path, &params).await?;
                let items = list(&raw, "items")?;
                if items.len() > limit - values.len() {
                    return Err(Error("overloaded"));
                }
                for item in items {
                    if kind == "search" {
                        if item["workspace_id"] != p.binding["workspace_id"] {
                            return Err(Error("overloaded"));
                        }
                        if p.readable(&item["id"]) {
                            if !q["kind_filter"].is_null() && q["kind_filter"] != item["kind"] {
                                return Err(Error("overloaded"));
                            }
                            values.push(node(p, item)?);
                        }
                    } else {
                        let category = item["category"].as_str().ok_or(Error("overloaded"))?;
                        let mut n = Value::Null;
                        let mut e = Value::Null;
                        if category == "revision" {
                            if item["data"]["id"] != q["node_id"] {
                                return Err(Error("overloaded"));
                            }
                            n = node(p, &item["data"])?;
                        } else if category == "edge" {
                            let (edges, _) = rt
                                .checked_edges(
                                    p,
                                    std::slice::from_ref(&item["data"]),
                                    Some(&q["node_id"]),
                                )
                                .await?;
                            let Some(value) = edges.into_iter().next() else {
                                continue;
                            };
                            e = value;
                        } else if category != "lifecycle" {
                            return Err(Error("overloaded"));
                        }
                        let v = json!({"entry_id":item["entry_id"],"sequence":item["sequence"].as_i64().filter(|s|*s>0).ok_or(Error("overloaded"))?.to_string(),"category":category,"subject_id":q["node_id"],"occurred_at":item["created_at"],"node":n,"edge":e});
                        if !contract::valid("HistoryItem", &v) {
                            return Err(Error("overloaded"));
                        }
                        values.push(v);
                    }
                }
                upstream = match raw.get("next_cursor") {
                    Some(Value::Null) => None,
                    Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                    _ => return Err(Error("overloaded")),
                };
                if upstream.is_none() {
                    exhausted = true;
                    break;
                }
                if values.len() == limit {
                    break;
                }
            }
            let next = cursor_save(state, p, q, upstream).await?;
            if kind == "history" {
                Ok(
                    json!({"kind":"history","items":values,"next_cursor":next,"coverage":coverage((!exhausted).then_some("scan_budget"))}),
                )
            } else {
                Ok(
                    json!({"kind":"search","nodes":values,"next_cursor":next,"coverage":coverage((!exhausted).then_some("scan_budget"))}),
                )
            }
        }
        _ => Err(Error("invalid_request")),
    }
}

#[async_trait::async_trait]
impl crate::row_retention::TaskState for Runtime {
    async fn completed(&self, workspace: Uuid, task: Uuid) -> anyhow::Result<bool> {
        let raw = self
            .get(&format!("/v1/workspaces/{workspace}/nodes/{task}"), &[])
            .await
            .map_err(|_| anyhow::anyhow!("retention Graph observation unavailable"))?;
        anyhow::ensure!(
            contract::upstream_valid("Node", &raw)
                && raw["workspace_id"] == json!(workspace)
                && raw["id"] == json!(task)
                && raw["kind"] == "task",
            "invalid retention Graph observation"
        );
        Ok(raw["task_state"] == "completed" && raw["deleted"] == false && raw["retired"] == false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn principal() -> Principal {
        let f: Value = serde_json::from_str(include_str!(
            "../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let c = &f["context"];
        Principal {
            created: Vec::new(),
            binding: c["binding"].clone(),
            profile: c["profile"].clone(),
            context: c["context"].clone(),
            version: 1,
            revision: 1,
            expires: 1,
        }
    }
    fn graph() -> Value {
        serde_json::from_str(include_str!(
            "../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
        ))
        .unwrap()
    }
    #[test]
    fn projection_removes_metadata_and_enforces_workspace() {
        let p = principal();
        let graph = graph();
        let raw = &graph["context"]["node"];
        let result = node(&p, raw).unwrap();
        assert!(result.get("provenance").is_none());
        assert!(result.get("metadata").is_none());
        assert!(result.get("created_by").is_none());
        let mut wrong = raw.clone();
        wrong["workspace_id"] = json!(Uuid::new_v4());
        assert!(node(&p, &wrong).is_err());
        let mut hidden = raw.clone();
        hidden["id"] = json!(Uuid::new_v4());
        assert_eq!(node(&p, &hidden).unwrap_err().0, "not_found");
    }
    #[test]
    fn edge_projection_requires_both_endpoints_and_allowed_type() {
        let p = principal();
        let g = graph();
        let raw = &g["context"]["edges"][0];
        assert!(edge(&p, raw).unwrap().is_some());
        let mut hidden = raw.clone();
        hidden["target"] = json!(Uuid::new_v4());
        assert!(edge(&p, &hidden).unwrap().is_none());
        let mut mismatch = raw.clone();
        mismatch["workspace_id"] = json!(Uuid::new_v4());
        assert!(edge(&p, &mismatch).is_err());
    }
    #[tokio::test]
    async fn http_adapter_refuses_missing_or_changed_response_version() {
        for version in [None, Some("2")] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new().fallback(move || async move {
                        let mut r = Json(json!({"secret":"must not project"})).into_response();
                        if let Some(v) = version {
                            r.headers_mut()
                                .insert("X-AQ-Graph-API-Version", v.parse().unwrap());
                        }
                        r
                    }),
                )
                .await
                .unwrap()
            });
            let rt = Runtime {
                mutation_actor: None,
                client: reqwest::Client::new(),
                url: format!("http://{address}").parse().unwrap(),
                token: "ordinary-service-token-0000000000000000".into(),
                ttl: 1000,
            };
            assert_eq!(rt.get("/v1/test", &[]).await.unwrap_err().0, "overloaded");
            task.abort();
        }
    }
    #[tokio::test]
    async fn http_adapter_never_exposes_graph_errors_or_follows_redirects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        [("X-AQ-Graph-API-Version", "1")],
                        "secret database details",
                    )
                }),
            )
            .await
            .unwrap()
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential");
        std::fs::write(&path, "ordinary-service-token-0000000000000000").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let rt = Runtime::new(&AgentAccessConfig {
            mutation_actor_id: None,
            graph_url: format!("http://{address}"),
            graph_token_file: path,
            credential_ttl_ms: 10000,
        })
        .await
        .unwrap();
        assert_eq!(rt.get("/v1/test", &[]).await.unwrap_err().0, "overloaded");
        task.abort();
    }
}
