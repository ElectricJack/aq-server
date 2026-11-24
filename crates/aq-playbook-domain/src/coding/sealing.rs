use super::*;

pub(super) struct ArtifactDraft<'a> {
    pub id: &'a str,
    pub purpose: &'a str,
    pub entry: EntryPoint,
    pub steps: BTreeMap<String, Step>,
}

impl Builder {
    pub(super) fn seal(
        &self,
        draft: ArtifactDraft<'_>,
        children: &[&Artifact],
        escalated: Expr,
    ) -> Result<Artifact, CodingError> {
        let ArtifactDraft {
            id,
            purpose,
            entry,
            mut steps,
        } = draft;
        let (registry, needed) = self.used_contracts(&steps)?;
        steps.insert("failure".into(), ret("escalated", escalated));
        let required_capabilities = (needed.into_iter())
            .chain(
                children
                    .iter()
                    .flat_map(|c| c.content.playbook.required_capabilities.clone()),
            )
            .collect();
        let dependencies = dependencies(children);
        let source = format!("{COMPILER}\n{id}\n{purpose}\n");
        let a = Artifact::seal(ArtifactContent {
            format_version: 1,
            compiler: COMPILER.into(),
            source: SourceBinding {
                digest: hash_bytes(source.as_bytes()),
                header: SourceHeader {
                    format_version: 1,
                    id: id.into(),
                    scope: "project".into(),
                    interfaces: BTreeSet::from(["main".into()]),
                    definitions: BTreeSet::new(),
                },
            },
            playbook: Playbook {
                id: id.into(),
                purpose: purpose.into(),
                scope: "project".into(),
                entries: BTreeMap::from([("main".into(), entry)]),
                steps,
                triggers: vec![],
                limits: Limits::default(),
                required_capabilities,
            },
            registry,
            dependencies,
            definitions: BTreeMap::new(),
            source_map: BTreeMap::new(),
        })
        .map_err(|e| CodingError::Invalid(e.to_string()))?;
        a.verify()
            .map_err(|e| CodingError::Invalid(format!("{id}: {e}")))?;
        Ok(a)
    }
    fn used_contracts(
        &self,
        steps: &BTreeMap<String, Step>,
    ) -> Result<(Registry, BTreeSet<String>), CodingError> {
        let mut commands = BTreeSet::new();
        let mut events = BTreeSet::new();
        for step in steps.values() {
            match &step.op {
                Operation::Invoke { command, .. } => {
                    commands.insert(command.name.clone());
                }
                Operation::Wait { event: Some(e), .. } => {
                    events.insert(e.name.clone());
                }
                _ => {}
            }
        }
        // One identity filler, the authoring tools' own: the contracts the
        // steps name, each asynchronous command's reconciliation command and
        // completion event, and the abilities those commands need.
        retain_registry(&self.registry, &commands, &events).map_err(CodingError::Invalid)
    }
}

fn dependencies(children: &[&Artifact]) -> BTreeMap<String, CallableDependency> {
    children
        .iter()
        .map(|a| {
            let binding = a.content.playbook.id.clone();
            (
                binding.clone(),
                CallableDependency {
                    binding,
                    selection: DependencySelection::Exact,
                    artifact: a.digest.clone(),
                    interfaces: a.content.playbook.entries.clone(),
                    capabilities: a.content.playbook.required_capabilities.clone(),
                    scope: "project".into(),
                    dependencies: a
                        .content
                        .dependencies
                        .values()
                        .flat_map(|d| {
                            std::iter::once(d.artifact.clone()).chain(d.dependencies.clone())
                        })
                        .collect(),
                },
            )
        })
        .collect()
}
