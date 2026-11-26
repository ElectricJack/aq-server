use super::*;
impl Restore {
    pub fn valid(&self) -> bool {
        ids_valid([self.operation_id, self.project_id, self.registration_id]) && self.revision >= 1
    }
    /// Undelete the registration's own closure gate at its guarded revision
    /// through Graph restoration, then verify it again: restoration never
    /// carries a verification badge forward.
    pub fn plan(
        &self,
        reg: &Registration,
        epic: &Value,
        GateRevision {
            tombstone,
            snapshot,
        }: GateRevision<'_>,
    ) -> Planned {
        require(
            reg.id == self.registration_id && !reg.closed(),
            "the registration is closed",
        )?;
        require(
            reg.open_epic(epic),
            "restore the gate of this project's open epic",
        )?;
        validate_tombstone(reg, tombstone)?;
        self.validate_snapshot(reg, snapshot)?;
        validate_links(reg, snapshot)?;
        let deleted = version(tombstone)?;
        Ok(vec![
            guard(&reg.node),
            guard(epic),
            json!({"op":"restore_node","id":reg.closure,"expected_version":deleted,"revision":self.revision,"include_lifecycle":true}),
            verify(
                reg.closure,
                deleted + 1,
                vec![task_evidence(reg.epic, "epic")],
            ),
        ])
    }
    fn validate_snapshot(&self, reg: &Registration, snapshot: &Value) -> Result<(), Refusal> {
        let guarded = json!({"aq:project_id":reg.project,"aq:human_actor_id":reg.actor,
            (META):{"registration":reg.id,"role":"closure_gate"}});
        let node = &snapshot["node"];
        let same_revision = node["id"] == json!(reg.closure)
            && node["content_revision"].as_i64() == Some(self.revision);
        let draft =
            node["kind"] == "task" && node["title"] == GATE_TITLE && node["task_state"] == "draft";
        require(
            same_revision
                && draft
                && verified(reg, node)
                && node["metadata"] == guarded
                && snapshot["relationships_available"] == true,
            "the revision is not the registration's verified closure gate",
        )
    }
}
fn validate_tombstone(reg: &Registration, tombstone: &Value) -> Result<(), Refusal> {
    require(
        tombstone["id"] == json!(reg.closure)
            && tombstone["kind"] == "task"
            && tombstone["deleted"] == true
            && tombstone["created_by"] == reg.service.as_str()
            && tombstone["metadata"]["aq:project_id"] == json!(reg.project)
            && reg.registered(tombstone, "closure_gate"),
        "only the registration's deleted closure gate can be restored",
    )
}
fn validate_links(reg: &Registration, snapshot: &Value) -> Result<(), Refusal> {
    let edge = Uuid::new_v5(
        &reg.id,
        format!("parent-child/{}/{}", reg.epic, reg.closure).as_bytes(),
    );
    let links: Vec<_> = snapshot["links"]
        .as_array()
        .ok_or("the revision has no relationship snapshot")?
        .iter()
        .filter(|l| l["deleted"] != true)
        .collect();
    require(
        links.len() == 1
            && links[0]["id"] == json!(edge)
            && links[0]["edge_type"] == "parent-child"
            && links[0]["source"] == json!(reg.epic)
            && links[0]["target"] == json!(reg.closure),
        "the revision lacks exactly its epic relationship",
    )
}

fn verified(reg: &Registration, node: &Value) -> bool {
    trusted(node, &reg.service)
        && node["provenance"]
            .as_array()
            .is_some_and(|p| p.contains(&task_evidence(reg.epic, "epic")))
}
