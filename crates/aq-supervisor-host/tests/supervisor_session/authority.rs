use super::*;

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
        aq_supervisor_host::Runtime {
            root: case.root.path().into(),
            local: case.local.clone(),
        },
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
    let mut case = Case::new().await;
    let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let host = Supervisor::new(
        case.pool.clone(),
        aq_supervisor_host::Runtime {
            root: case.root.path().into(),
            local: case.local.clone(),
        },
        Arc::new(SwitchAdmission(allowed.clone())),
        Arc::new(RevokeDuringPreparation(allowed)),
    );
    assert!(
        host.start(
            aq_supervisor_host::Startup {
                central: case.central.clone(),
                host: case.host.clone(),
                input: b"supervisor startup\n".to_vec()
            },
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
    // Recover against the same mail batch using a new, independently admitted generation.
    let next = case.controller();
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
    case.host.session = case.central.session.clone();
    case.host.input.dispatch_id = case.central.dispatch_id;
    case.host.input.host_operation_id = case.central.host_operation_id;
    let now = aq_supervisor_host::clock().unwrap();
    case.host.admitted_boot_ms = now.boot_ms;
    case.host.deadline_boot_ms = now.boot_ms + case.central.wall_time_ms;
    case.central.profiles.host_plan.digest = case.host.digest().unwrap();
    let state = case.start(&next).await;
    assert_eq!(state.run.phase(), Phase::RunningReported);
    next.stop(case.central.subject.actor_id()).await.unwrap();
}
