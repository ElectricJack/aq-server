use crate::*;
use std::collections::BTreeSet;
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub slots: u32,
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub disk_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeCapacity {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub revision: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub available: Resources,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeEligibility {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub eligible: bool,
    pub source_warm: bool,
    pub reason_codes: Vec<Label>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementCandidate {
    pub project_id: Uuid,
    pub task: TaskRevision,
    pub selection: ArtifactRef,
    pub priority: i32,
    pub queued_at_ms: u64,
    pub expires_at_ms: u64,
    pub resources: Resources,
    pub nodes: Vec<NodeEligibility>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementInput {
    pub schema_version: u32,
    pub now_ms: u64,
    pub after_project: Option<Uuid>,
    pub max_offers: u32,
    pub candidates: Vec<PlacementCandidate>,
    pub nodes: Vec<NodeCapacity>,
}
impl PlacementInput {
    pub fn validate(&self) -> Result<()> {
        require(
            self.schema_version == 1 && self.now_ms > 0 && (1..=128).contains(&self.max_offers),
            "placement version/time/limit",
        )?;
        if let Some(id) = self.after_project {
            crate::id(id)?;
        }
        require(
            self.candidates.len() <= 256 && self.nodes.len() <= 128,
            "placement cardinality",
        )?;
        let mut nodes = BTreeSet::new();
        for n in &self.nodes {
            id(n.node_id)?;
            id(n.boot_id)?;
            revision(n.revision)?;
            time(n.observed_at_ms, n.expires_at_ms, 120_000)?;
            require(nodes.insert(n.node_id), "duplicate node capacity")?;
        }
        let mut tasks = BTreeSet::new();
        let mut projects = BTreeSet::new();
        for c in &self.candidates {
            id(c.project_id)?;
            c.task.validate()?;
            artifact(&c.selection)?;
            projects.insert(c.project_id);
            require(
                tasks.insert(c.task.task_id),
                "ambiguous duplicate graph task",
            )?;
            require(
                c.queued_at_ms > 0
                    && c.queued_at_ms <= self.now_ms
                    && c.expires_at_ms > c.queued_at_ms
                    && c.expires_at_ms
                        .checked_sub(self.now_ms)
                        .is_none_or(|remaining| remaining <= 30_000)
                    && c.resources.slots > 0
                    && c.resources.memory_bytes > 0
                    && c.resources.cpu_millis > 0
                    && c.nodes.len() <= 128,
                "candidate time/resources/bounds",
            )?;
            let mut eligibility = BTreeSet::new();
            for n in &c.nodes {
                id(n.node_id)?;
                id(n.boot_id)?;
                require(
                    n.reason_codes.len() <= 16 && eligibility.insert(n.node_id),
                    "duplicate/excess node eligibility",
                )?;
            }
        }
        require(projects.len() <= 64, "project cardinality")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposedPlacement {
    pub project_id: Uuid,
    pub task: TaskRevision,
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub capacity_revision: u64,
    pub selection: ArtifactRef,
    pub resources: Resources,
    pub source_warm: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementRefusal {
    pub task_id: Uuid,
    pub reason: Label,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlacementOutput {
    pub schema_version: u32,
    pub authority_admission: bool,
    pub next_after_project: Option<Uuid>,
    pub proposed: Vec<ProposedPlacement>,
    pub refused: Vec<PlacementRefusal>,
}
impl PlacementOutput {
    pub fn validate(&self) -> Result<()> {
        require(
            self.schema_version == 1
                && !self.authority_admission
                && self.proposed.len() <= 128
                && self.refused.len() <= 256,
            "proposal is not authority",
        )?;
        if let Some(project) = self.next_after_project {
            id(project)?;
        }
        let mut seen = BTreeSet::new();
        for p in &self.proposed {
            id(p.project_id)?;
            p.task.validate()?;
            id(p.node_id)?;
            id(p.boot_id)?;
            revision(p.capacity_revision)?;
            artifact(&p.selection)?;
            require(
                p.resources.slots > 0
                    && p.resources.memory_bytes > 0
                    && p.resources.cpu_millis > 0
                    && seen.insert(p.task.task_id),
                "duplicate/invalid placement",
            )?;
        }
        for r in &self.refused {
            id(r.task_id)?;
            require(seen.insert(r.task_id), "duplicate refusal/placement")?;
        }
        Ok(())
    }
}
