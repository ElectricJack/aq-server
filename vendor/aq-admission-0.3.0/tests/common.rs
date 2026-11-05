#![allow(dead_code)]
use aq_admission::*;
use uuid::Uuid;
pub fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
pub fn artifact(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
pub fn binding() -> LaunchBinding {
    LaunchBinding {
        schema_version: 1,
        session: SessionIdentity::Remote {
            node_id: id(11),
            boot_id: id(12),
            attempt_id: id(13),
            assignment_generation: 1,
            session_id: id(14),
        },
        offer_id: id(15),
        attempt_id: id(13),
        assignment_generation: 1,
        execution_spec_digest: Digest::of(b"spec"),
        launch_manifest_digest: Digest::of(b"manifest"),
        graph_access_profile_digest: Digest::of(b"graph-access"),
    }
}
pub fn lease() -> MaterialLease {
    MaterialLease {
        schema_version: 1,
        lease_id: id(16),
        binding: binding(),
        credential_profile: artifact("credential-profile"),
        credential_revision: 1,
        provider: Label::new("fixture-provider").unwrap(),
        account_reference: Label::new("fixture-account").unwrap(),
        endpoint_reference: Label::new("fixture-endpoint").unwrap(),
        purpose: Label::new("model-authentication").unwrap(),
        material_digest: Digest::of(b"synthetic-key"),
        material_bytes: 13,
        issued_at_ms: 1000,
        expires_at_ms: 2000,
    }
}
pub fn source() -> SourceAdmission {
    SourceAdmission {
        schema_version: 1,
        project_id: id(31),
        project_revision: 1,
        repository_id: id(41),
        repository_binding_generation: 1,
        policy: artifact("source-policy"),
        selection: SourceSelection::NamedRef {
            reference: Label::new("refs/heads/main").unwrap(),
        },
        resolved_object: ObjectId {
            format: ObjectFormat::Sha1,
            hex: "a".repeat(40),
        },
        observed_at_ms: 1000,
        expires_at_ms: 2000,
    }
}
pub fn retained() -> RetainedArtifact {
    RetainedArtifact {
        artifact: ArtifactRef {
            digest: Digest::of(b"actual-artifact"),
            ..artifact("context")
        },
        kind: ArtifactKind::Context,
        bytes: 15,
        retention_receipt: artifact("receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    }
}
pub fn candidate(project: u128, task: u128) -> PlacementCandidate {
    PlacementCandidate {
        project_id: id(project),
        task: TaskRevision {
            workspace_id: id(21),
            task_id: id(task),
            version: 1,
            content_revision: 1,
        },
        selection: artifact("sealed-selection"),
        priority: 0,
        queued_at_ms: 1000,
        expires_at_ms: 2000,
        resources: Resources {
            slots: 1,
            memory_bytes: 100,
            cpu_millis: 100,
            disk_bytes: 100,
        },
        nodes: vec![NodeEligibility {
            node_id: id(11),
            boot_id: id(12),
            eligible: true,
            source_warm: false,
            reason_codes: vec![],
        }],
    }
}
pub fn placement() -> PlacementInput {
    PlacementInput {
        schema_version: 1,
        now_ms: 1500,
        after_project: None,
        max_offers: 2,
        candidates: vec![candidate(31, 51), candidate(31, 52), candidate(32, 53)],
        nodes: vec![NodeCapacity {
            node_id: id(11),
            boot_id: id(12),
            revision: 1,
            observed_at_ms: 1000,
            expires_at_ms: 2000,
            available: Resources {
                slots: 2,
                memory_bytes: 200,
                cpu_millis: 200,
                disk_bytes: 200,
            },
        }],
    }
}
pub fn documents() -> Vec<ContractDocument> {
    let lease = lease();
    vec![
        ContractDocument::SourceAdmission(source()),
        ContractDocument::RetainedArtifact(retained()),
        ContractDocument::MaterialLease(lease.clone()),
        ContractDocument::MaterialReceipt(MaterialReceipt {
            schema_version: 1,
            operation_id: id(17),
            lease_id: lease.lease_id,
            binding: lease.binding.clone(),
            material_digest: lease.material_digest.clone(),
            private_binding: artifact("worker-private-credential-binding"),
            received_at_ms: 1500,
        }),
        ContractDocument::PlacementInput(placement()),
        ContractDocument::PlacementOutput(place(&placement()).unwrap()),
    ]
}

// Legacy placement fixture builder only; scheduling is not a protocol API.
use std::collections::{BTreeMap, BTreeSet};
trait FixtureResources {
    fn fits(self, capacity: Self) -> bool;
    fn reserve(&mut self, value: Self);
}
impl FixtureResources for Resources {
    fn fits(self, capacity: Self) -> bool {
        self.slots <= capacity.slots
            && self.memory_bytes <= capacity.memory_bytes
            && self.cpu_millis <= capacity.cpu_millis
            && self.disk_bytes <= capacity.disk_bytes
    }
    fn reserve(&mut self, value: Self) {
        self.slots -= value.slots;
        self.memory_bytes -= value.memory_bytes;
        self.cpu_millis -= value.cpu_millis;
        self.disk_bytes -= value.disk_bytes;
    }
}
/// Fair bounded proposal only. No DB claim is made; eligibility is an explicit
/// trusted-input fact, while warmth affects preference only AFTER eligibility.
pub fn place(input: &PlacementInput) -> Result<PlacementOutput> {
    input.validate()?;
    let mut capacity: BTreeMap<_, _> = input
        .nodes
        .iter()
        .map(|n| (n.node_id, (n, n.available)))
        .collect();
    let mut projects: BTreeMap<Uuid, Vec<&PlacementCandidate>> = BTreeMap::new();
    for c in &input.candidates {
        projects.entry(c.project_id).or_default().push(c);
    }
    for tasks in projects.values_mut() {
        tasks.sort_by_key(|c| {
            (
                std::cmp::Reverse(c.priority),
                c.queued_at_ms,
                c.task.task_id,
            )
        });
    }
    let mut order: Vec<_> = projects.keys().copied().collect();
    if let Some(after) = input.after_project {
        let index = order.partition_point(|p| *p <= after);
        order.rotate_left(index);
    }
    let mut result = PlacementOutput {
        schema_version: 1,
        authority_admission: false,
        next_after_project: input.after_project,
        proposed: vec![],
        refused: vec![],
    };
    let mut pending: BTreeSet<_> = input.candidates.iter().map(|c| c.task.task_id).collect();
    loop {
        let mut progress = false;
        for project in &order {
            if result.proposed.len() >= input.max_offers as usize {
                break;
            }
            let mut accepted = None;
            for c in &projects[project] {
                if !pending.contains(&c.task.task_id) {
                    continue;
                }
                if input.now_ms >= c.expires_at_ms {
                    pending.remove(&c.task.task_id);
                    result.refused.push(PlacementRefusal {
                        task_id: c.task.task_id,
                        reason: Label::new("candidate_stale")?,
                    });
                    continue;
                }
                let mut eligible: Vec<_> = c.nodes.iter().filter(|n| n.eligible).collect();
                eligible.sort_by_key(|n| (!n.source_warm, n.node_id));
                for n in eligible {
                    if let Some((snapshot, available)) = capacity.get_mut(&n.node_id)
                        && snapshot.boot_id == n.boot_id
                        && snapshot.observed_at_ms <= input.now_ms
                        && input.now_ms < snapshot.expires_at_ms
                        && c.resources.fits(*available)
                    {
                        available.reserve(c.resources);
                        accepted = Some((*c, n, snapshot.revision));
                        break;
                    }
                }
                if accepted.is_some() {
                    break;
                }
            }
            if let Some((c, n, revision)) = accepted {
                pending.remove(&c.task.task_id);
                result.proposed.push(ProposedPlacement {
                    project_id: *project,
                    task: c.task.clone(),
                    node_id: n.node_id,
                    boot_id: n.boot_id,
                    capacity_revision: revision,
                    selection: c.selection.clone(),
                    resources: c.resources,
                    source_warm: n.source_warm,
                });
                result.next_after_project = Some(*project);
                progress = true;
            }
        }
        if !progress || result.proposed.len() >= input.max_offers as usize {
            break;
        }
    }
    for task in pending {
        result.refused.push(PlacementRefusal {
            task_id: task,
            reason: Label::new(if result.proposed.len() >= input.max_offers as usize {
                "batch_limit"
            } else {
                "no_current_eligible_resource_fit"
            })?,
        });
    }
    result.refused.sort_by_key(|r| r.task_id);
    result.validate()?;
    Ok(result)
}
