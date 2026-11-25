use super::*;
/// What still keeps the epic open, first item only.
fn outstanding(
    reg: &Registration,
    recorded: &Recorded<'_>,
    cleanup: &Value,
    tree: &Value,
) -> Result<(), Refusal> {
    for repo in reg.repositories().into_keys() {
        let merge = recorded
            .merges
            .get(repo)
            .ok_or("a cleanup merge is pending")?;
        let build = recorded
            .ordinary
            .get(repo)
            .filter(|b| b["sha"] == merge["sha"])
            .ok_or("an ordinary build is pending")?;
        require(
            build["passed"] == true && build["flag_absent"] == true,
            "an ordinary build or flag removal check failed",
        )?;
    }
    let integrated = recorded
        .integrated
        .as_ref()
        .ok_or("the integrated acceptance check is pending")?;
    require(
        integrated["passed"] == true,
        "the integrated acceptance check failed",
    )?;
    let builds: Map<String, Value> = recorded
        .ordinary
        .iter()
        .map(|(repo, b)| (repo.clone(), b["evidence"].clone()))
        .collect();
    require(
        integrated["revisions"] == json!(recorded.merge_shas())
            && integrated["builds"] == Value::Object(builds),
        "the integrated check no longer matches the ordinary builds",
    )?;
    require(
        cleanup["task_state"] == "completed",
        "the cleanup task has not completed through normal delivery",
    )?;
    require(
        tree["truncated"] == false && tree["node"]["id"] == json!(reg.epic),
        "the epic task list is unavailable",
    )?;
    for child in tree["children"]
        .as_array()
        .ok_or("the epic task list is unavailable")?
    {
        let task = &child["node"];
        let own = [json!(reg.cleanup), json!(reg.closure)].contains(&task["id"]);
        require(
            own || matches!(task["task_state"].as_str(), Some("completed" | "canceled")),
            "another epic task is still open",
        )?;
    }
    Ok(())
}

impl Close {
    pub fn valid(&self) -> bool {
        ids_valid([self.operation_id, self.project_id, self.registration_id])
    }
    /// Complete the closure gate and the epic atomically with a verified
    /// closure receipt, only after the approved decision, every registered
    /// repository's merge and passing flag-free ordinary build, the passing
    /// integrated check over exactly those builds, and the cleanup task's
    /// ordinary completion.
    pub fn plan(
        &self,
        reg: &Registration,
        ClosureReads {
            epic,
            gates,
            records,
            tree,
        }: ClosureReads<'_>,
    ) -> Planned {
        require(
            reg.id == self.registration_id && !reg.closed(),
            "this epic retirement is closed",
        )?;
        let [approval, cleanup, closure] = reg.gates(gates)?;
        let (decision, approved) = reg.approved(records)?;
        require(
            approval["task_state"] == "completed" && closure["task_state"] == "draft",
            "the retirement gates are in the wrong state for closure",
        )?;
        let recorded = reg.evidence(records)?;
        outstanding(reg, &recorded, cleanup, tree)?;
        require(
            reg.open_epic(epic),
            "the epic was closed outside the workflow",
        )?;
        let anchor = reg.anchor("close");
        let event = json!({"registration":reg.id,"kind":"closure","approval_revision":approved["revision"],
            "decision":decision["id"],"cleanup":reg.cleanup,"merges":reg.get("merges"),
            "ordinary":reg.get("ordinary"),"integrated":reg.get("integrated")});
        let integrated = recorded
            .integrated
            .as_ref()
            .ok_or("the integrated acceptance check is pending")?;
        let proof = json!({"kind":"artifact","source_id":"integrated-acceptance",
            "artifact_id":integrated["evidence"]["artifact_id"],"sha256":integrated["evidence"]["sha256"]});
        let mut operations: Vec<Value> = [approval, cleanup, decision]
            .into_iter()
            .chain(recorded.nodes.iter().copied())
            .map(guard)
            .collect();
        operations.extend(reg.create(
            anchor,
            Receipt::Closure(&event),
            vec![proof.clone(), task_evidence(reg.cleanup, "cleanup")],
        ));
        operations.extend([
            patch(closure, json!({"task_state":"completed",
                "metadata":marked(closure, json!({"registration":reg.id,"role":"closure_gate","receipt":anchor}))?})),
            verify(reg.closure, version(closure)? + 1, vec![proof]),
            patch(epic, json!({"task_state":"completed"})),
        ]);
        let mut state = reg.state.clone();
        state.insert("closed".into(), json!(anchor));
        operations.extend(reg.save(state)?);
        Ok(operations)
    }
}
