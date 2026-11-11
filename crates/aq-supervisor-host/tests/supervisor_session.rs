use anyhow::Result;
use aq_central_authority::{owner::Snapshot, *};
use aq_session_host::{
    AdapterLaunch, HarnessReadiness, HostAdapter, HostInput, HostObservation, HostPlan,
    HostStartup, HostSubject, LocalCommand, LocalEnvironment, ResourcePolicy,
};
use aq_session_protocol::LaunchManifest;
use aq_supervisor_host::{PlanAuthority, Supervisor};
use std::{path::Path, sync::Arc, time::Duration};
use uuid::Uuid;

struct Admission;
#[async_trait::async_trait]
impl PlanAuthority for Admission {
    async fn authorize(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &CentralPlan,
        _: &HostPlan,
    ) -> Result<()> {
        Ok(())
    }
}
struct FixtureAdapter;
impl HostAdapter for FixtureAdapter {
    fn prepare(&self, p: &HostPlan, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest:p.digest()?, adapter:p.harness.adapter.clone(),cli_version:p.harness.cli_version.as_str().into(),
            environment:p.harness.environment.clone(),capabilities:p.harness.required_capabilities.clone(),
            command:LocalCommand {executable:"/bin/sh".into(),arguments:vec!["-c".into(),"cat \"$AQ_SESSION_DIRECTORY/context-0.bin\" > \"$AQ_SESSION_DIRECTORY/delivered\"; exec /bin/sleep 60".into()], environment:Default::default()},
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
}
fn pin(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
struct Case {
    _db: aq_server::test_support::TestDb,
    pool: sqlx::PgPool,
    root: tempfile::TempDir,
    local: LocalEnvironment,
    central: CentralPlan,
    host: HostPlan,
    startup: Vec<u8>,
}
fn example_plan() -> CentralPlan {
    let examples: serde_json::Value = serde_json::from_str(include_str!(
        "../../aq-central-authority/artifacts/examples.json"
    ))
    .unwrap();
    let Document::Plan(mut central) =
        decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
    else {
        panic!()
    };
    central.wall_time_ms = 60_000;
    central.max_permit_ms = 30_000;
    central
}
impl Case {
    async fn new() -> Self {
        let db = aq_server::test_support::TestDb::create().await;
        drop(db.store().await);
        Self::build(db, example_plan(), b"supervisor startup\n".to_vec()).await
    }
    async fn build(
        db: aq_server::test_support::TestDb,
        central: CentralPlan,
        startup: Vec<u8>,
    ) -> Self {
        let mut central = central;
        let pool = db.pool().await;
        let root = tempfile::Builder::new()
            .prefix("aqsh-")
            .tempdir_in("/tmp")
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let legacy: LaunchManifest = serde_json::from_str(include_str!(
            "../../../vendor/aq-session-host-0.1.7/tests/fixtures/manifest.json"
        ))
        .unwrap();
        let CentralSubject::CentralConversation {
            actor_id,
            conversation_id,
            conversation_revision,
            policy,
        } = &central.subject;
        let now = aq_supervisor_host::clock().unwrap();
        let mut resources = legacy.resources;
        resources.wall_time_ms = 60_000;
        resources.stop_grace_ms = 50;
        let mut terminal = legacy.terminal;
        terminal.input_enabled = false;
        let host = HostPlan {
            host_plan_version: 1,
            subject: HostSubject::CentralConversation {
                actor_id: *actor_id,
                conversation_id: *conversation_id,
                conversation_revision: *conversation_revision,
            },
            session: central.session.clone(),
            policy: policy.clone(),
            execution: central.profiles.execution.clone(),
            input: HostInput {
                dispatch_id: central.dispatch_id,
                host_operation_id: central.host_operation_id,
                batch: ArtifactRef {
                    digest: central.batch.digest().unwrap(),
                    ..pin("batch")
                },
            },
            harness: legacy.harness,
            resources,
            terminal,
            startup: HostStartup {
                descriptor: pin("startup"),
                payload: ArtifactRef {
                    digest: Digest::of(&startup),
                    ..pin("payload")
                },
                bytes: startup.len() as u64,
            },
            machine_boot_id: now.machine_boot_id,
            admitted_boot_ms: now.boot_ms,
            deadline_boot_ms: now.boot_ms + 60_000,
        };
        central.profiles.host_plan = ArtifactRef {
            digest: host.digest().unwrap(),
            ..pin("host")
        };
        central.profiles.adapter = host.harness.adapter.clone();
        central.profiles.environment = host.harness.environment.clone();
        central.profiles.startup = host.startup.descriptor.clone();
        let version = std::process::Command::new("/usr/bin/tmux")
            .arg("-V")
            .output()
            .unwrap();
        assert!(version.status.success());
        let local = LocalEnvironment {
            resource_policy: ResourcePolicy::CallerAdmission(pin("admission")),
            workspace: root.path().canonicalize().unwrap(),
            helper_executable: env!("CARGO_BIN_EXE_aq-session-host").into(),
            tmux_executable: "/usr/bin/tmux".into(),
            tmux_version: String::from_utf8(version.stdout).unwrap().trim().into(),
            credential_profiles: vec![],
            context_material: vec![],
        };
        Self {
            _db: db,
            pool,
            root,
            local,
            central,
            host,
            startup,
        }
    }
    fn controller(&self) -> Supervisor<Admission, FixtureAdapter> {
        self.controller_with(Admission)
    }
    fn controller_with<A: PlanAuthority>(&self, authority: A) -> Supervisor<A, FixtureAdapter> {
        Supervisor::new(
            self.pool.clone(),
            self.root.path().into(),
            self.local.clone(),
            Arc::new(authority),
            Arc::new(FixtureAdapter),
        )
    }
    async fn start<A: PlanAuthority>(&self, host: &Supervisor<A, FixtureAdapter>) -> Snapshot {
        host.start(
            self.central.clone(),
            self.host.clone(),
            self.startup.clone(),
            30_000,
        )
        .await
        .unwrap()
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_session_starts_under_central_permit() {
    let case = Case::new().await;
    let host = case.controller();
    let run = case.start(&host).await;
    assert_eq!(run.run.phase(), Phase::RunningReported);
    assert_eq!(run.run.plan().session, case.central.session);
    let observed = host.observe(case.central.subject.actor_id()).await.unwrap();
    assert_eq!(observed.phase, aq_session_host::HostPhase::Running);
    assert_eq!(observed.permit_sequence, 1);
    assert_eq!(
        observed.permit_deadline_boot_ms,
        run.run.permit().unwrap().expires_boot_ms
    );
    let directory = case
        .root
        .path()
        .join(observed.guardian.intent.launch_id.to_string());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        std::fs::read(directory.join("delivered")).unwrap(),
        b"supervisor startup\n"
    );
    let stopped = host.stop(case.central.subject.actor_id()).await.unwrap();
    assert_eq!(stopped.run.phase(), Phase::StoppedReported);
    assert!(directory.exists());
}

struct InstalledPlan {
    authority: aq_supervisor_host::config::InstalledAuthority,
    host: HostPlan,
}
#[async_trait::async_trait]
impl owner::EvidenceVerifier for InstalledPlan {
    async fn authorize_plan(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        plan: &CentralPlan,
    ) -> owner::OwnerResult<()> {
        self.authority
            .authorize(tx, plan, &self.host)
            .await
            .map_err(|_| owner::OwnerError::Evidence)
    }
    async fn verify_event(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &Run,
        _: &Event,
    ) -> owner::OwnerResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn installed_supervisor_capacity_is_atomic_and_replay_survives_limit_drop() {
    use aq_supervisor_host::config::{Config, InstalledAuthority};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    let mut case = Case::new().await;
    let user = case
        ._db
        .store()
        .await
        .bootstrap_owner(
            "capacity@localhost",
            "dev",
            "capacity@localhost",
            chrono::Utc::now(),
        )
        .await
        .unwrap()
        .unwrap();
    let catalog = json!({"catalog":{"intelligence":[{"id":"engineering","version":4,"candidates":[{"provider":"openai","model":"test-model","harnesses":["codex"],"reasoning_configuration_reference":"high@1"}]}]}});
    let digest = Digest::of(b"supervisor-capacity-catalog");
    sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,$2,$3,1)")
        .bind(digest.as_str()).bind(serde_json::to_vec(&catalog).unwrap()).bind(user.id).execute(&case.pool).await.unwrap();
    case.central.profiles.catalog.digest = digest.clone();
    let config = Config {
        database_url: "postgres://unused/test".into(),
        root: case.root.path().into(),
        workspace: case.root.path().join("workspace"),
        helper: "/usr/bin/true".into(),
        tmux: "/usr/bin/tmux".into(),
        tmux_version: "fixture".into(),
        native: "/usr/bin/true".into(),
        native_digest: Digest::of(b"native"),
        homes: case.root.path().join("homes"),
        secret_file: case.root.path().join("secret"),
        model: aq_session_protocol::ModelSettings {
            provider: Text::new("openai").unwrap(),
            model: Text::new("test-model").unwrap(),
            reasoning: Text::new("high").unwrap(),
            max_output_tokens: 8192,
            spending_budget: None,
        },
        central: case.central.clone(),
        host: case.host.clone(),
        startup_file: case.root.path().join("startup"),
        resource_admission: pin("admission"),
        subscription: None,
        swarm_profile: Some(
            json!({"profile_id":"engineering","profile_version":4,"provider":"openai","model":"test-model","harness":"codex","reasoning_configuration_reference":"high@1"}),
        ),
    };
    let bytes = serde_json::to_vec(&config).unwrap();
    let path = case.root.path().join("turn.json");
    std::fs::write(&path, &bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = owner::SessionOwner::new(
        case.pool.clone(),
        InstalledPlan {
            authority: InstalledAuthority {
                path,
                digest: Digest::of(&bytes),
            },
            host: case.host.clone(),
        },
    );
    let clock = Clock {
        machine_boot_id: case.host.machine_boot_id,
        boot_ms: case.host.admitted_boot_ms,
    };
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"engineering","limit":0}]))
        .execute(&case.pool)
        .await
        .unwrap();
    // An older host has no shared admission integration. The owning database
    // rejects its new generation before any physical launch can occur.
    assert!(
        case.controller()
            .start(
                case.central.clone(),
                case.host.clone(),
                case.startup.clone(),
                30_000
            )
            .await
            .is_err()
    );
    let generations: i64 = sqlx::query_scalar("SELECT count(*) FROM central_session_generations")
        .fetch_one(&case.pool)
        .await
        .unwrap();
    assert_eq!(
        generations, 0,
        "an unreserved legacy host cannot bypass configured limits"
    );
    assert!(
        owner
            .prepare(case.central.clone(), clock.clone())
            .await
            .is_err()
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM swarm_supervisor_admissions")
        .fetch_one(&case.pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "refused preparation leaves no reservation");
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"engineering","limit":1}]))
        .execute(&case.pool)
        .await
        .unwrap();
    let prepared = owner
        .prepare(case.central.clone(), clock.clone())
        .await
        .unwrap();
    let mut conn = case.pool.acquire().await.unwrap();
    let cell = aq_swarm_settings::admission::Cell {
        provider: "openai".into(),
        intelligence_class: "engineering".into(),
    };
    assert_eq!(
        aq_swarm_settings::admission::check(&mut conn, Some(&cell))
            .await
            .unwrap(),
        Some(aq_swarm_settings::admission::CEILING)
    );
    drop(conn);
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"engineering","limit":0}]))
        .execute(&case.pool)
        .await
        .unwrap();
    assert_eq!(
        owner
            .prepare(case.central.clone(), clock.clone())
            .await
            .unwrap()
            .revision,
        prepared.revision
    );
    let cancelled = owner
        .report(
            &case.central.binding().unwrap(),
            Event::Cancel {
                reason: CancelReason::Operator,
            },
            clock,
        )
        .await
        .unwrap();
    assert_eq!(cancelled.snapshot.run.phase(), Phase::CancelledBeforeStart);
    let mut conn = case.pool.acquire().await.unwrap();
    assert!(
        aq_swarm_settings::admission::counts(&mut conn)
            .await
            .unwrap()
            .iter()
            .all(|c| c.running == 0),
        "confirmed pre-start cancellation releases capacity"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_restart_resumes_without_duplicate_session() {
    let case = Case::new().await;
    let first = case.controller();
    let started = case.start(&first).await;
    let identity = first
        .observe(case.central.subject.actor_id())
        .await
        .unwrap()
        .guardian;
    drop(first);
    let second = case.controller();
    let resumed = second
        .reconcile(case.central.subject.actor_id())
        .await
        .unwrap();
    assert_eq!(resumed.run.plan().session, started.run.plan().session);
    assert_eq!(resumed.run.permit(), started.run.permit());
    let replay = case.start(&second).await;
    assert_eq!(replay.run.plan().generation, 1);
    assert_eq!(
        second
            .observe(case.central.subject.actor_id())
            .await
            .unwrap()
            .guardian,
        identity
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM central_host_launches")
        .fetch_one(&case.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    second.stop(case.central.subject.actor_id()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_start_has_one_guardian_and_cancel_prevents_revival() {
    let case = Case::new().await;
    let a = case.controller();
    let b = case.controller();
    let (x, y) = tokio::join!(
        a.start(
            case.central.clone(),
            case.host.clone(),
            b"supervisor startup\n".to_vec(),
            30_000
        ),
        b.start(
            case.central.clone(),
            case.host.clone(),
            b"supervisor startup\n".to_vec(),
            30_000
        )
    );
    assert_eq!(x.unwrap().run.plan(), y.unwrap().run.plan());
    assert_eq!(
        a.observe(case.central.subject.actor_id())
            .await
            .unwrap()
            .guardian,
        b.observe(case.central.subject.actor_id())
            .await
            .unwrap()
            .guardian
    );
    a.stop(case.central.subject.actor_id()).await.unwrap();
    assert!(
        b.renew(case.central.subject.actor_id(), 1, 30_000)
            .await
            .is_err()
    );
    assert!(
        b.start(
            case.central.clone(),
            case.host.clone(),
            b"supervisor startup\n".to_vec(),
            30_000
        )
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_host_plan_never_creates_a_writer() {
    let mut case = Case::new().await;
    let host = case.controller();
    case.host.input.dispatch_id = Uuid::new_v4();
    assert!(
        host.start(
            case.central.clone(),
            case.host.clone(),
            b"supervisor startup\n".to_vec(),
            30_000
        )
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM central_host_launches")
        .fetch_one(&case.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_supervisor_is_stopped_and_scratch_is_retained() {
    let case = Case::new().await;
    let host = case.controller();
    host.start(
        case.central.clone(),
        case.host.clone(),
        b"supervisor startup\n".to_vec(),
        2000,
    )
    .await
    .unwrap();
    let id = host
        .observe(case.central.subject.actor_id())
        .await
        .unwrap()
        .guardian
        .intent
        .launch_id;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let stopped = host
        .reconcile(case.central.subject.actor_id())
        .await
        .unwrap();
    assert_eq!(stopped.run.phase(), Phase::StoppedReported);
    assert!(
        case.root
            .path()
            .join(id.to_string())
            .join("delivered")
            .exists()
    );
    assert!(
        host.renew(case.central.subject.actor_id(), 1, 30_000)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successor_requires_stop_and_retains_prior_generation() {
    let mut case = Case::new().await;
    let host = case.controller();
    case.start(&host).await;
    let actor = case.central.subject.actor_id();
    let original = case.central.clone();
    case.central.generation = 2;
    if let SessionIdentity::Central {
        session_id,
        generation,
        ..
    } = &mut case.central.session
    {
        *session_id = Uuid::new_v4();
        *generation = 2;
    }
    case.central.dispatch_id = Uuid::new_v4();
    case.central.host_operation_id = Uuid::new_v4();
    case.central.batch.messages.truncate(1);
    case.central.batch.messages[0].message_id = Uuid::new_v4();
    case.central.batch.messages[0].cursor = 100;
    case.central.batch.through_cursor = 100;
    case.host.session = case.central.session.clone();
    case.host.input.dispatch_id = case.central.dispatch_id;
    case.host.input.host_operation_id = case.central.host_operation_id;
    case.host.input.batch.digest = case.central.batch.digest().unwrap();
    let now = aq_supervisor_host::clock().unwrap();
    case.host.admitted_boot_ms = now.boot_ms;
    case.host.deadline_boot_ms = now.boot_ms + 60_000;
    case.central.profiles.host_plan.digest = case.host.digest().unwrap();
    assert!(
        host.start(
            case.central.clone(),
            case.host.clone(),
            b"supervisor startup\n".to_vec(),
            30_000
        )
        .await
        .is_err()
    );
    host.stop(actor).await.unwrap();
    let now = aq_supervisor_host::clock().unwrap();
    case.host.admitted_boot_ms = now.boot_ms;
    case.host.deadline_boot_ms = now.boot_ms + 60_000;
    case.central.profiles.host_plan.digest = case.host.digest().unwrap();
    let next = case.start(&host).await;
    assert_eq!(next.run.plan().generation, 2);
    let bytes: String = sqlx::query_scalar(
        "SELECT state_bytes FROM central_session_generations WHERE actor_id=$1 AND generation=1",
    )
    .bind(actor)
    .fetch_one(&case.pool)
    .await
    .unwrap();
    let Document::State(old) = decode(bytes.as_bytes()).unwrap() else {
        panic!()
    };
    assert_eq!(old.plan(), &original);
    assert_eq!(old.phase(), Phase::StoppedReported);
    host.stop(actor).await.unwrap();
}

struct Revoked;
#[async_trait::async_trait]
impl PlanAuthority for Revoked {
    async fn authorize(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &CentralPlan,
        _: &HostPlan,
    ) -> Result<()> {
        anyhow::bail!("policy revoked")
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_policy_stops_instead_of_readopting() {
    let case = Case::new().await;
    case.start(&case.controller()).await;
    let host = Supervisor::new(
        case.pool.clone(),
        case.root.path().into(),
        case.local.clone(),
        Arc::new(Revoked),
        Arc::new(FixtureAdapter),
    );
    assert_eq!(
        host.reconcile(case.central.subject.actor_id())
            .await
            .unwrap()
            .run
            .phase(),
        Phase::StoppedReported
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn physical_renewal_requires_next_sequence_and_later_deadline() {
    let case = Case::new().await;
    let host = case.controller();
    host.start(
        case.central.clone(),
        case.host.clone(),
        b"supervisor startup\n".to_vec(),
        5000,
    )
    .await
    .unwrap();
    let before = host.observe(case.central.subject.actor_id()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let next = host
        .renew(case.central.subject.actor_id(), 1, 10000)
        .await
        .unwrap();
    let after = host.observe(case.central.subject.actor_id()).await.unwrap();
    assert_eq!(after.guardian, before.guardian);
    assert_eq!(after.permit_sequence, 2);
    assert!(after.permit_deadline_boot_ms > before.permit_deadline_boot_ms);
    assert_eq!(
        after.permit_deadline_boot_ms,
        next.run.permit().unwrap().expires_boot_ms
    );
    assert!(
        host.renew(case.central.subject.actor_id(), 1, 10000)
            .await
            .is_err()
    );
    host.stop(case.central.subject.actor_id()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_guardian_blocks_successor_and_retains_scratch() {
    let case = Case::new().await;
    let host = case.controller();
    case.start(&host).await;
    let observed = host.observe(case.central.subject.actor_id()).await.unwrap();
    use aq_session_host::ownership::{signal, verified_pidfd};
    let guardian = verified_pidfd(&observed.guardian.process).unwrap();
    signal(&guardian, libc::SIGKILL).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        host.reconcile(case.central.subject.actor_id())
            .await
            .is_err()
    );
    assert_eq!(
        host.inspect(case.central.subject.actor_id())
            .await
            .unwrap()
            .run
            .phase(),
        Phase::StopUncertain
    );
    assert!(
        case.root
            .path()
            .join(observed.guardian.intent.launch_id.to_string())
            .exists()
    );
    assert!(
        host.start(
            case.central.clone(),
            case.host.clone(),
            b"supervisor startup\n".to_vec(),
            30_000
        )
        .await
        .is_err()
    );
    // Test-only cleanup by exact pidfd. This is deliberately not stopped proof.
    if let Some(pane) = observed.pane {
        for p in [pane.pane, pane.server] {
            if let Ok(fd) = verified_pidfd(&p) {
                let _ = signal(&fd, libc::SIGKILL);
            }
        }
    }
}

struct SwitchAdmission(Arc<std::sync::atomic::AtomicBool>);
#[async_trait::async_trait]
impl PlanAuthority for SwitchAdmission {
    async fn authorize(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &CentralPlan,
        _: &HostPlan,
    ) -> Result<()> {
        anyhow::ensure!(self.0.load(std::sync::atomic::Ordering::SeqCst), "revoked");
        Ok(())
    }
}
struct RevokeDuringPreparation(Arc<std::sync::atomic::AtomicBool>);
impl HostAdapter for RevokeDuringPreparation {
    fn prepare(&self, p: &HostPlan, w: &Path) -> Result<AdapterLaunch> {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        FixtureAdapter.prepare(p, w)
    }
    fn readiness(&self, o: &HostObservation) -> Result<HarnessReadiness> {
        FixtureAdapter.readiness(o)
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_before_physical_start_refuses_and_stops_guardian() {
    let case = Case::new().await;
    let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let host = Supervisor::new(
        case.pool.clone(),
        case.root.path().into(),
        case.local.clone(),
        Arc::new(SwitchAdmission(allowed.clone())),
        Arc::new(RevokeDuringPreparation(allowed)),
    );
    assert!(
        host.start(
            case.central.clone(),
            case.host.clone(),
            b"supervisor startup\n".to_vec(),
            30_000
        )
        .await
        .is_err()
    );
    assert_eq!(
        host.inspect(case.central.subject.actor_id())
            .await
            .unwrap()
            .run
            .phase(),
        Phase::StoppedReported
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM central_host_facts WHERE kind='start'")
            .fetch_one(&case.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn main_delivery_database_upgrades_without_losing_history() {
    let db = aq_server::test_support::TestDb::create().await;
    let pool = db.pool().await;
    aq_server::test_support::migrate_profile(&pool, 45)
        .await
        .unwrap();
    sqlx::query("INSERT INTO delivery_intents(operation_id,repository_id,binding_generation,operation,inputs,input_digest,payload_digest,request) VALUES($1,$2,1,'candidate.show','{}',repeat('0',64),repeat('0',64),$3)")
        .bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(vec![1u8]).execute(&pool).await.unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT to_regclass('central_session_heads') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    drop(db.store().await);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_intents")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT to_regclass('central_host_launches') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
}

/// F1b plan authority: the plan and its startup bytes must be the recorded turn.
struct Dispatched;
#[async_trait::async_trait]
impl PlanAuthority for Dispatched {
    async fn authorize(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        central: &CentralPlan,
        host: &HostPlan,
    ) -> Result<()> {
        aq_server::agent_access::supervisor_conversation::dispatch::authorize_dispatch(
            tx,
            central,
            &host.startup.payload.digest,
        )
        .await
        .map_err(|e| anyhow::anyhow!("dispatch refused: {e:?}"))
    }
}
async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<String>,
) -> (axum::http::StatusCode, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut r = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    let response = app
        .clone()
        .oneshot(
            r.body(axum::body::Body::from(body.unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({"text": String::from_utf8_lossy(&bytes)})),
    )
}
fn conversation_schema(v: &serde_json::Value) {
    let s: serde_json::Value = serde_json::from_str(include_str!(
        "../../../protocol/human-supervisor-conversation/v1/schema.json"
    ))
    .unwrap();
    assert!(aq_agent_api::check(&s, &s, v), "public schema {v}");
}
/// Pages the whole conversation forward and backward with small limits. Every
/// item (owner or supervisor) has a distinct cursor, every page respects the
/// limit, and the pages concatenate to `expected` with nothing lost or repeated.
async fn assert_history_pages_exactly(
    app: &axum::Router,
    messages: &str,
    headers: &[(&str, &str)],
    expected: &[Uuid],
) {
    let ids = |page: &serde_json::Value| -> Vec<(Uuid, String)> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["message_id"].as_str().unwrap().parse().unwrap(),
                    i["cursor"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let get = |query: String| async move {
        let (c, page) = call(app, "GET", &format!("{messages}?{query}"), headers, None).await;
        assert_eq!(c, axum::http::StatusCode::OK, "{query}: {page}");
        conversation_schema(&page);
        page
    };
    for limit in [1usize, 5, 16] {
        // Forward from the start.
        let mut seen: Vec<(Uuid, String)> = vec![];
        let mut after = "0".to_owned();
        loop {
            let page = get(format!("after={after}&limit={limit}")).await;
            let items = ids(&page);
            assert!(items.len() <= limit && !items.is_empty(), "{page}");
            seen.extend(items);
            if !page["has_more"].as_bool().unwrap() {
                assert!(page["next_cursor"].is_null(), "{page}");
                break;
            }
            after = page["next_cursor"].as_str().unwrap().to_owned();
            assert_eq!(
                &after,
                &seen.last().unwrap().1,
                "next_cursor is the last item"
            );
        }
        assert_eq!(
            seen.iter().map(|s| s.0).collect::<Vec<_>>(),
            expected,
            "forward limit {limit}"
        );
        let cursors: std::collections::HashSet<&String> = seen.iter().map(|s| &s.1).collect();
        assert_eq!(cursors.len(), expected.len(), "cursors are distinct");
        // Backward from the newest.
        let mut pages: Vec<Vec<(Uuid, String)>> = vec![];
        let mut query = format!("limit={limit}");
        loop {
            let page = get(query).await;
            let items = ids(&page);
            assert!(items.len() <= limit && !items.is_empty(), "{page}");
            let more = page["has_more"].as_bool().unwrap();
            let next = page["next_cursor"].as_str().map(str::to_owned);
            if more {
                assert_eq!(
                    next.as_ref(),
                    Some(&items[0].1),
                    "next_cursor is the oldest item"
                );
            }
            pages.push(items);
            let Some(next) = next.filter(|_| more) else {
                break;
            };
            query = format!("before={next}&limit={limit}");
        }
        let backward: Vec<Uuid> = pages.into_iter().rev().flatten().map(|s| s.0).collect();
        assert_eq!(backward, expected, "backward limit {limit}");
        // A reply cursor resumes exactly between two replies of the same turn.
        let middle = &seen[expected.len() / 2];
        let tail = get(format!("after={}&limit=16", middle.1)).await;
        assert_eq!(
            ids(&tail).first().map(|s| s.0),
            Some(expected[expected.len() / 2 + 1])
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_message_reaches_supervisor_and_reply_is_recorded() {
    use aq_server::agent_access::supervisor_conversation::{
        self as conversation, dispatch, reply, reservations::*,
    };
    use axum::http::StatusCode;
    use chrono::TimeZone;
    use serde_json::json;
    const BODY: &str = "Hello supervisor ☃\nPlease plan the next milestone.";
    let db = aq_server::test_support::TestDb::create().await;
    let store = db.store().await;
    let now = chrono::Utc.timestamp_millis_opt(1_000_000).unwrap();
    let user = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap()
        .unwrap();
    let config: aq_server::config::Config = toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
    let state = aq_server::AppState {
        store,
        identity: aq_server::identity::verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        clock: Arc::new(move || now),
        agent_access: None,
        source_retention: None,
    };
    let owner_app = conversation::routes(&state)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            aq_server::authz::csrf_guard,
        ))
        .with_state(state.clone());
    let supervisor_app = conversation::supervisor_routes(&state).with_state(state.clone());
    let as_owner = [("x-aq-dev-email", "owner@localhost"), ("x-aq-request", "1")];
    // 1. The owner sends one message through the mounted owner route.
    let (c, found) = call(
        &owner_app,
        "GET",
        "/api/human/conversations/global",
        &as_owner,
        None,
    )
    .await;
    assert_eq!(c, StatusCode::OK, "{found}");
    let conversation_id: Uuid = found["conversation"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let messages = format!("/api/human/conversations/{conversation_id}/messages");
    let message_id = Uuid::new_v4();
    let create = json!({"operation_id":Uuid::new_v4(),"message_id":message_id,"expected_conversation_revision":"1","body":BODY,"context":[],"attachments":[]});
    let (c, queued) = call(
        &owner_app,
        "POST",
        &messages,
        &as_owner,
        Some(create.to_string()),
    )
    .await;
    assert_eq!(c, StatusCode::OK, "{queued}");
    assert_eq!(queued["state"], "queued");
    // 2. Reserve it as one bounded batch for the configured supervisor actor.
    let actor = Uuid::new_v4();
    let owner = ReservationOwner::new(&state);
    owner
        .configure_actor(
            &user,
            &serde_json::to_vec(&Configure {
                operation_id: Uuid::new_v4(),
                actor_id: actor,
                expected_actor_revision: 0,
                policy: LocalPolicy {
                    schema_version: 1,
                    kind: PolicyKind::GlobalSupervisorReservation,
                    actor_id: actor,
                    conversation_id,
                    enabled: true,
                },
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let revision = owner.inspect_actor(&user, actor).await.unwrap().revision;
    let preview = owner
        .preview_batch(
            &user,
            PreviewRequest {
                actor_id: actor,
                expected_actor_revision: revision,
                scan_after: 0,
                through_cursor: None,
            },
        )
        .await
        .unwrap();
    let reserved = owner
        .reserve_batch(
            &user,
            &serde_json::to_vec(&Reserve {
                operation_id: Uuid::new_v4(),
                reservation_id: Uuid::new_v4(),
                actor_id: actor,
                expected_actor_revision: preview.actor_revision,
                expected_policy_revision: preview.policy_revision,
                scan_after: preview.scan_after,
                through_cursor: preview.through_cursor,
                selection_digest: preview.selection_digest.clone(),
            })
            .unwrap(),
        )
        .await
        .unwrap()
        .reservation
        .unwrap();
    // 3. Record the turn (dispatch intent + exact host input) before any handoff.
    let mut central = example_plan();
    let CentralSubject::CentralConversation {
        actor_id,
        conversation_id: subject_conversation,
        ..
    } = &mut central.subject;
    *actor_id = actor;
    *subject_conversation = conversation_id;
    let SessionIdentity::Central { session_id, .. } = central.session else {
        panic!("central session")
    };
    let pool = db.pool().await;
    let turn = dispatch::prepare_turn(
        &pool,
        actor,
        reserved.id,
        central.generation,
        session_id,
        1_000_000,
    )
    .await
    .unwrap();
    assert_eq!(turn.batch.messages.len(), 1);
    assert_eq!(turn.batch.messages[0].message_id, message_id);
    assert!(
        String::from_utf8(turn.input.clone())
            .unwrap()
            .contains(BODY)
    );
    let replayed = dispatch::prepare_turn(
        &pool,
        actor,
        reserved.id,
        central.generation,
        session_id,
        1_000_001,
    )
    .await
    .unwrap();
    assert_eq!(
        replayed, turn,
        "the recorded turn is replayed, never re-rendered"
    );
    central.dispatch_id = turn.dispatch_id;
    central.host_operation_id = turn.host_operation_id;
    central.batch = turn.batch.clone();
    let plan_wire = serde_json::to_string(&central).unwrap();
    assert!(
        !plan_wire.contains("Please plan"),
        "plans carry refs, never bodies"
    );
    // 4. Start the F1a session on that exact plan; the host stages the turn.
    let case = Case::build(db, central, turn.input.clone()).await;
    assert!(!dispatch::live(&case.pool).await);
    let host = case.controller_with(Dispatched);
    let started = case.start(&host).await;
    assert_eq!(started.run.phase(), Phase::RunningReported);
    assert!(dispatch::live(&case.pool).await);
    let observed = host.observe(actor).await.unwrap();
    let directory = case
        .root
        .path()
        .join(observed.guardian.intent.launch_id.to_string());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        std::fs::read(directory.join("delivered")).unwrap(),
        turn.input
    );
    // Host Running is not delivery: until the handoff proof the owner sees pending.
    let (_, history) = call(&owner_app, "GET", &messages, &as_owner, None).await;
    assert_eq!(history["items"][0]["host_delivery"], "pending", "{history}");
    // Uncertain handoff must not issue a credential or start native reply access.
    let staged_input = directory.join("context-0.bin");
    std::fs::write(&staged_input, b"mismatched input").unwrap();
    assert!(host.enable_replies(actor, 998_999).await.is_err());
    assert!(!directory.join("supervisor.credential").exists());
    let issued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM central_supervisor_credentials WHERE actor_id=$1")
            .bind(actor)
            .fetch_one(&case.pool)
            .await
            .unwrap();
    assert_eq!(issued, 0);
    assert!(matches!(
        host.inspect(actor).await.unwrap().run.dispatch(),
        DispatchState::Unknown { .. }
    ));
    std::fs::write(&staged_input, &turn.input).unwrap();
    // 5. Use the binary's activation path: verified handoff, issue and install.
    let path = host.enable_replies(actor, 999_000).await.unwrap();
    let handed = host.inspect(actor).await.unwrap();
    let DispatchState::Resolved { evidence } = handed.run.dispatch() else {
        panic!("handoff resolved: {:?}", handed.run.dispatch())
    };
    assert_eq!(evidence.disposition, HandoffDisposition::HandedOff);
    assert_eq!(
        host.handoff(actor).await.unwrap().run.dispatch(),
        handed.run.dispatch()
    );
    // 6. One scoped credential, bound to this generation, only beside the session.
    assert!(path.starts_with(&directory));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let token = std::fs::read_to_string(&path).unwrap();
    // Controller re-adoption must not revoke the token held by the native child.
    let resumed = case.controller_with(Dispatched);
    assert_eq!(resumed.enable_replies(actor, 999_001).await.unwrap(), path);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), token);
    let issued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM central_supervisor_credentials WHERE actor_id=$1")
            .bind(actor)
            .fetch_one(&case.pool)
            .await
            .unwrap();
    assert_eq!(issued, 1, "restart reuses the installed credential");
    let lifetime: i64 = sqlx::query_scalar(
        "SELECT expires_at_ms-issued_at_ms FROM central_supervisor_credentials WHERE actor_id=$1",
    )
    .bind(actor)
    .fetch_one(&case.pool)
    .await
    .unwrap();
    assert!(lifetime > 0 && lifetime <= 60_000);
    // The root wrapper stages a 0600 supervisor-home copy before native spawn.
    let home = tempfile::tempdir().unwrap();
    let native_path = aq_supervisor_host::credentials::stage_supervisor_credential(
        &path,
        home.path(),
        case.host.deadline_boot_ms,
    )
    .await
    .unwrap();
    let installed = std::fs::read_to_string(native_path).unwrap();
    assert_eq!(installed, token);
    let principal = reply::authenticate(&case.pool, &installed, "messaging", 1_000_000)
        .await
        .unwrap();
    assert_eq!(principal.actor_id, actor);
    assert_eq!(principal.generation, 1);
    assert_eq!(principal.scopes, reply::SCOPES.map(String::from).to_vec());
    // 7. The supervisor replies with the exact dispatch association.
    let bearer = format!("Bearer {installed}");
    let as_supervisor = [("authorization", bearer.as_str())];
    let reply_id = Uuid::new_v4();
    let request = json!({"operation_id":Uuid::new_v4(),"message_id":reply_id,"dispatch_id":turn.dispatch_id,"body":"Supervisor reply: milestone plan follows."}).to_string();
    let path_reply = "/v1/supervisor/conversation/replies";
    let (c, receipt) = call(
        &supervisor_app,
        "POST",
        path_reply,
        &as_supervisor,
        Some(request.clone()),
    )
    .await;
    assert_eq!(c, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["dispatch_id"], json!(turn.dispatch_id));
    assert_eq!(
        receipt["author"],
        json!({"kind":"central_supervisor","actor_id":actor,"generation":1,"session_id":session_id})
    );
    assert_eq!(receipt["logical_admission"], "queued");
    let (c, again) = call(
        &supervisor_app,
        "POST",
        path_reply,
        &as_supervisor,
        Some(request),
    )
    .await;
    assert_eq!((c, &again), (StatusCode::OK, &receipt), "reply replays");
    let wrong = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"dispatch_id":Uuid::new_v4(),"body":"wrong turn"}).to_string();
    let (c, _) = call(
        &supervisor_app,
        "POST",
        path_reply,
        &as_supervisor,
        Some(wrong),
    )
    .await;
    assert_eq!(c, StatusCode::CONFLICT);
    let (c, _) = call(
        &supervisor_app,
        "POST",
        path_reply,
        &as_owner,
        Some("{}".into()),
    )
    .await;
    assert_eq!(
        c,
        StatusCode::UNAUTHORIZED,
        "owner sessions cannot author replies"
    );
    // 8. The conversation shows the delivered owner message, then the reply.
    let (c, history) = call(&owner_app, "GET", &messages, &as_owner, None).await;
    assert_eq!(c, StatusCode::OK, "{history}");
    conversation_schema(&history);
    let items = history["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{history}");
    assert_eq!(items[0]["message_id"], json!(message_id));
    assert_eq!(items[0]["host_delivery"], "delivered");
    assert_eq!(items[1]["message_id"], json!(reply_id));
    assert_eq!(items[1]["author"]["kind"], "supervisor");
    assert_eq!(items[1]["dispatch"]["dispatch_id"], json!(turn.dispatch_id));
    assert_eq!(
        items[1]["dispatch"]["host_operation_id"],
        json!(turn.host_operation_id)
    );
    // 8b. Many replies to one turn: each has its own cursor and pages exactly.
    let mut expected = vec![message_id, reply_id];
    for n in 0..17 {
        let id = Uuid::new_v4();
        let more = json!({"operation_id":Uuid::new_v4(),"message_id":id,"dispatch_id":turn.dispatch_id,"body":format!("Reply part {n}")}).to_string();
        let (c, r) = call(
            &supervisor_app,
            "POST",
            path_reply,
            &as_supervisor,
            Some(more),
        )
        .await;
        assert_eq!(c, StatusCode::OK, "{r}");
        expected.push(id);
    }
    assert_history_pages_exactly(&owner_app, &messages, &as_owner, &expected).await;
    // 9. Stopping the generation ends the credential and the live capability.
    let stopped = host.stop(actor).await.unwrap();
    assert_eq!(stopped.run.phase(), Phase::StoppedReported);
    assert!(host.enable_replies(actor, 1_000_000).await.is_err());
    assert!(!dispatch::live(&case.pool).await);
    let late = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"dispatch_id":turn.dispatch_id,"body":"too late"}).to_string();
    let (c, _) = call(
        &supervisor_app,
        "POST",
        path_reply,
        &as_supervisor,
        Some(late),
    )
    .await;
    assert_eq!(c, StatusCode::UNAUTHORIZED);
    let leaked: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM central_supervisor_credentials c WHERE strpos(to_jsonb(c)::text,$1)>0)+(SELECT count(*) FROM central_host_facts f WHERE strpos(to_jsonb(f)::text,$1)>0)+(SELECT count(*) FROM mailbox_messages m WHERE strpos(to_jsonb(m)::text,$1)>0)")
        .bind(&token)
        .fetch_one(&case.pool)
        .await
        .unwrap();
    assert_eq!(
        leaked, 0,
        "the plaintext credential is never stored centrally"
    );
}
