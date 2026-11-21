//! Fake-peer conformance only. This fixture is never a daemon registration.
mod common;
use aq_playbook_host::{
    model::*,
    runner::{Authority, DispatchResult, Host, Intent, KeyedAdmissionRequest, Storage},
    *,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};
const DEADLINE: Duration = Duration::from_secs(5);
fn contract() -> CommandContract {
    CommandContract {
        name: "message.send".into(),
        version: "fixture-only".into(),
        input: Schema::Null,
        outcomes: BTreeMap::from([("queued".into(), Schema::Null)]),
        capabilities: BTreeSet::new(),
        scopes: BTreeSet::from(["project".into()]),
        effect: Effect::Mutation,
        recovery: Recovery::Reconcilable,
        asynchronous: None,
        cancellation: true,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 1024,
        deadline_ms: 1000,
    }
}
#[derive(Clone, Default)]
struct Counts {
    dispatch: Arc<AtomicUsize>,
    reconcile: Arc<AtomicUsize>,
    revoked: Arc<AtomicBool>,
    clock: Arc<AtomicI64>,
}
struct Peer {
    counts: Counts,
    mode: HostMode,
}
impl Host for Peer {
    fn authorize(&self, _: &Authority, c: &CommandContract) -> bool {
        c == &contract()
            && (self.mode == HostMode::Reconcile || !self.counts.revoked.load(Ordering::SeqCst))
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        if intent.step == "prepare" {
            self.counts.clock.store(12_000, Ordering::SeqCst);
            return DispatchResult::Completed {
                outcome: "queued".into(),
                output: Value::Null,
            };
        }
        assert_eq!(
            intent.deadline_at_ms,
            12_000 + contract().deadline_ms as i64
        );
        self.counts.dispatch.fetch_add(1, Ordering::SeqCst);
        DispatchResult::Unknown {
            reason: "synthetic response loss".into(),
        }
    }
    fn reconcile(&mut self, _: &Intent) -> DispatchResult {
        assert_eq!(self.mode, HostMode::Reconcile);
        self.counts.reconcile.fetch_add(1, Ordering::SeqCst);
        DispatchResult::Completed {
            outcome: "queued".into(),
            output: Value::Null,
        }
    }
}
impl DomainHost for Peer {
    fn transition_time(&self, now_ms: i64) -> i64 {
        now_ms.max(self.counts.clock.load(Ordering::SeqCst))
    }
    fn set_mode(&mut self, mode: HostMode) {
        self.mode = mode
    }
}
#[tokio::test]
async fn explicit_registry_keyed_replay_and_reconcile_mode_do_not_redispatch() {
    let counts = Counts::default();
    let host_counts = counts.clone();
    let store = MemoryStorage::default();
    let check = store.clone();
    let c = contract();
    let registry = Registry {
        commands: BTreeMap::from([(c.name.clone(), c.clone())]),
        events: BTreeMap::new(),
    };
    let owner = Owner::start_registered(
        "registered".into(),
        aq_playbook_host::Limits::default(),
        registry.clone(),
        move || Ok(store),
        move |acquired| {
            assert_eq!(acquired.namespace, "registered");
            uuid::Uuid::parse_str(&acquired.incarnation).unwrap();
            Ok(Peer {
                counts: host_counts,
                mode: HostMode::Dispatch,
            })
        },
    )
    .await
    .unwrap();
    let mut a = common::artifact();
    a.content.registry = registry;
    a.content.playbook.entries.get_mut("main").unwrap().start = "prepare".into();
    a.content.playbook.steps.insert(
        "send".into(),
        Step {
            op: Operation::Invoke {
                command: ContractRef {
                    name: c.name.clone(),
                    fingerprint: c.fingerprint().unwrap(),
                },
                input: Expr::literal(Value::Null),
                bind: "result".into(),
                retry: None,
            },
            edges: BTreeMap::from([("queued".into(), "done".into())]),
            on_error: Some("done".into()),
        },
    );
    // The first host call takes longer than the following command's entire
    // timeout. The second intent must still get its full dispatch window.
    let mut prepare = a.content.playbook.steps["send"].clone();
    let Operation::Invoke { bind, .. } = &mut prepare.op else {
        unreachable!()
    };
    *bind = "prepared".into();
    prepare.edges.insert("queued".into(), "send".into());
    a.content.playbook.steps.insert("prepare".into(), prepare);
    let a = Artifact::seal(a.content).unwrap();
    let digest = a.digest.clone();
    let mut altered = a.clone();
    altered
        .content
        .registry
        .commands
        .get_mut("message.send")
        .unwrap()
        .deadline_ms += 1;
    assert!(matches!(
        owner
            .call(
                Request::Install {
                    artifact: Box::new(altered)
                },
                DEADLINE
            )
            .await,
        Err(Error::Invalid)
    ));
    owner
        .call(
            Request::Install {
                artifact: Box::new(a),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Activate {
                binding: "test".into(),
                artifact: digest,
                authority: common::authority(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    let activation = owner
        .call(
            Request::Binding {
                binding: "test".into(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    let request = KeyedAdmissionRequest {
        key: "opaque-operation".into(),
        binding: "test".into(),
        expected_activation_revision: activation["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: Value::Null,
        authority: common::authority(),
    };
    let first = owner
        .call(
            Request::AdmitOnce {
                request: Box::new(request.clone()),
                requested_at_ms: None,
                now_ms: 1,
            },
            DEADLINE,
        )
        .await
        .unwrap();
    let replay = owner
        .call(
            Request::AdmitOnce {
                request: Box::new(request),
                requested_at_ms: None,
                now_ms: 2,
            },
            DEADLINE,
        )
        .await
        .unwrap();
    assert_eq!(first["receipt"], replay["receipt"]);
    assert_eq!(replay["duplicate"], true);
    owner
        .call(Request::Tick { now_ms: 3 }, DEADLINE)
        .await
        .unwrap();
    assert_eq!(counts.dispatch.load(Ordering::SeqCst), 1);
    counts.revoked.store(true, Ordering::SeqCst);
    owner
        .call(Request::Recover { now_ms: 12_001 }, DEADLINE)
        .await
        .unwrap();
    assert_eq!(counts.reconcile.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dispatch.load(Ordering::SeqCst), 1);
    assert_eq!(check.load("registered").unwrap().state.runs.len(), 1);
}
#[tokio::test]
async fn generic_command_and_unsafe_recovery_catalogs_refuse_before_factory() {
    for name in ["wildcard.proxy", "message.send"] {
        let mut c = contract();
        c.name = name.into();
        if name == "message.send" {
            c.recovery = Recovery::Unsafe;
        }
        let counts = Counts::default();
        let observed = counts.dispatch.clone();
        let factory_counts = counts.clone();
        let result = Owner::start_registered(
            "invalid".into(),
            aq_playbook_host::Limits::default(),
            Registry {
                commands: BTreeMap::from([(c.name.clone(), c)]),
                events: BTreeMap::new(),
            },
            move || {
                factory_counts.dispatch.fetch_add(1, Ordering::SeqCst);
                Ok(MemoryStorage::default())
            },
            move |_| {
                Ok(Peer {
                    counts,
                    mode: HostMode::Dispatch,
                })
            },
        )
        .await;
        assert!(matches!(result, Err(Error::Invalid)));
        assert_eq!(observed.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn d4_host_catalog_recovery_classes_are_admitted() {
    let registry: Registry = serde_json::from_str(include_str!(
        "../../../vendor/aq-host-catalog-1.0.0/registry.json"
    ))
    .unwrap();
    assert_eq!(
        digest(&("1.0.0", &registry)).unwrap(),
        "sha256:627125cc393d8274bfaa9ffc785c338e566cc69af362ac027a3daa355c3509e1"
    );
    let counts = Counts::default();
    let owner = Owner::start_registered(
        "d4-catalog".into(),
        aq_playbook_host::Limits::default(),
        registry,
        || Ok(MemoryStorage::default()),
        move |_| {
            Ok(Peer {
                counts,
                mode: HostMode::Dispatch,
            })
        },
    )
    .await;
    assert!(owner.is_ok());
    // Host catalog 2.2.0 (sixteen commands) is admitted whole.
    let registry: Registry = serde_json::from_str(include_str!(
        "../../../vendor/aq-host-catalog-2.2.0/registry.json"
    ))
    .unwrap();
    assert_eq!(registry.commands.len(), 16);
    let owner = Owner::start_registered(
        "host-2-2-0".into(),
        aq_playbook_host::Limits::default(),
        registry,
        || Ok(MemoryStorage::default()),
        move |_| {
            Ok(Peer {
                counts: Counts::default(),
                mode: HostMode::Dispatch,
            })
        },
    )
    .await;
    assert!(owner.is_ok());
    // Host catalog 2.3.0 (twenty-six commands, Epic 0) is admitted whole, with
    // the feature `delivery_playbook` only.
    #[cfg(feature = "delivery_playbook")]
    {
        let registry: Registry = serde_json::from_str(include_str!(
            "../../../vendor/aq-host-catalog-2.3.0/registry.json"
        ))
        .unwrap();
        assert_eq!(registry.commands.len(), 26);
        let owner = Owner::start_registered(
            "host-2-3-0".into(),
            aq_playbook_host::Limits::default(),
            registry,
            || Ok(MemoryStorage::default()),
            move |_| {
                Ok(Peer {
                    counts: Counts::default(),
                    mode: HostMode::Dispatch,
                })
            },
        )
        .await;
        assert!(owner.is_ok());
    }
    for refused in [Recovery::Unsafe, Recovery::NaturallyIdempotent] {
        let mut c = contract();
        c.recovery = refused;
        let result = Owner::start_registered(
            "d4-refused".into(),
            aq_playbook_host::Limits::default(),
            Registry {
                commands: BTreeMap::from([(c.name.clone(), c)]),
                events: BTreeMap::new(),
            },
            || Ok(MemoryStorage::default()),
            move |_| {
                Ok(Peer {
                    counts: Counts::default(),
                    mode: HostMode::Dispatch,
                })
            },
        )
        .await;
        assert!(matches!(result, Err(Error::Invalid)));
    }
}
