//! Binding-owned intervals share the activation, event and run transaction.
//! A failed commit leaves its schedule cursor eligible for another collection.

use super::*;
use aq_playbooks_model::schedules::{
    BindingSchedule, FixedIntervalSchedule, ScheduleCursor, ScheduleOccurrence,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingSchedules {
    pub(super) configuration: ScheduleConfiguration,
    registrations: BTreeMap<String, Registration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct ScheduleConfiguration {
    pub(super) event: ContractRef,
    declarations: BTreeMap<String, BindingSchedule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeactivatedBinding {
    revision: u64,
    pub(super) configuration: Option<ScheduleConfiguration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registration {
    definition: FixedIntervalSchedule,
    cursor: ScheduleCursor,
}

impl BindingSchedules {
    pub(super) fn register(
        configuration: ScheduleConfiguration,
        namespace: &str,
        binding: &str,
        activation: &Activation,
        now_ms: i64,
        previous: Option<Self>,
    ) -> Result<Self> {
        let mut registrations = BTreeMap::new();
        for (key, declaration) in &configuration.declarations {
            let existing = previous
                .as_ref()
                .filter(|old| {
                    old.configuration.event == configuration.event
                        && old.configuration.declarations.get(key) == Some(declaration)
                })
                .and_then(|old| old.registrations.get(key));
            let registration = if let Some(existing) = existing {
                existing.clone()
            } else {
                let id = digest(&("aq.binding.interval", namespace, binding, key))?;
                let definition = declaration.register(
                    id,
                    activation.authority.scope.clone(),
                    activation.revision.to_string(),
                    now_ms,
                )?;
                let cursor = definition.cursor()?;
                Registration { definition, cursor }
            };
            registrations.insert(key.clone(), registration);
        }
        Ok(Self {
            configuration,
            registrations,
        })
    }
}

pub(super) fn check_event(artifact: &Artifact, scope: &str, event: &ContractRef) -> Result<()> {
    let contract = artifact.content.registry.events.get(&event.name);
    if !contract.is_some_and(|c| {
        c.scopes.contains(scope) && c.fingerprint().is_ok_and(|hash| hash == event.fingerprint)
    }) || !artifact
        .content
        .playbook
        .triggers
        .iter()
        .any(|trigger| trigger.event == *event)
    {
        return Err(Error::Refused(
            "a scheduled binding requires its interval event trigger".into(),
        ));
    }
    Ok(())
}

impl<S: Storage> Engine<S> {
    /// Reconcile a configured binding without renewing its grant or replaying
    /// its schedule. A disabled binding stays disabled across startup.
    #[allow(clippy::too_many_arguments)]
    pub fn configure_binding(
        &self,
        binding: &str,
        artifact: &str,
        authority: Authority,
        may_activate: bool,
        declarations: &[BindingSchedule],
        event: Option<ContractRef>,
        now_ms: i64,
    ) -> Result<bool> {
        BindingSchedule::validate_all(declarations)?;
        let configuration = if declarations.is_empty() {
            None
        } else {
            let event = event.ok_or_else(|| Error::Refused("interval event unavailable".into()))?;
            Some(ScheduleConfiguration {
                event,
                declarations: declarations
                    .iter()
                    .map(|d| (d.key.clone(), d.clone()))
                    .collect(),
            })
        };
        self.transaction_choice_at(Some(now_ms), |s| {
            if let Some(disabled) = s.deactivated_bindings.get_mut(binding) {
                let changed = disabled.configuration != configuration;
                disabled.configuration = configuration;
                return Ok((false, changed));
            }
            let active = s.bindings.get(binding);
            if active.is_some_and(|a| a.artifact != artifact || a.authority != authority) {
                return Err(Error::Conflict("configured activation differs".into()));
            }
            let old_configuration = s
                .binding_schedules
                .get(binding)
                .map(|old| &old.configuration);
            if active.is_some() && old_configuration == configuration.as_ref() {
                return Ok((true, false));
            }
            if !may_activate && (active.is_none() || !declarations.is_empty()) {
                return Err(Error::Refused("binding requires a live grant".into()));
            }
            if let Some(a) = s.bindings.get_mut(binding) {
                s.catalog_revision += 1;
                a.revision = s.catalog_revision;
                record_binding_change(s, binding, now_ms)?;
            } else {
                activate_binding(s, binding, artifact, authority.clone(), now_ms)?;
            }
            let previous = s.binding_schedules.remove(binding);
            if let Some(configuration) = configuration {
                let activation = &s.bindings[binding];
                check_event(
                    &s.artifacts[artifact],
                    &authority.scope,
                    &configuration.event,
                )?;
                let registered = BindingSchedules::register(
                    configuration,
                    &self.namespace,
                    binding,
                    activation,
                    now_ms,
                    previous,
                )?;
                s.binding_schedules.insert(binding.into(), registered);
            }
            Ok((true, true))
        })
    }

    /// Stop new calls and event starts. A retry at the same revision is safe.
    /// Already accepted events and admitted runs keep their pinned snapshots.
    pub fn deactivate_binding(&self, binding: &str, revision: u64, now_ms: i64) -> Result<bool> {
        self.transaction_choice_at(Some(now_ms), |s| {
            if s.deactivated_bindings
                .get(binding)
                .is_some_and(|d| d.revision == revision)
            {
                return Ok((true, false));
            }
            if s.bindings.get(binding).map(|a| a.revision) != Some(revision) {
                return Ok((false, false));
            }
            s.bindings.remove(binding);
            let configuration = s
                .binding_schedules
                .remove(binding)
                .map(|old| old.configuration);
            s.deactivated_bindings.insert(
                binding.into(),
                DeactivatedBinding {
                    revision,
                    configuration,
                },
            );
            s.catalog_revision += 1;
            record_binding_change(s, binding, now_ms)?;
            Ok((true, true))
        })
    }

    /// Admit only the latest missed occurrence for each due schedule. Each
    /// interval starts triggers only on its owning binding.
    pub fn collect_interval_events(
        &self,
        now_ms: i64,
        limit: usize,
        mut event_for: impl FnMut(&BindingSchedule, ContractRef, ScheduleOccurrence) -> EventEnvelope,
    ) -> Result<usize> {
        if !(1..=2).contains(&limit) {
            return Err(Error::Refused("invalid interval collection limit".into()));
        }
        self.transaction_choice_at(Some(now_ms), |s| {
            let mut due = vec![];
            for (binding, schedules) in &s.binding_schedules {
                if !s
                    .bindings
                    .get(binding)
                    .is_some_and(|a| a.controls.event_starts_enabled)
                {
                    continue;
                }
                for (key, r) in &schedules.registrations {
                    if let Ok(at) = r.definition.scheduled_at_ms(r.cursor.next_index)
                        && at <= now_ms
                        && r.cursor.last_observed_ms.is_none_or(|last| last <= now_ms)
                    {
                        due.push((at, binding.clone(), key.clone()));
                    }
                }
            }
            due.sort_unstable();
            let mut count = 0;
            for (_, binding, key) in due.into_iter().take(limit) {
                let schedules = &s.binding_schedules[&binding];
                let r = &schedules.registrations[&key];
                let plan = r.definition.plan(&r.cursor, now_ms, 1)?;
                let Some(occurrence) = plan.occurrences.into_iter().next() else {
                    continue;
                };
                let event = event_for(
                    &schedules.configuration.declarations[&key],
                    schedules.configuration.event.clone(),
                    occurrence,
                );
                let producer = event.producer.clone();
                let epoch = s
                    .retention
                    .producer_epochs
                    .get(&producer)
                    .copied()
                    .unwrap_or(0);
                let accepted =
                    accept_event(s, &producer, event, epoch, None, now_ms, Some(&binding))?
                        .ok_or_else(|| invariant("new interval event has expired".into()))?;
                fanout_event(s, &accepted.key, 128, now_ms)?;
                s.binding_schedules
                    .get_mut(&binding)
                    .unwrap()
                    .registrations
                    .get_mut(&key)
                    .unwrap()
                    .cursor = plan.cursor;
                count += 1;
            }
            Ok((count, count > 0))
        })
    }
}
