//! Project allowlist checks shared by owner binding mutations.
use crate::ValidatedCatalog;
pub fn validate_project_profiles(
    catalog: &ValidatedCatalog,
    agents: &[String],
    intelligence: &[String],
    execution: &[String],
    environments: &[String],
) -> Result<(), &'static str> {
    let a = &catalog.artifact().catalog;
    for key in agents {
        let agent = a
            .agents
            .iter()
            .find(|a| crate::key(&a.id, a.version) == *key)
            .ok_or("unknown allowed agent profile")?;
        if !intelligence.contains(&agent.intelligence) || !execution.contains(&agent.execution) {
            return Err("agent defaults outside project profile allowlists");
        }
    }
    for key in intelligence {
        if !a
            .intelligence
            .iter()
            .any(|i| crate::key(&i.id, i.version) == *key)
        {
            return Err("unknown allowed intelligence profile");
        }
    }
    for key in execution {
        if !a
            .execution
            .iter()
            .any(|e| crate::key(&e.id, e.version) == *key)
        {
            return Err("unknown allowed execution profile");
        }
    }
    for environment in environments {
        if !a.execution.iter().any(|e| {
            execution.contains(&crate::key(&e.id, e.version))
                && e.environments.contains(environment)
        }) {
            return Err("environment outside allowed execution profiles");
        }
    }
    Ok(())
}
