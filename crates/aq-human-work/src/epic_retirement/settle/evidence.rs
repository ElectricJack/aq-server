use super::*;
impl Evidence {
    fn event(&self, reg: &Registration) -> Value {
        let mut event = json!({"registration":reg.id,"kind":self.kind,
            "approval_revision":self.approval_revision,"evidence":self.evidence.json()});
        let map = event.as_object_mut().expect("object");
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                map.insert(key.into(), value);
            }
        };
        put("repository", self.repository.as_ref().map(|v| json!(v)));
        put("pull_request", self.pull_request.as_ref().map(|v| json!(v)));
        put("sha", self.sha.as_ref().map(|v| json!(v)));
        put("supersedes_merge", self.supersedes_merge.map(|v| json!(v)));
        put("flag_absent", self.flag_absent.map(|v| json!(v)));
        put("revisions", self.revisions.as_ref().map(|v| json!(v)));
        put(
            "builds",
            self.builds
                .as_ref()
                .map(|b| b.iter().map(|(r, a)| (r.clone(), a.json())).collect()),
        );
        put("passed", self.passed.map(|v| json!(v)));
        event
    }
    /// Record one owner-attested cleanup receipt against the approved
    /// decision: a merge, the ordinary build at that merge, or the integrated
    /// check over exactly the passing flag-free ordinary builds.
    pub fn plan(
        &self,
        reg: &Registration,
        gates: [&Value; 3],
        records: &BTreeMap<Uuid, Value>,
    ) -> Planned {
        require(
            reg.id == self.registration_id && !reg.closed(),
            "this epic retirement is closed",
        )?;
        let (_, decision) = reg.approved(records)?;
        require(
            decision["revision"].as_i64() == Some(i64::from(self.approval_revision)),
            "evidence must name the approved decision revision",
        )?;
        let [approval, _, _] = reg.gates(gates)?;
        require(
            approval["task_state"] == "completed",
            "the approval gate has not released cleanup",
        )?;
        let current = reg.evidence(records)?;
        let event = self.event(reg);
        let anchor = reg.anchor(&format!("evidence/{}", digest(&event)));
        let mut update = EvidenceUpdate {
            reg,
            current,
            anchor,
            state: reg.state.clone(),
        };
        let commits = match self.kind {
            EvidenceKind::Merge | EvidenceKind::Ordinary => update.repository(self)?,
            EvidenceKind::Integrated => update.integrated(self, &event)?,
        };
        let mut evidence = git_evidence(&commits);
        evidence.push(self.evidence.proof(match self.kind {
            EvidenceKind::Merge => "merge",
            EvidenceKind::Ordinary => "ordinary",
            EvidenceKind::Integrated => "integrated",
        }));
        let mut operations = reg
            .create(anchor, Receipt::Evidence(&event), evidence)
            .to_vec();
        operations.extend(reg.save(update.state)?);
        Ok(operations)
    }
}

struct EvidenceUpdate<'a> {
    reg: &'a Registration,
    current: Recorded<'a>,
    anchor: Uuid,
    state: Map<String, Value>,
}
impl EvidenceUpdate<'_> {
    fn repository(&mut self, request: &Evidence) -> Result<BTreeMap<String, String>, Refusal> {
        let repo = request.repository.clone().unwrap_or_default();
        let sha = request.sha.clone().unwrap_or_default();
        require(
            self.reg.repositories().contains_key(repo.as_str()),
            "the repository is not registered",
        )?;
        if request.kind == EvidenceKind::Merge {
            require(
                self.reg.get("merges")[&repo]
                    == request.supersedes_merge.map_or(Value::Null, |id| json!(id)),
                "a merge must supersede the repository's current receipt",
            )?;
            require(
                self.current
                    .merges
                    .get(&repo)
                    .is_none_or(|m| m["sha"] != sha.as_str()),
                "a repair merge must have a different SHA",
            )?;
            let mut merges = self
                .reg
                .get("merges")
                .as_object()
                .cloned()
                .unwrap_or_default();
            merges.insert(repo.clone(), json!(self.anchor));
            let mut ordinary = self
                .reg
                .get("ordinary")
                .as_object()
                .cloned()
                .unwrap_or_default();
            ordinary.remove(&repo);
            self.state.insert("merges".into(), Value::Object(merges));
            self.state
                .insert("ordinary".into(), Value::Object(ordinary));
            self.state.insert("integrated".into(), Value::Null);
        } else {
            require(
                self.current
                    .merges
                    .get(&repo)
                    .is_some_and(|m| m["sha"] == sha.as_str()),
                "the ordinary build must match the recorded merge SHA",
            )?;
            let mut ordinary = self
                .reg
                .get("ordinary")
                .as_object()
                .cloned()
                .unwrap_or_default();
            ordinary.insert(repo.clone(), json!(self.anchor));
            self.state
                .insert("ordinary".into(), Value::Object(ordinary));
        }
        Ok(BTreeMap::from([(repo, sha)]))
    }
    fn integrated(
        &mut self,
        request: &Evidence,
        event: &Value,
    ) -> Result<BTreeMap<String, String>, Refusal> {
        let merged = self.current.merge_shas();
        require(
            json!(request.revisions) == json!(merged)
                && merged
                    .keys()
                    .map(String::as_str)
                    .eq(self.reg.repositories().into_keys()),
            "the integrated check must name every registered repository's merge SHA",
        )?;
        let builds: BTreeMap<_, _> = self
            .current
            .ordinary
            .iter()
            .filter(|(_, b)| b["passed"] == true && b["flag_absent"] == true)
            .map(|(repo, b)| (repo.clone(), b["evidence"].clone()))
            .collect();
        require(
            event["builds"] == json!(builds) && builds.keys().eq(merged.keys()),
            "the integrated check must name the passing flag-free ordinary builds",
        )?;
        self.state.insert("integrated".into(), json!(self.anchor));
        Ok(request.revisions.clone().unwrap_or_default())
    }
}
