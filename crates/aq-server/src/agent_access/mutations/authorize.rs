use super::*;
use std::collections::{BTreeMap, BTreeSet};
fn list<'a>(v: &'a Value, k: &str) -> Result<&'a Vec<Value>> {
    v[k].as_array().ok_or(Error("invalid_request"))
}
fn cap(p: &Principal, c: &str) -> Result<()> {
    if list(&p.profile, "capabilities")?.contains(&json!(c)) {
        Ok(())
    } else {
        Err(Error("unsupported_capability"))
    }
}
fn write(p: &Principal, id: &Value) -> Result<()> {
    p.require(id)?;
    if !p.profile["write_scope"]["record_ids"]
        .as_array()
        .is_some_and(|ids| ids.contains(id))
    {
        return Err(Error("scope_denied"));
    }
    Ok(())
}
fn guard(node: &Value, actor: &str) -> Result<()> {
    if node["deleted"] != false
        || node["retired"] != false
        || node["verification"] != "unverified"
        || (node["kind"] == "knowledge" && node["created_by"] != actor)
    {
        return Err(Error("scope_denied"));
    }
    Ok(())
}
fn version(v: &Value) -> Result<i64> {
    v.as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .ok_or(Error("invalid_request"))
}
pub(super) fn identity(rt: &Runtime, workspace: &Value) -> Result<Value> {
    Ok(
        json!({"origin":rt.url.as_str(),"actor":rt.mutation_actor.as_ref().ok_or(Error("unsupported_capability"))?,"credential_verifier":crate::tokens::verifier(&rt.token),"workspace_id":workspace,"receipt_retention":"indefinite-pinned-graph-v1"}),
    )
}
struct ReadPlan {
    reads: BTreeMap<String, bool>,
    created: BTreeSet<String>,
    creations: usize,
    is_proposal: bool,
}

fn collect_reads(p: &Principal, commands: &[Value]) -> Result<ReadPlan> {
    let mut reads = BTreeMap::<String, bool>::new();
    let mut created = BTreeSet::<String>::new();
    let mut proposed = BTreeSet::<String>::new();
    let proposal = commands
        .iter()
        .filter(|c| c["kind"] == "propose_decomposition")
        .count();
    if proposal > 0 && (proposal != 1 || commands.len() != 1) {
        return Err(Error("invalid_request"));
    }
    for c in commands {
        let kind = c["kind"].as_str().ok_or(Error("invalid_request"))?;
        cap(
            p,
            match kind {
                "create_finding" => "write.finding",
                "correct_record" => "write.correct",
                "add_relationship" | "remove_relationship" => "write.relationships",
                "propose_decomposition" => "write.proposal",
                _ => return Err(Error("invalid_request")),
            },
        )?;
        match kind {
            "create_finding" => {
                if !created.insert(c["id"].as_str().unwrap().into()) {
                    return Err(Error("id_conflict"));
                }
            }
            "correct_record" => {
                reads.insert(c["id"].as_str().unwrap().into(), true);
                for key in c["patch"].as_object().unwrap().keys() {
                    if !list(&p.profile["write_scope"], "correct_fields")?.contains(&json!(key)) {
                        return Err(Error("scope_denied"));
                    }
                }
            }
            "add_relationship" | "remove_relationship" => {
                reads.insert(c["source"].as_str().unwrap().into(), true);
                reads
                    .entry(c["target"].as_str().unwrap().into())
                    .or_insert(false);
            }
            "propose_decomposition" => {
                collect_proposal_reads(p, c, &mut reads, &mut proposed)?;
            }
            _ => unreachable!(),
        }
    }
    if created.len() + proposed.len()
        > p.profile["write_scope"]["max_created_records"]
            .as_u64()
            .unwrap_or(0) as usize
    {
        return Err(Error("scope_denied"));
    }
    Ok(ReadPlan {
        creations: created.len() + proposed.len(),
        is_proposal: proposal > 0,
        reads,
        created,
    })
}

