use super::*;
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DependencyKey {
    Task { project: Uuid, task: Uuid },
    Wait { wait: Uuid },
}
#[derive(Clone, Debug)]
pub struct KnownGraph {
    pub scope: wire::Scope,
    pub complete_known_component: bool,
    pub revision: wire::ArtifactRef,
    pub edges: Vec<(DependencyKey, DependencyKey)>,
    pub external_unknown: bool,
}
impl KnownGraph {
    /// Outside storage serializes this bounded known-component proof with the
    /// registration write. This helper cannot fence a concurrent remote graph.
    pub fn check(
        &self,
        owner: &DependencyKey,
        added: &[DependencyKey],
    ) -> Result<wire::CycleAssessment> {
        wire::artifact(&self.revision)?;
        self.scope.validate()?;
        if !self.complete_known_component {
            return Err(Error::Capacity);
        }
        if self
            .edges
            .len()
            .checked_add(added.len())
            .is_none_or(|n| n > 4096)
        {
            return Err(Error::Capacity);
        }
        for key in self
            .edges
            .iter()
            .flat_map(|(a, b)| [a, b])
            .chain(std::iter::once(owner))
            .chain(added.iter())
        {
            match key {
                DependencyKey::Task { project, task } => {
                    wire::uuid(*project)?;
                    wire::uuid(*task)?;
                    require(*project == self.scope.project_id)?;
                }
                DependencyKey::Wait { wait } => wire::uuid(*wait)?,
            }
        }
        let mut adjacency: BTreeMap<&DependencyKey, Vec<&DependencyKey>> = BTreeMap::new();
        let mut nodes = BTreeSet::new();
        for (a, b) in &self.edges {
            nodes.insert(a);
            nodes.insert(b);
            adjacency.entry(a).or_default().push(b);
        }
        nodes.insert(owner);
        for b in added {
            nodes.insert(b);
        }
        if nodes.len() > 1024 {
            return Err(Error::Capacity);
        }
        for start in added {
            let mut visited = BTreeSet::new();
            let mut pending = vec![(start, 0)];
            while let Some((node, depth)) = pending.pop() {
                if node == owner {
                    return Err(Error::Cycle);
                }
                if depth > 64 {
                    return Err(Error::Capacity);
                }
                if !visited.insert(node) {
                    continue;
                }
                if let Some(next) = adjacency.get(node) {
                    for n in next {
                        pending.push((*n, depth + 1));
                    }
                }
            }
        }
        Ok(if self.external_unknown {
            wire::CycleAssessment::ExternalUnknown {
                reason: wire::Text::new("remote dependency closure is not proven")?,
            }
        } else {
            wire::CycleAssessment::NoKnownCycle {
                revision: self.revision.clone(),
            }
        })
    }
}
