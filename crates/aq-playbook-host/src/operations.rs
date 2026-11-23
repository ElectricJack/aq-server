use super::*;
use Request::*;

fn event_status<S: Storage>(engine: &Engine<S>, event_key: &str) -> Result<Value, Error> {
    let snapshot = engine.snapshot().map_err(engine_error)?;
    let event = snapshot.events.get(event_key).ok_or(Error::Invalid)?;
    let runs: Vec<_> = event
        .destinations
        .iter()
        .filter_map(|d| d.run.as_deref())
        .collect();
    Ok(
        serde_json::json!({"destinations":event.destinations.len(),"pending":event.destinations.len() - runs.len(),"runs":runs}),
    )
}
fn interval_events<S: Storage>(engine: &Engine<S>, now_ms: i64) -> Result<Value, Error> {
    let count = engine
        .collect_interval_events(now_ms, 2, |schedule, contract, occurrence| EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: occurrence.id.clone(),
            contract,
            scope: occurrence.scope,
            correlation: [("operation".into(), occurrence.id)].into(),
            subject: Some(occurrence.schedule_id.clone()),
            revision: Some(occurrence.index),
            causation: None,
            occurred_at_ms: Some(occurrence.scheduled_at_ms),
            observed_at_ms: Some(now_ms),
            payload: serde_json::json!({
                "outcome": "reported", "reference": schedule.reference(),
                "subject": {"id": occurrence.schedule_id, "revision": occurrence.index}
            }),
        })
        .map_err(runner_error)?;
    Ok(serde_json::json!(count))
}
fn retryable_plugin(intent: &Intent, now_ms: i64) -> bool {
    intent.deadline_at_ms > now_ms
        && !intent.cancel_requested
        && matches!(
            intent.status,
            IntentStatus::Dispatched | IntentStatus::Unknown
        )
        && plugins::command_plugin(&intent.command.name).is_some()
}
fn pending_plugin<S: Storage>(engine: &Engine<S>, now_ms: i64) -> Result<bool, Error> {
    Ok(engine
        .snapshot()
        .map_err(engine_error)?
        .intents
        .values()
        .any(|intent| retryable_plugin(intent, now_ms)))
}
fn tick<S: Storage>(
    engine: &Engine<S>,
    host: &mut impl DomainHost,
    shared: &Shared,
    now_ms: i64,
) -> Result<Value, Error> {
    let mut now_ms = now_ms.max(host.transition_time(now_ms));
    if shared.plugin_artifacts.load(Ordering::Acquire) && pending_plugin(engine, now_ms)? {
        host.set_mode(HostMode::RetryPlugins);
        let recovery = engine.recover(host, now_ms);
        host.set_mode(HostMode::Dispatch);
        recovery.map_err(engine_error)?;
    }
    let mut guarded = PluginCatalogHost {
        engine,
        host,
        stale_artifacts: shared
            .stale_artifacts
            .get()
            .filter(|stale| !stale.is_empty()),
    };
    // A slow host operation can use most of a tick. Start each new
    // command's deadline when its own transition runs and check an
    // existing deadline against the current time before dispatch.
    let mut transitions = 0;
    while transitions < shared.limits.transitions {
        now_ms = now_ms.max(guarded.host.transition_time(now_ms));
        if engine
            .drive(&mut guarded, now_ms, 1)
            .map_err(engine_error)?
            == 0
        {
            break;
        }
        transitions += 1;
    }
    Ok(serde_json::json!({"transitions": transitions}))
}
fn install<S: Storage>(
    engine: &Engine<S>,
    shared: &Shared,
    artifact: Artifact,
) -> Result<Value, Error> {
    let registry = artifact.content.registry.clone();
    engine.install(artifact).map_err(engine_error)?;
    remember_contracts(shared, &registry);
    Ok(Value::Null)
}
fn json(value: impl Serialize) -> Result<Value, Error> {
    serde_json::to_value(value).map_err(engine_error)
}
impl Request {
    fn available(&self, shared: &Shared) -> bool {
        if let Request::Activate { artifact, .. }
        | Request::ActivateAt { artifact, .. }
        | Request::ConfigureBinding { artifact, .. } = self
            && shared
                .stale_artifacts
                .get()
                .is_some_and(|stale| stale.contains(artifact))
        {
            return false;
        }
        true
    }
    fn mode(&self) -> HostMode {
        match self {
            Request::Recover { .. } => HostMode::Reconcile,
            Request::Cancel { .. } | Request::CancelEffects { .. } => HostMode::Cancel,
            _ => HostMode::Dispatch,
        }
    }
}