fn collect_proposal_reads(
    p: &Principal,
    c: &Value,
    reads: &mut BTreeMap<String, bool>,
    proposed: &mut BTreeSet<String>,
) -> Result<()> {
    reads.insert(c["parent_id"].as_str().unwrap().into(), true);
    if list(c, "tasks")?.len()
        > p.profile["write_scope"]["max_proposal_tasks"]
            .as_u64()
            .unwrap_or(0) as usize
    {
        return Err(Error("scope_denied"));
    }
    for t in list(c, "tasks")? {
        if !proposed.insert(t["id"].as_str().unwrap().into()) {
            return Err(Error("id_conflict"));
        }
    }
    let mut guards = BTreeSet::new();
    for g in list(c, "guards")? {
        let id = g["id"].as_str().unwrap();
        if !guards.insert(id) || g["id"] == c["parent_id"] {
            return Err(Error("invalid_request"));
        }
        reads.insert(id.into(), true);
    }
    let mut edges = BTreeSet::new();
    for edge in list(c, "relationships")? {
        if !edges.insert(edge["id"].as_str().unwrap()) {
            return Err(Error("id_conflict"));
        }
        for endpoint in ["source", "target"] {
            let id = edge[endpoint].as_str().unwrap();
            if !proposed.contains(id) {
                if edge[endpoint] != c["parent_id"] && !guards.contains(id) {
                    return Err(Error("invalid_request"));
                }
                reads.insert(id.into(), true);
            }
        }
    }
    Ok(())
}

async fn observe_records(
    rt: &Runtime,
    p: &Principal,
    actor: &str,
    plan: &ReadPlan,
) -> Result<BTreeMap<String, Value>> {
    let base = format!(
        "/v1/workspaces/{}",
        p.binding["workspace_id"].as_str().unwrap()
    );
    let mut facts = BTreeMap::new();
    // Permission is checked before any public graph lookup. New IDs are verified
    // atomically by graph creation rather than probing inaccessible existence.
    for (id, mutating) in &plan.reads {
        if plan.created.contains(id) {
            continue;
        }
        if *mutating {
            write(p, &json!(id))?;
        } else {
            p.require(&json!(id))?;
        }
        let raw = rt.get(&format!("{base}/nodes/{id}"), &[]).await?;
        if !contract::upstream_valid("Node", &raw)
            || raw["id"] != *id
            || raw["workspace_id"] != p.binding["workspace_id"]
        {
            return Err(Error("overloaded"));
        }
        if *mutating {
            guard(&raw, actor)?;
        }
        facts.insert(id.clone(), raw);
    }
    Ok(facts)
}

fn validate_observed_versions(
    p: &Principal,
    commands: &[Value],
    facts: &BTreeMap<String, Value>,
) -> Result<()> {
    // Author/protection observations authorize only the exact observed version.
    // A caller-chosen future CAS token must never turn a stale guard into a write.
    for c in commands {
        let mut checks = Vec::new();
        match c["kind"].as_str().unwrap() {
            "correct_record" => checks.push((&c["id"], &c["expected_version"])),
            "add_relationship" | "remove_relationship" => {
                checks.push((&c["source"], &c["expected_source_version"]))
            }
            "propose_decomposition" => {
                checks.push((&c["parent_id"], &c["expected_parent_version"]));
                for g in list(c, "guards")? {
                    checks.push((&g["id"], &g["expected_version"]));
                }
            }
            _ => {}
        }
        for (id, expected) in checks {
            if let Some(node) = facts.get(id.as_str().unwrap())
                && node["version"].as_i64() != Some(version(expected)?)
            {
                return Err(Error("id_conflict"));
            }
        }
        if c["kind"] == "propose_decomposition" {
            for edge in list(c, "relationships")? {
                if !list(&p.profile["write_scope"], "relationship_types")?
                    .contains(&edge["edge_type"])
                    || !list(&p.profile["read_scope"], "edge_types")?.contains(&edge["edge_type"])
                {
                    return Err(Error("scope_denied"));
                }
            }
        }
    }
    Ok(())
}

