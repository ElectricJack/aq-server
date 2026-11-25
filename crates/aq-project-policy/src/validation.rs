use super::{ProjectBinding, ProjectCeilings, ProjectConfig};
use std::collections::BTreeSet;
use uuid::Uuid;
impl ProjectConfig {
    pub fn validate(&self) -> Result<(), String> {
        require(self.worker_groups.len() <= 64, "worker group bound")?;
        for (id, nodes) in &self.worker_groups {
            label(id)?;
            ids(nodes, 256)?;
            require(!nodes.is_empty(), "empty worker group")?;
        }
        Ok(())
    }
}
fn require(ok: bool, reason: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(reason.into()) }
}
pub(super) fn label(s: &str) -> Result<(), String> {
    require(
        !s.is_empty()
            && s.len() <= 128
            && s.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@' | b':')
            }),
        "bounded logical reference required",
    )
}
pub(super) fn hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn ids(values: &[Uuid], max: usize) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    require(values.len() <= max, "identity list bound")?;
    for id in values {
        require(!id.is_nil() && seen.insert(id), "nil or duplicate identity")?;
    }
    Ok(())
}
pub(super) fn labels(values: &[String], max: usize) -> Result<(), String> {
    require(values.len() <= max, "reference list bound")?;
    let mut seen = BTreeSet::new();
    for s in values {
        label(s)?;
        require(seen.insert(s), "duplicate reference")?;
    }
    Ok(())
}
impl ProjectBinding {
    pub fn validate(&self, config: &ProjectConfig) -> Result<(), String> {
        config.validate()?;
        if let Some(p) = &self.publication {
            p.validate().map_err(|e| e.to_string())?;
        }
        self.validate_identity()?;
        self.validate_environment_and_roles()?;
        self.validate_workers(config)?;
        self.validate_references()?;
        self.ceilings.validate()
    }
    fn validate_identity(&self) -> Result<(), String> {
        require(
            self.schema_version == 1
                && [self.project_id, self.repository_id, self.graph_workspace_id]
                    .iter()
                    .all(|id| !id.is_nil())
                && (1..=i64::MAX as u64).contains(&self.repository_binding_generation),
            "project identity/version",
        )?;
        require(
            !self.name.trim().is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(char::is_control),
            "project name",
        )?;
        require(
            self.repository_adapter_reference == "aq.git.configured"
                && self.graph_adapter_reference == "aq.graph.configured",
            "unknown configured adapter reference",
        )?;
        Ok(())
    }
    fn validate_environment_and_roles(&self) -> Result<(), String> {
        labels(&self.allowed_environments, 32)?;
        require(
            !self.allowed_environments.is_empty(),
            "empty environment allowlist",
        )?;
        #[cfg(feature = "project_environments")]
        require(
            (self.default_environment.as_ref())
                .is_none_or(|e| self.allowed_environments.contains(e)),
            "default environment is not an allowed environment",
        )?;
        label(&self.default_role).map_err(|_| "default role")?;
        label(&self.default_intelligence).map_err(|_| "default intelligence profile")?;
        for (role, note) in &self.role_notes {
            label(role).map_err(|_| "role notes role")?;
            require(!note.contains('\0'), "role notes text")?;
        }
        Ok(())
    }
    fn validate_workers(&self, config: &ProjectConfig) -> Result<(), String> {
        ids(&self.allowed_node_ids, 256)?;
        labels(&self.allowed_worker_groups, 32)?;
        require(
            self.allow_enrolled_nodes
                || !self.allowed_node_ids.is_empty()
                || !self.allowed_worker_groups.is_empty(),
            "no eligible worker identities",
        )?;
        for group in &self.allowed_worker_groups {
            require(
                config.worker_groups.contains_key(group),
                "unknown worker group",
            )?;
        }
        Ok(())
    }
    fn validate_references(&self) -> Result<(), String> {
        require(self.context.len() <= 32, "context reference bound")?;
        let mut seen = BTreeSet::new();
        for r in std::iter::once(&self.policy).chain(&self.context) {
            label(&r.identity)?;
            label(&r.revision)?;
            require(
                hex(&r.digest) && seen.insert((&r.identity, &r.revision)),
                "invalid or duplicate policy/context reference",
            )?;
        }
        Ok(())
    }
    pub fn eligible_node(&self, config: &ProjectConfig, node: Uuid) -> bool {
        self.allow_enrolled_nodes
            || self.allowed_node_ids.contains(&node)
            || self.allowed_worker_groups.iter().any(|g| {
                config
                    .worker_groups
                    .get(g)
                    .is_some_and(|ids| ids.contains(&node))
            })
    }
}
impl ProjectCeilings {
    fn validate(&self) -> Result<(), String> {
        self.validate_permissions()?;
        self.validate_resources()?;
        require(
            self.cost_limit_microunits.is_some() == self.currency.is_some()
                && (!self.require_hard_cost_enforcement || self.cost_limit_microunits.is_some()),
            "cost ceiling basis",
        )?;
        if let Some(currency) = &self.currency {
            require(
                currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()),
                "currency code",
            )?;
        }
        Ok(())
    }
    fn validate_permissions(&self) -> Result<(), String> {
        require(self.permissions.len() <= 128, "permission bound")?;
        let mut permissions = BTreeSet::new();
        for p in &self.permissions {
            label(&p.operation)?;
            label(&p.scope_reference)?;
            require(
                p.scope_reference.contains(':')
                    && permissions.insert((&p.operation, &p.scope_reference)),
                "invalid or duplicate permission",
            )?;
        }
        Ok(())
    }
    fn validate_resources(&self) -> Result<(), String> {
        require(
            (1..=1_048_576).contains(&self.context_bytes)
                && (1..=604_800_000).contains(&self.wall_time_ms)
                && (1..=i64::MAX as u64).contains(&self.memory_bytes)
                && (1..=i64::MAX as u64).contains(&self.cpu_millis)
                && (1..=65_536).contains(&self.process_limit),
            "resource ceiling bounds",
        )
    }
}