struct Runtime<'a, S: Storage, H> {
    engine: &'a Engine<S>,
    host: &'a mut H,
    shared: &'a Shared,
}

struct Activation {
    binding: String,
    artifact: String,
    authority: Authority,
}
impl Activation {
    fn new(binding: String, artifact: String, authority: Authority) -> Self {
        Self {
            binding,
            artifact,
            authority,
        }
    }
}

struct Schedules {
    entries: Vec<model::schedules::BindingSchedule>,
    event: Option<model::ContractRef>,
}
impl Schedules {
    fn new(
        entries: Vec<model::schedules::BindingSchedule>,
        event: Option<model::ContractRef>,
    ) -> Self {
        Self { entries, event }
    }
}

impl<S: Storage, H: DomainHost> Runtime<'_, S, H> {
    fn inspect(&self, query: runner::InspectionQuery) -> Result<Value, Error> {
        json(
            self.engine
                .store
                .inspect(&self.engine.namespace, &query)
                .map_err(engine_error)?,
        )
    }
    fn binding(&self, name: &str) -> Result<Value, Error> {
        let state = self.engine.snapshot().map_err(engine_error)?;
        json(state.bindings.get(name).ok_or(Error::Invalid)?)
    }
    fn runs(&self, after: Option<String>, limit: usize) -> Result<Value, Error> {
        json(
            self.engine
                .query(&IndexQuery::Runs { after, limit })
                .map_err(engine_error)?,
        )
    }
    fn activate(&self, target: Activation, at: Option<i64>) -> Result<Value, Error> {
        let Activation {
            binding,
            artifact,
            authority,
        } = target;
        match at {
            Some(now_ms) => self
                .engine
                .activate_at(&binding, &artifact, authority, now_ms)
                .map_err(runner_error)?,
            None => self
                .engine
                .activate(&binding, &artifact, authority)
                .map_err(engine_error)?,
        };
        Ok(Value::Null)
    }
    fn configure(
        &self,
        target: Activation,
        schedules: Schedules,
        (may_activate, now_ms): (bool, i64),
    ) -> Result<Value, Error> {
        let result = self
            .engine
            .configure_binding(
                &target.binding,
                &target.artifact,
                target.authority,
                may_activate,
                &schedules.entries,
                schedules.event,
                now_ms,
            )
            .map_err(runner_error)?;
        Ok(Value::Bool(result))
    }
    fn deactivate(&self, binding: String, revision: u64, now_ms: i64) -> Result<Value, Error> {
        self.engine
            .deactivate_binding(&binding, revision, now_ms)
            .map(Value::Bool)
            .map_err(runner_error)
    }
    fn admit(
        &self,
        (binding, entry): (&str, &str),
        (input, authority): (Value, Authority),
        now_ms: i64,
    ) -> Result<Value, Error> {
        self.engine
            .admit(binding, entry, input, authority, now_ms)
            .map(Value::String)
            .map_err(engine_error)
    }
    fn admit_once(
        &self,
        request: KeyedAdmissionRequest,
        (requested, now): (Option<i64>, i64),
    ) -> Result<Value, Error> {
        json(
            self.engine
                .admit_once_at(request, requested, now)
                .map_err(runner_error)?,
        )
    }
    fn admission(&self, key: &str) -> Result<Value, Error> {
        json(self.engine.admission_receipt(key).map_err(engine_error)?)
    }
    fn event(
        &self,
        (producer, epoch): (&str, u64),
        event: EventEnvelope,
        (requested, now): (Option<i64>, i64),
    ) -> Result<Value, Error> {
        json(
            self.engine
                .admit_event_at(producer, event, epoch, requested, now)
                .map_err(runner_error)?,
        )
    }
    fn fanout(&self, key: &str, limit: usize, now_ms: i64) -> Result<Value, Error> {
        Ok(
            serde_json::json!({"admitted":self.engine.fanout(key,limit,now_ms).map_err(runner_error)?}),
        )
    }
    fn cancel_effects(&mut self, now_ms: i64) -> Result<Value, Error> {
        Ok(serde_json::json!({"cancelled":cancel_effect(self.engine,self.host,now_ms)?}))
    }
    fn cancel(&mut self, run: &str, now_ms: i64) -> Result<Value, Error> {
        self.engine.cancel(run, now_ms).map_err(engine_error)?;
        cancel_effect(self.engine, self.host, now_ms)?;
        Ok(Value::Null)
    }
    fn recover(&mut self, now_ms: i64) -> Result<Value, Error> {
        Ok(
            serde_json::json!({"reconciled":self.engine.recover(self.host,now_ms).map_err(engine_error)?}),
        )
    }

    fn run(&mut self, request: Request) -> Result<Value, Error> {
        match request {
            Inspect { query } => self.inspect(query),
            Binding { binding } => self.binding(&binding),
            Runs { after, limit } => self.runs(after, limit),
            EventStatus { event_key } => event_status(self.engine, &event_key),
            Admission { key } => self.admission(&key),
            CancelEffects { now_ms } => self.cancel_effects(now_ms),
            Recover { now_ms } => self.recover(now_ms),
            Cancel { run, now_ms } => self.cancel(&run, now_ms),
            Install { artifact } => install(self.engine, self.shared, *artifact),
            Activate {
                binding,
                artifact,
                authority,
            } => self.activate(Activation::new(binding, artifact, authority), None),
            ActivateAt {
                binding,
                artifact,
                authority,
                now_ms,
            } => self.activate(Activation::new(binding, artifact, authority), Some(now_ms)),
            ConfigureBinding {
                binding,
                artifact,
                authority,
                may_activate,
                schedules,
                interval_event,
                now_ms,
            } => self.configure(
                Activation::new(binding, artifact, authority),
                Schedules::new(schedules, interval_event),
                (may_activate, now_ms),
            ),
            DeactivateBinding {
                binding,
                revision,
                now_ms,
            } => self.deactivate(binding, revision, now_ms),
            CollectIntervalEvents { now_ms } => interval_events(self.engine, now_ms),
            Admit {
                binding,
                entry,
                input,
                authority,
                now_ms,
            } => self.admit((&binding, &entry), (input, authority), now_ms),
            AdmitOnce {
                request,
                requested_at_ms,
                now_ms,
            } => self.admit_once(*request, (requested_at_ms, now_ms)),
            Event {
                authenticated_producer: producer,
                epoch,
                event,
                requested_at_ms,
                now_ms,
            } => self.event((&producer, epoch), *event, (requested_at_ms, now_ms)),
            Fanout {
                event_key,
                limit,
                now_ms,
            } => self.fanout(&event_key, limit, now_ms),
            Tick { now_ms } => tick(self.engine, self.host, self.shared, now_ms),
            // The owner acknowledges stop only after reaching this boundary.
            Stop => Ok(Value::Null),
        }
    }
}

pub(super) fn execute<S: Storage>(
    engine: &Engine<S>,
    host: &mut impl DomainHost,
    request: Request,
    shared: &Shared,
) -> Result<Value, Error> {
    if !request.available(shared) {
        return Err(Error::Invalid);
    }
    host.set_mode(request.mode());
    Runtime {
        engine,
        host,
        shared,
    }
    .run(request)
}
