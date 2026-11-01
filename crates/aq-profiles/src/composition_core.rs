//! One compiler algorithm shared by distinct task and central adapters.
use crate::*;
use aq_session_api::{Omission, OverrideExplanation, Permission, Source};
use std::collections::{BTreeMap, BTreeSet};
pub(crate) enum Accounting {
    RawText,
    SeparatedStartup,
}
impl Accounting {
    fn charge(&self, index: usize, c: &ContextInput) -> u64 {
        c.text.len() as u64
            + if matches!(self, Self::SeparatedStartup) && index != 0 {
                2
            } else {
                0
            }
    }
}
pub(crate) struct CoreRequest<'a> {
    pub catalog_digest: &'a str,
    pub agent_profile: &'a str,
    pub ceilings: [&'a [Permission]; 4],
    pub available_tools: &'a [String],
    pub available_adapters: &'a [String],
    pub harness: &'a String,
    pub environment: &'a String,
    pub platform: &'a ContextInput,
    pub subject_contexts: &'a [ContextInput],
    pub context: &'a [ContextInput],
    pub overrides: &'a [Override],
    pub context_limit_bytes: Option<u64>,
    pub context_entry_bytes: usize,
    pub accounting: Accounting,
    pub required_context_error: &'static str,
}
pub(crate) struct Compiled<'a> {
    pub agent: &'a Agent,
    pub intelligence: &'a Intelligence,
    pub role_source: Source,
    pub limit: u64,
    pub blocking: Vec<String>,
    pub overrides: Vec<OverrideExplanation>,
    pub flattened: Vec<Source>,
    pub requested: Vec<Permission>,
    pub effective: Vec<Permission>,
    pub denied: Vec<Permission>,
    pub omissions: Vec<Omission>,
    pub references: Vec<Source>,
    pub required_bytes: u64,
    pub optional_bytes: u64,
    pub instructions: Vec<Instruction>,
}
fn same(a: &Permission, b: &Permission) -> bool {
    a.operation == b.operation && a.scope_reference == b.scope_reference
}
fn contains(list: &[Permission], p: &Permission) -> bool {
    list.iter().any(|q| same(p, q))
}
fn add(list: &mut Vec<Permission>, p: &Permission) {
    if !contains(list, p) {
        list.push(p.clone());
    }
}
fn source_valid(s: &Source) -> Result<()> {
    text(&s.identity)?;
    text(&s.revision)?;
    ensure(
        s.digest.len() == 64
            && s.digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "context source digest",
    )
}
pub(crate) fn context_valid(c: &ContextInput, limit: usize) -> Result<()> {
    source_valid(&c.source)?;
    ensure(
        !c.text.trim().is_empty() && c.text.len() <= limit,
        "context byte bounds",
    )?;
    ensure(
        aq_session_protocol::Digest::of(c.text.as_bytes()).as_str() == c.source.digest,
        "context bytes do not match source digest",
    )?;
    ensure(
        !c.required || c.delivery == Delivery::Startup,
        "required context cannot be on-demand",
    )
}
pub(crate) fn compile_core<'a>(
    catalog: &'a ValidatedCatalog,
    request: &CoreRequest<'_>,
) -> Result<Compiled<'a>> {
    ensure(
        request.catalog_digest == catalog.digest(),
        "request catalog digest mismatch",
    )?;
    ensure(
        request.context.len() <= 128
            && request.overrides.len() <= 16
            && request.available_tools.len() <= 128
            && request.available_adapters.len() <= 128,
        "compile request cardinality",
    )?;
    for list in request.ceilings {
        ensure(list.len() <= 128, "ceiling bound")?;
        let mut seen = BTreeSet::new();
        for p in list {
            text(&p.operation)?;
            text(&p.scope_reference)?;
            ensure(
                seen.insert((&p.operation, &p.scope_reference)),
                "duplicate ceiling",
            )?;
        }
    }
    for list in [request.available_tools, request.available_adapters] {
        let mut seen = BTreeSet::new();
        for value in list {
            text(value)?;
            ensure(seen.insert(value), "duplicate available capability")?;
        }
    }
    text(request.harness)?;
    text(request.environment)?;
    let a = &catalog.artifact().catalog;
    let agent = a
        .agents
        .iter()
        .find(|a| key(&a.id, a.version) == request.agent_profile)
        .ok_or_else(|| Error::Invalid("unknown agent recipe".into()))?;
    let execution = a
        .execution
        .iter()
        .find(|e| key(&e.id, e.version) == agent.execution)
        .unwrap();
    let intelligence = a
        .intelligence
        .iter()
        .find(|i| key(&i.id, i.version) == agent.intelligence)
        .unwrap();
    let mut limit = execution.context_limit_bytes;
    if let Some(ceiling) = request.context_limit_bytes {
        ensure(
            ceiling > 0 && ceiling <= 1_048_576,
            "subject context ceiling",
        )?;
        limit = limit.min(ceiling);
    }
    let mut overrides = Vec::new();
    let mut override_fields = BTreeSet::new();
    for o in request.overrides {
        source_valid(&o.source)?;
        text(&o.reason_code)?;
        ensure(override_fields.insert(&o.field), "duplicate override")?;
        ensure(
            agent.allowed_overrides.contains(&o.field) && o.field == "context_limit_bytes",
            "protected or undeclared override",
        )?;
        ensure(
            o.value > 0 && o.value <= limit,
            "override cannot broaden context ceiling",
        )?;
        limit = o.value;
        overrides.push(OverrideExplanation {
            field: o.field.clone(),
            source: o.source.clone(),
            reason_code: o.reason_code.clone(),
        });
    }
    let mut blocking = Vec::new();
    if !execution.harnesses.contains(request.harness)
        || !execution.environments.contains(request.environment)
    {
        blocking.push("unsupported harness/environment".into());
    }
    if !request.available_adapters.contains(request.harness) {
        blocking.push(format!("unavailable adapter:{}", request.harness));
    }
    if !intelligence
        .candidates
        .iter()
        .any(|c| c.harnesses.contains(request.harness))
    {
        blocking.push("no compatible intelligence candidate".into());
    }
    let mut roots = agent.behaviors.clone();
    roots.extend(agent.capabilities.clone());
    let topological = expand(catalog.artifact(), &roots)?;
    let modules: BTreeMap<_, _> = a
        .modules
        .iter()
        .map(|m| (key(&m.id, m.version), m))
        .collect();
    let closure = |roots: &[String]| -> Result<BTreeSet<String>> {
        Ok(expand(catalog.artifact(), roots)?.into_iter().collect())
    };
    let required_roots: Vec<_> = roots
        .iter()
        .filter(|r| modules[*r].required)
        .cloned()
        .collect();
    let required = closure(&required_roots)?;
    let allowed = |p: &Permission| {
        request.available_tools.contains(&p.operation)
            && [
                request.ceilings[0],
                request.ceilings[1],
                request.ceilings[2],
                request.ceilings[3],
                execution.permissions.as_slice(),
            ]
            .iter()
            .all(|c| contains(c, p))
    };
    let mut available = BTreeMap::<String, bool>::new();
    let mut reasons = BTreeMap::<String, String>::new();
    for name in &topological {
        let m = modules[name];
        let missing: Vec<_> = m
            .required_adapters
            .iter()
            .filter(|id| !request.available_adapters.contains(id))
            .collect();
        let reason = if !missing.is_empty() {
            Some(format!("adapter_unavailable:{missing:?}"))
        } else if m
            .requirements
            .iter()
            .any(|r| r.required && !allowed(&r.permission))
        {
            Some("required_operation_denied".into())
        } else if m.dependencies.iter().any(|d| !available[d]) {
            Some("dependency_unavailable".into())
        } else {
            None
        };
        available.insert(name.clone(), reason.is_none());
        if let Some(reason) = reason {
            reasons.insert(name.clone(), reason);
        }
    }
    let selected_roots: Vec<_> = roots.iter().filter(|r| available[*r]).cloned().collect();
    let selected = closure(&selected_roots)?;
    let mut order = topological.clone();
    order.sort_by_key(|name| usize::from(modules[name].kind == ModuleKind::Capability));
    let mut requested = Vec::new();
    let mut effective = Vec::new();
    let mut denied = Vec::new();
    let mut omissions = Vec::new();
    let mut module_context = Vec::new();
    let mut flattened = Vec::new();
    for name in &order {
        let m = modules[name];
        let source = catalog.source("module", name)?;
        flattened.push(source.clone());
        let is_required = required.contains(name);
        if is_required && !available[name] {
            blocking.push(format!("required module {name}:{}", reasons[name]));
        }
        for r in &m.requirements {
            add(&mut requested, &r.permission);
            if is_required && r.required && !allowed(&r.permission) {
                add(&mut denied, &r.permission);
            }
        }
        if !selected.contains(name) {
            omissions.push(Omission {
                source,
                reason_code: reasons
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "dependent_module_omitted".into()),
                required: is_required,
            });
            continue;
        }
        ensure(
            !is_required || m.delivery == Delivery::Startup,
            "required dependency cannot be deferred",
        )?;
        for r in &m.requirements {
            if allowed(&r.permission) {
                add(&mut effective, &r.permission);
            } else {
                omissions.push(Omission {
                    source: source.clone(),
                    reason_code: format!("optional_permission_denied:{}", r.permission.operation),
                    required: false,
                });
            }
        }
        module_context.push(ContextInput {
            source,
            text: catalog.artifact().instructions[name].clone(),
            delivery: m.delivery.clone(),
            required: is_required,
            omission_priority: 0,
        });
    }
    for c in std::iter::once(request.platform).chain(request.subject_contexts.iter()) {
        context_valid(c, request.context_entry_bytes)?;
        ensure(
            c.required && c.delivery == Delivery::Startup,
            request.required_context_error,
        )?;
    }
    for c in request.context {
        context_valid(c, request.context_entry_bytes)?;
    }
    let mut aggregate_input_bytes = 0usize;
    for context in std::iter::once(request.platform)
        .chain(request.subject_contexts.iter())
        .chain(request.context.iter())
    {
        aggregate_input_bytes = aggregate_input_bytes
            .checked_add(context.text.len())
            .ok_or_else(|| Error::Invalid("context input overflow".into()))?;
        ensure(
            aggregate_input_bytes <= MAX_CATALOG_BYTES,
            "aggregate context input bound",
        )?;
    }
    let role_source = catalog.source("agent", request.agent_profile)?;
    let role = ContextInput {
        source: role_source.clone(),
        text: agent.role.clone(),
        delivery: Delivery::Startup,
        required: true,
        omission_priority: 0,
    };
    // Fixed prompt order: platform, role/behavior, packs, project, exact assignment, labelled context.
    let mut contexts = vec![request.platform.clone(), role];
    contexts.extend(module_context);
    contexts.extend(request.subject_contexts.iter().cloned());
    contexts.extend(request.context.iter().cloned());
    ensure(contexts.len() <= 128, "instruction plan cardinality")?;
    let mut seen = BTreeSet::new();
    for c in &contexts {
        ensure(
            seen.insert((c.source.identity.clone(), c.source.revision.clone())),
            "duplicate context identity",
        )?;
    }
    let sum = |required: bool| -> Result<u64> {
        contexts
            .iter()
            .enumerate()
            .filter(|(_, c)| c.delivery == Delivery::Startup && c.required == required)
            .try_fold(0u64, |n, (index, c)| {
                n.checked_add(request.accounting.charge(index, c))
                    .ok_or_else(|| Error::Invalid("context sum overflow".into()))
            })
    };
    let required_bytes = sum(true)?;
    let mut optional_bytes = sum(false)?;
    if required_bytes > limit {
        return Err(Error::Invalid(format!(
            "required context overflow: {required_bytes}>{limit}; sources:{}",
            contexts
                .iter()
                .filter(|c| c.required)
                .map(|c| c.source.identity.as_str())
                .collect::<Vec<_>>()
                .join(",")
        )));
    }
    let mut drop_order: Vec<_> = contexts
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.required && c.delivery == Delivery::Startup)
        .map(|(i, c)| (c.omission_priority, i))
        .collect();
    drop_order.sort();
    let mut dropped = BTreeSet::new();
    let mut dropped_modules = BTreeSet::new();
    for (_, index) in drop_order {
        if required_bytes + optional_bytes <= limit {
            break;
        }
        if dropped.contains(&index) {
            continue;
        }
        let mut group = BTreeSet::from([index]);
        if let Some(name) = contexts[index].source.identity.strip_prefix("module:") {
            let mut names = BTreeSet::from([name.to_string()]);
            loop {
                let before = names.len();
                for module in &order {
                    if modules[module]
                        .dependencies
                        .iter()
                        .any(|d| names.contains(d))
                    {
                        names.insert(module.clone());
                    }
                }
                if before == names.len() {
                    break;
                }
            }
            for (i, c) in contexts.iter().enumerate() {
                if c.source
                    .identity
                    .strip_prefix("module:")
                    .is_some_and(|n| names.contains(n))
                {
                    ensure(!c.required, "cannot omit required dependency closure")?;
                    group.insert(i);
                }
            }
            dropped_modules.extend(names);
        }
        for index in group {
            if dropped.insert(index) {
                if contexts[index].delivery == Delivery::Startup {
                    optional_bytes -= request.accounting.charge(index, &contexts[index]);
                }
                omissions.push(Omission {
                    source: contexts[index].source.clone(),
                    reason_code: "optional_context_budget".into(),
                    required: false,
                });
            }
        }
    }
    // Omitted optional capability closures cannot leave their operations effective.
    effective.clear();
    for name in &order {
        if selected.contains(name) && !dropped_modules.contains(name) {
            for r in &modules[name].requirements {
                if allowed(&r.permission) {
                    add(&mut effective, &r.permission);
                }
            }
        }
    }
    let references = contexts
        .iter()
        .enumerate()
        .filter(|(index, c)| c.delivery == Delivery::OnDemand && !dropped.contains(index))
        .map(|(_, c)| c.source.clone())
        .collect();
    let instructions = contexts
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !dropped.contains(i))
        .map(|(_, c)| Instruction {
            source: c.source,
            delivery: c.delivery,
            required: c.required,
            text: c.text,
        })
        .collect();
    Ok(Compiled {
        agent,
        intelligence,
        role_source,
        limit,
        blocking,
        overrides,
        flattened,
        requested,
        effective,
        denied,
        omissions,
        references,
        required_bytes,
        optional_bytes,
        instructions,
    })
}