async fn map_operations(
    rt: &Runtime,
    p: &Principal,
    commands: &[Value],
    facts: &BTreeMap<String, Value>,
    restricted: bool,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let base = format!(
        "/v1/workspaces/{}",
        p.binding["workspace_id"].as_str().unwrap()
    );
    let provenance = json!({"source_id":format!("aq:{}",p.binding["attempt_id"].as_str().unwrap()),"kind":"task","task_id":p.binding["task_id"],"attempt_id":p.binding["attempt_id"],"session_id":p.binding["session_id"]});
    let mut operations = Vec::new();
    let mut finding_ids = Vec::new();
    for c in commands {
        let kind = c["kind"].as_str().unwrap();
        match kind {
            "create_finding" => {
                finding_ids.push(c["id"].clone());
                operations.push(json!({"op":"create_node","id":c["id"],"kind":"knowledge","title":c["title"],"body":c["body"],"labels":c["labels"],"content_type":c["content_type"],"provenance":[provenance]}));
            }
            "correct_record" => {
                let key = c["id"].as_str().unwrap();
                if let Some(n) = facts.get(key)
                    && n["kind"] == "task"
                {
                    if restricted {
                        return Err(Error("scope_denied"));
                    }
                    cap(p, "write.task_content")?;
                }
                operations.push(json!({"op":"update_node","id":c["id"],"expected_version":version(&c["expected_version"])?,"patch":c["patch"]}));
            }
            "add_relationship" | "remove_relationship" => {
                operations.push(json!({"op":"update_node","id":c["source"],"expected_version":version(&c["expected_source_version"])?,"patch":{}}));
                let edge_type = relationship_type(rt, p, c, &base).await?;
                if (restricted && matches!(edge_type.as_str(), Some("blocks" | "parent-child")))
                    || !list(&p.profile["write_scope"], "relationship_types")?.contains(&edge_type)
                    || !list(&p.profile["read_scope"], "edge_types")?.contains(&edge_type)
                {
                    return Err(Error("scope_denied"));
                }
                if kind == "remove_relationship" {
                    operations.push(json!({"op":"remove_edge","id":c["id"],"expected_version":version(&c["expected_version"])?}));
                } else {
                    operations.push(json!({"op":"add_edge","id":c["id"],"source":c["source"],"target":c["target"],"edge_type":edge_type,"description":c["description"],"source_revision":if c["source_revision"].is_null(){None}else{Some(version(&c["source_revision"])? )},"target_revision":if c["target_revision"].is_null(){None}else{Some(version(&c["target_revision"])? )}}));
                }
            }
            "propose_decomposition" => {
                append_proposal(c, facts, &provenance, &mut operations)?;
            }
            _ => unreachable!(),
        }
    }
    Ok((operations, finding_ids))
}

async fn relationship_type(rt: &Runtime, p: &Principal, c: &Value, base: &str) -> Result<Value> {
    Ok(if c["kind"] == "remove_relationship" {
        let raw = rt
            .get(
                &format!("{base}/nodes/{}/context", c["source"].as_str().unwrap()),
                &[
                    ("depth".into(), "1".into()),
                    ("limit".into(), "200".into()),
                    ("max_bytes".into(), "1048576".into()),
                ],
            )
            .await?;
        let edge = list(&raw, "edges")?
            .iter()
            .find(|e| e["id"] == c["id"])
            .ok_or(Error("not_found"))?;
        if !contract::upstream_valid("Edge", edge)
            || edge["workspace_id"] != p.binding["workspace_id"]
            || edge["source"] != c["source"]
            || edge["target"] != c["target"]
        {
            return Err(Error("not_found"));
        }
        edge["edge_type"].clone()
    } else {
        c["edge_type"].clone()
    })
}

fn append_proposal(
    c: &Value,
    facts: &BTreeMap<String, Value>,
    provenance: &Value,
    operations: &mut Vec<Value>,
) -> Result<()> {
    if facts
        .get(c["parent_id"].as_str().unwrap())
        .is_none_or(|n| n["kind"] != "task")
    {
        return Err(Error("invalid_request"));
    }
    operations.push(json!({"op":"update_node","id":c["parent_id"],"expected_version":version(&c["expected_parent_version"])?,"patch":{}}));
    for g in list(c, "guards")? {
        operations.push(json!({"op":"update_node","id":g["id"],"expected_version":version(&g["expected_version"])?,"patch":{}}));
    }
    for t in list(c, "tasks")? {
        let mut op = t.clone();
        op["op"] = json!("create_node");
        op["kind"] = json!("task");
        op["task_state"] = json!("draft");
        op["provenance"] = json!([provenance]);
        operations.push(op);
    }
    for e in list(c, "relationships")? {
        for endpoint in ["source", "target"] {
            if facts
                .get(e[endpoint].as_str().unwrap())
                .is_some_and(|n| n["kind"] != "task")
            {
                return Err(Error("invalid_request"));
            }
        }
        let mut op = e.clone();
        op["op"] = json!("add_edge");
        operations.push(op);
    }
    Ok(())
}

