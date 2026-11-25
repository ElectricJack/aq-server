use super::*;
impl Decide {
    pub fn valid(&self) -> bool {
        ids_valid([self.operation_id, self.project_id, self.registration_id])
            && self.revision >= 1
            && (1..=32).contains(&self.vetted.len())
            && commits(&self.vetted)
            && self.evidence.valid()
    }
    /// Whether the approval task, from its `/context` read, is no longer
    /// draft: then the plan reconciles an earlier release and needs the AQ
    /// journal's registration provenance.
    pub fn released(approval: &Value) -> bool {
        approval["node"]["task_state"] != "draft"
    }
    /// Record the owner's decision. Approval completes the approval gate and
    /// releases the existing cleanup task once, in the same batch.
    ///
    /// `journaled` says whether the AQ journal holds this owner's confirmed
    /// operation that created the registration; it is read only when the
    /// approval task was already [`released`](Self::released).
    pub fn plan(&self, reg: &Registration, gates: [&Value; 3], journaled: bool) -> Planned {
        require(reg.id == self.registration_id, "wrong registration")?;
        require(
            reg.same_repositories(&self.vetted),
            "vet exactly the registered repositories",
        )?;
        require(
            reg.get("approved").is_null() && !reg.closed(),
            "this generation has already released its cleanup",
        )?;
        require(
            u64::try_from(self.revision).ok() == Some(reg.decision_revision() + 1),
            "the decision revision must follow the last decision",
        )?;
        let [approval, cleanup, closure] = reg.gates(gates)?;
        if approval["task_state"] != "draft" {
            return self.reconcile(reg, [approval, cleanup, closure], journaled);
        }
        require(
            approval["task_state"] == "draft"
                && cleanup["task_state"] == "draft"
                && trusted(cleanup, &reg.service),
            "approval and the protected cleanup must both still be draft",
        )?;
        let anchor = reg.anchor(&format!("decision/{}", self.revision));
        let event = json!({"registration":reg.id,"kind":"decision","revision":self.revision,
            "choice":self.choice,"vetted":self.vetted,"evidence":self.evidence.json()});
        let mut evidence = git_evidence(&self.vetted);
        evidence.push(self.evidence.proof("integrated-vetting"));
        let mut operations = reg
            .create(anchor, Receipt::Decision(&event), evidence)
            .to_vec();
        let mut state = reg.state.clone();
        state.insert("decision".into(), json!(anchor));
        state.insert("decision_revision".into(), json!(self.revision));
        if self.choice == Choice::Approve {
            operations.extend([
                patch(approval, json!({"task_state":"completed",
                    "metadata":marked(approval, json!({"registration":reg.id,"role":"approval","decision":anchor}))?})),
                verify(reg.approval, version(approval)? + 1, vec![self.evidence.proof("integrated-vetting")]),
                patch(cleanup, json!({"task_state":"active",
                    "metadata":marked(cleanup, json!({"registration":reg.id,"role":"cleanup","approval":anchor}))?})),
            ]);
            state.insert("approved".into(), json!(anchor));
        }
        operations.extend(reg.save(state)?);
        Ok(operations)
    }

    /// Record the owner's approval for an existing registration whose
    /// approval task finished as ordinary work, releasing the cleanup task,
    /// before any decision was recorded. The task state is never taken as
    /// approval: only this explicit, vetted owner decision is. The original
    /// tasks are version-guarded and left as they are, so nothing is released
    /// again and the work that ran is preserved.
    fn reconcile(&self, reg: &Registration, gates: [&Value; 3], journaled: bool) -> Planned {
        let [approval, cleanup, closure] = gates;
        require(
            journaled,
            "the registration was not created by this owner's AQ registration",
        )?;
        require(
            self.choice == Choice::Approve,
            "released cleanup work cannot be returned for changes",
        )?;
        require(
            reg.get("decision").is_null() && reg.decision_revision() == 0,
            "a decision is already recorded for this registration",
        )?;
        require(
            approval["task_state"] == "completed" && metadata(approval)?.get(META).is_none(),
            "the approval task did not complete outside a decision",
        )?;
        require(
            matches!(cleanup["task_state"].as_str(), Some("active" | "completed"))
                && cleanup["metadata"][META] == json!({"registration":reg.id,"role":"cleanup"}),
            "the cleanup task is not the registered task released before a decision",
        )?;
        require(
            closure["task_state"] == "draft",
            "the closure gate is not the registration's open draft",
        )?;
        let observed = |node: &Value| json!({"id":node["id"],"version":node["version"],"task_state":node["task_state"]});
        let anchor = reg.anchor(&format!("decision/{}", self.revision));
        let event = json!({"registration":reg.id,"kind":"decision","revision":self.revision,
            "choice":self.choice,"vetted":self.vetted,"evidence":self.evidence.json(),
            "released_before_decision":{"approval":observed(approval),"cleanup":observed(cleanup)}});
        let mut evidence = git_evidence(&self.vetted);
        evidence.push(self.evidence.proof("integrated-vetting"));
        evidence.push(task_evidence(reg.approval, "approval"));
        let mut operations: Vec<Value> = [approval, cleanup, closure].map(guard).to_vec();
        operations.extend(reg.create(anchor, Receipt::Decision(&event), evidence));
        let mut state = reg.state.clone();
        state.insert("decision".into(), json!(anchor));
        state.insert("decision_revision".into(), json!(self.revision));
        state.insert("approved".into(), json!(anchor));
        state.insert("reconciled".into(), json!(anchor));
        operations.extend(reg.save(state)?);
        Ok(operations)
    }
}
