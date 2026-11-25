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
        let host = fixture_host(&central, &startup);
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
            aq_supervisor_host::Runtime {
                root: self.root.path().into(),
                local: self.local.clone(),
            },
            Arc::new(authority),
            Arc::new(FixtureAdapter),
        )
    }
    async fn start<A: PlanAuthority>(&self, host: &Supervisor<A, FixtureAdapter>) -> Snapshot {
        host.start(
            aq_supervisor_host::Startup {
                central: self.central.clone(),
                host: self.host.clone(),
                input: self.startup.clone(),
            },
            30_000,
        )
        .await
        .unwrap()
    }
}

fn fixture_host(central: &CentralPlan, startup: &[u8]) -> HostPlan {
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
    HostPlan {
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
                digest: Digest::of(startup),
                ..pin("payload")
            },
            bytes: startup.len() as u64,
        },
        machine_boot_id: now.machine_boot_id,
        admitted_boot_ms: now.boot_ms,
        deadline_boot_ms: now.boot_ms + 60_000,
    }
}

#[path = "supervisor_session/authority.rs"]
mod authority;
#[path = "supervisor_session/capacity.rs"]
mod capacity;
#[path = "supervisor_session/conversation.rs"]
mod conversation;
#[path = "supervisor_session/lifecycle.rs"]
mod lifecycle;
#[path = "supervisor_session/renewal.rs"]
mod renewal;
#[path = "supervisor_session/upgrade.rs"]
mod upgrade;