pub(super) async fn prepare(
    state: &AppState,
    p: &Principal,
    request: &Value,
) -> Result<(Value, usize)> {
    let rt = runtime(state)?;
    let actor = rt
        .mutation_actor
        .as_deref()
        .ok_or(Error("unsupported_capability"))?;
    let commands = list(request, "commands")?;
    let plan = collect_reads(p, commands)?;
    let facts = observe_records(rt, p, actor, &plan).await?;
    let task_writes = plan.is_proposal
        || commands.iter().any(|c| match c["kind"].as_str() {
            Some("correct_record") => facts
                .get(c["id"].as_str().unwrap())
                .is_some_and(|n| n["kind"] == "task"),
            Some("remove_relationship") => true,
            _ => false,
        });
    let restricted = task_writes
        && (p
            .grants
            .get(aq_agent_grants::op::TASK_CREATE_SUBTASK)
            .is_some_and(|g| g.limits.draft_only)
            || grant_check::work_policy(state, p).await?.is_some());
    if restricted && plan.is_proposal {
        return Err(Error("scope_denied"));
    }
    validate_observed_versions(p, commands, &facts)?;
    let (operations, finding_ids) = map_operations(rt, p, commands, &facts, restricted).await?;
    let mut subjects = BTreeSet::new();
    for op in &operations {
        if let Some(id) = op["id"].as_str() {
            subjects.insert(id.to_string());
        }
    }
    if operations.len() > 100 || subjects.len() > 64 {
        return Err(Error("invalid_request"));
    }
    let mut payload = json!({"idempotency_key":format!("aq-agent-v1:{}",request["operation_id"].as_str().unwrap()),"reason":request["reason"],"operations":operations});
    if plan.is_proposal {
        payload["name"] = commands[0]["name"].clone();
    }
    if !contract::upstream_valid(
        if plan.is_proposal {
            "DraftRequest"
        } else {
            "MutationRequest"
        },
        &payload,
    ) {
        return Err(Error("invalid_request"));
    }
    let mapped = json!({"service":identity(rt,&p.binding["workspace_id"])?,"class":if plan.is_proposal{"draft_create"}else{"mutate"},"payload":payload,"subjects":subjects,"finding_ids":finding_ids});
    // Reserve the worst bounded public receipt, not merely the pending envelope.
    let versions:Vec<Value>=subjects.iter().map(|id|json!({"kind":"node","id":id,"version":"9223372036854775807","content_revision":"9223372036854775807"})).collect();
    let receipt = json!({"kind":"graph_mutation","workspace_id":p.binding["workspace_id"],"change_set_id":Uuid::new_v4(),"versions":versions,"affected_record_ids":subjects,"task_completion":false});
    p.bound(&json!({"api_version":1,"operation_id":request["operation_id"],"binding":p.binding,"payload_digest":digest(request)?,"state":"confirmed","admission":"recorded","effect":"confirmed","receipt":receipt}))?;
    Ok((mapped, plan.creations))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Principal, Value) {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let principal = Principal {
            profile: fixture["context"]["profile"].clone(),
            ..Default::default()
        };
        (principal, fixture["command_requests"].clone())
    }

    #[test]
    fn preflight_rejects_mixed_proposals_and_unguarded_endpoints() {
        let (p, requests) = fixture();
        let proposal = &requests["propose_decomposition"]["commands"][0];
        let correction = &requests["correct_record"]["commands"][0];
        assert_eq!(
            collect_reads(&p, &[proposal.clone(), correction.clone()])
                .err()
                .unwrap()
                .0,
            "invalid_request"
        );
        let mut unguarded = proposal.clone();
        unguarded["guards"] = json!([]);
        assert_eq!(
            collect_reads(&p, &[unguarded]).err().unwrap().0,
            "invalid_request"
        );
        let duplicate = requests["create_finding"]["commands"][0].clone();
        assert_eq!(
            collect_reads(&p, &[duplicate.clone(), duplicate.clone()])
                .err()
                .unwrap()
                .0,
            "id_conflict"
        );
        let mut restricted = p;
        restricted.profile["write_scope"]["correct_fields"] = json!(["title"]);
        assert_eq!(
            collect_reads(&restricted, std::slice::from_ref(correction))
                .err()
                .unwrap()
                .0,
            "scope_denied"
        );
    }

    #[test]
    fn observed_version_guards_refuse_both_past_and_future_tokens() {
        let (p, requests) = fixture();
        for (kind, id_field, version_field) in [
            ("correct_record", "id", "expected_version"),
            ("add_relationship", "source", "expected_source_version"),
            ("remove_relationship", "source", "expected_source_version"),
        ] {
            let mut command = requests[kind]["commands"][0].clone();
            let facts = BTreeMap::from([(
                command[id_field].as_str().unwrap().to_owned(),
                json!({"version":2}),
            )]);
            for version in ["1", "2", "3"] {
                command[version_field] = json!(version);
                let result = validate_observed_versions(&p, std::slice::from_ref(&command), &facts);
                if version == "2" {
                    result.unwrap();
                } else {
                    assert_eq!(result.unwrap_err().0, "id_conflict");
                }
            }
        }
    }
}
