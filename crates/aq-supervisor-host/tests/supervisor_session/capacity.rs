use super::*;
use serde_json::json;

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
    let case = Case::new().await;
    let owner = installed_owner(&case);
    let clock = Clock {
        machine_boot_id: case.host.machine_boot_id,
        boot_ms: case.host.admitted_boot_ms,
    };
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"engineering","limit":0}]))
        .execute(&case.pool)
        .await
        .unwrap();
    assert_admission_refused(&case, &owner, &clock).await;
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"engineering","limit":1}]))
        .execute(&case.pool)
        .await
        .unwrap();
    let prepared = owner
        .prepare(case.central.clone(), clock.clone())
        .await
        .unwrap();
    assert_provider_ceiling(&case).await;
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

fn installed_owner(case: &Case) -> owner::SessionOwner<InstalledPlan> {
    use aq_supervisor_host::config::{Config, InstalledAuthority};
    use std::os::unix::fs::PermissionsExt;
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
            json!({"profile_id":"engineering","candidate_id":"openai:test-model:high","provider":"openai","model":"test-model","reasoning":"high","harness":"codex","snapshot_digest":Digest::of(b"supervisor-capacity-snapshot")}),
        ),
    };
    let bytes = serde_json::to_vec(&config).unwrap();
    let path = case.root.path().join("turn.json");
    std::fs::write(&path, &bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    owner::SessionOwner::new(
        case.pool.clone(),
        InstalledPlan {
            authority: InstalledAuthority {
                path,
                digest: Digest::of(&bytes),
            },
            host: case.host.clone(),
        },
    )
}

async fn assert_admission_refused(
    case: &Case,
    owner: &owner::SessionOwner<InstalledPlan>,
    clock: &Clock,
) {
    // An older host has no shared admission integration. The owning database
    // rejects its new generation before any physical launch can occur.
    assert!(
        case.controller()
            .start(
                aq_supervisor_host::Startup {
                    central: case.central.clone(),
                    host: case.host.clone(),
                    input: case.startup.clone()
                },
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
}

async fn assert_provider_ceiling(case: &Case) {
    let version: i32 =
        sqlx::query_scalar("SELECT admission_version FROM swarm_supervisor_admissions")
            .fetch_one(&case.pool)
            .await
            .unwrap();
    assert_eq!(
        version, 2,
        "installed host certifies independent capacity admission"
    );
    sqlx::query("UPDATE swarm_settings SET limits=$1 WHERE singleton")
        .bind(json!({"provider_limits":[{"provider":"openai","limit":0}]}))
        .execute(&case.pool)
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
}
