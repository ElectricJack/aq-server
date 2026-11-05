//! Real PostgreSQL, synthetic lifecycle evidence; no physical host or sleeps.
use super::*;
use crate::test_support::TestDb;
use aq_central_authority::{
    ArtifactRef, CancelReason, CentralPlan, Clock, Decision, Document, Event, HostLaunchRef, Run,
    Text, decode, owner::*,
};

struct FixtureVerifier;
#[async_trait::async_trait]
impl EvidenceVerifier for FixtureVerifier {
    async fn authorize_plan(
        &self,
        _: &mut Transaction<'_, Postgres>,
        _: &CentralPlan,
    ) -> OwnerResult<()> {
        Ok(())
    }
    async fn verify_event(
        &self,
        _: &mut Transaction<'_, Postgres>,
        _: &Run,
        _: &Event,
    ) -> OwnerResult<()> {
        Ok(())
    }
}
fn clock(ms: u64) -> Clock {
    Clock {
        machine_boot_id: Uuid::from_u128(9),
        boot_ms: ms,
    }
}
fn artifact() -> ArtifactRef {
    ArtifactRef {
        id: Text::new("fixture").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"fixture"),
    }
}
struct Fixture {
    db: TestDb,
    pool: PgPool,
    owner: SessionOwner<FixtureVerifier>,
    snapshot: Snapshot,
    launch: HostLaunchRef,
}
impl Fixture {
    async fn new(version: u32) -> Self {
        let db = TestDb::create().await;
        drop(db.store_at_version(version).await);
        let pool = db.pool().await;
        let examples: Value = serde_json::from_str(include_str!(
            "../../../../../aq-central-authority/artifacts/examples.json"
        ))
        .unwrap();
        let Document::Plan(plan) = decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
        else {
            panic!("plan")
        };
        let owner = SessionOwner::new(pool.clone(), FixtureVerifier);
        owner.prepare(plan.clone(), clock(1000)).await.unwrap();
        let launch = HostLaunchRef {
            binding: plan.binding().unwrap(),
            launch_id: Uuid::new_v4(),
            intent: artifact(),
        };
        let snapshot = owner
            .start(
                &plan.binding().unwrap(),
                launch.clone(),
                60_000,
                clock(1000),
            )
            .await
            .unwrap()
            .snapshot;
        Self {
            db,
            pool,
            owner,
            snapshot,
            launch,
        }
    }
    fn actor(&self) -> Uuid {
        self.snapshot.run.plan().subject.actor_id()
    }
    async fn token(&self) -> String {
        issue_credential(&self.pool, self.actor(), 60_000, 1000)
            .await
            .unwrap()
    }
    async fn report(&self, event: Event, ms: u64) {
        let r = self
            .owner
            .report(&self.launch.binding, event, clock(ms))
            .await
            .unwrap();
        assert_eq!(r.decision, Decision::Applied);
    }
    async fn revoked(&self) -> Option<i64> {
        sqlx::query_scalar("SELECT revoked_at_ms FROM central_supervisor_credentials WHERE actor_id=$1 ORDER BY issued_at_ms DESC LIMIT 1")
            .bind(self.actor()).fetch_one(&self.pool).await.unwrap()
    }
    async fn write(&self, token: &str, principal: &SupervisorPrincipal) -> Result<ReplyReceipt> {
        let service = Service {
            pool: self.pool.clone(),
            gate: Semaphore::new(2),
        };
        let dispatch = self.snapshot.run.association().unwrap().clone();
        write(
            &service,
            principal,
            token,
            ReplyRequest {
                operation_id: Uuid::new_v4(),
                message_id: Uuid::new_v4(),
                dispatch_id: dispatch.dispatch_id,
                body: "late reply".into(),
                attachments: vec![],
            },
            dispatch,
            2000,
        )
        .await
    }
}

#[tokio::test]
async fn supervisor_credential_scopes_are_required_for_each_operation() {
    let f = Fixture::new(51).await;
    let token = f.token().await;
    for scope in SCOPES {
        assert!(
            authenticate(&f.pool, &token, scope, 2000).await.is_ok(),
            "{scope}"
        );
    }
    assert!(matches!(
        authenticate(&f.pool, &token, "unknown", 2000).await,
        Err(AppError::Forbidden(_))
    ));
    assert!(matches!(
        authenticate(&f.pool, &token, "messaging", 61_000).await,
        Err(AppError::Unauthenticated(_))
    ));
    // Reduced credentials must not acquire any of the other issued scopes.
    for allowed in SCOPES {
        sqlx::query("UPDATE central_supervisor_credentials SET revoked_at_ms=2000 WHERE actor_id=$1 AND revoked_at_ms IS NULL")
            .bind(f.actor()).execute(&f.pool).await.unwrap();
        let limited = format!(
            "aqsv1_{}",
            hex::encode(sha2::Sha256::digest(allowed.as_bytes()))
        );
        sqlx::query("INSERT INTO central_supervisor_credentials(token_digest,actor_id,generation,session_id,scopes,issued_at_ms,expires_at_ms) VALUES($1,$2,1,$3,$4,1000,61000)")
            .bind(token_digest(&limited)).bind(f.actor()).bind(dispatch::session_id(f.snapshot.run.plan()).unwrap())
            .bind(vec![allowed.to_string()]).execute(&f.pool).await.unwrap();
        for required in SCOPES {
            let result = authenticate(&f.pool, &limited, required, 2000).await;
            if required == allowed {
                assert!(result.is_ok());
            } else {
                assert!(
                    matches!(result, Err(AppError::Forbidden(_))),
                    "{allowed} cannot grant {required}"
                );
            }
        }
    }
}

#[tokio::test]
async fn supervisor_credentials_revoke_on_stop_loss_and_expiry() {
    for end in ["stop", "lost", "expiry"] {
        let f = Fixture::new(51).await;
        let token = f.token().await;
        let (event, at) = match end {
            "stop" => (
                Event::Cancel {
                    reason: CancelReason::Operator,
                },
                2000,
            ),
            "lost" => (
                Event::HostLost {
                    binding: f.launch.binding.clone(),
                    launch_id: f.launch.launch_id,
                    report: artifact(),
                },
                2000,
            ),
            _ => (Event::Tick, 61_000),
        };
        assert_eq!(f.revoked().await, None);
        f.report(event, at).await;
        let revoked = f.revoked().await.unwrap();
        assert!(revoked > 0);
        assert!(matches!(
            authenticate(&f.pool, &token, "messaging", 2000).await,
            Err(AppError::Unauthenticated(_))
        ));
        assert!(
            issue_credential(&f.pool, f.actor(), 60_000, 2000)
                .await
                .is_err()
        );
        // Further inactive transitions preserve the first revocation timestamp.
        f.owner
            .report(&f.launch.binding, Event::Tick, clock(at + 1))
            .await
            .unwrap();
        assert_eq!(f.revoked().await, Some(revoked));
    }
}

#[tokio::test]
async fn supervisor_reply_rechecks_lifecycle_and_token_inside_write() {
    let f = Fixture::new(51).await;
    let token = f.token().await;
    let principal = authenticate(&f.pool, &token, "messaging", 2000)
        .await
        .unwrap();
    // Reissue after initial authentication but before opening the reply write.
    let replacement = f.token().await;
    assert!(matches!(
        f.write(&token, &principal).await,
        Err(AppError::Unauthenticated(_))
    ));
    let principal = authenticate(&f.pool, &replacement, "messaging", 2000)
        .await
        .unwrap();
    f.report(
        Event::Cancel {
            reason: CancelReason::Operator,
        },
        2000,
    )
    .await;
    assert!(matches!(
        f.write(&replacement, &principal).await,
        Err(AppError::Unauthenticated(_))
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM central_supervisor_replies")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn supervisor_write_authentication_holds_lifecycle_and_credential_locks() {
    let f = Fixture::new(51).await;
    let token = f.token().await;
    let mut tx = f.pool.begin().await.unwrap();
    authenticate_in_transaction(&f.pool, &mut tx, &token, "messaging", 2000)
        .await
        .unwrap();
    for sql in [
        "SELECT actor_id FROM central_session_heads WHERE actor_id=$1 FOR UPDATE NOWAIT",
        "SELECT actor_id FROM central_supervisor_credentials WHERE actor_id=$1 FOR UPDATE NOWAIT",
    ] {
        let error = sqlx::query(sql)
            .bind(f.actor())
            .fetch_one(&f.pool)
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("55P03")
        );
    }
    tx.commit().await.unwrap();
    f.report(
        Event::Cancel {
            reason: CancelReason::Operator,
        },
        2000,
    )
    .await;
    assert!(f.revoked().await.is_some());
}

#[tokio::test]
async fn supervisor_credential_upgrade_revokes_preexisting_inactive_tokens() {
    let f = Fixture::new(48).await;
    let token = f.token().await;
    f.report(
        Event::Cancel {
            reason: CancelReason::Operator,
        },
        2000,
    )
    .await;
    assert_eq!(
        f.revoked().await,
        None,
        "old format left revocation implicit"
    );
    drop(f.db.store().await);
    assert!(f.revoked().await.is_some());
    assert!(matches!(
        authenticate(&f.pool, &token, "messaging", 2000).await,
        Err(AppError::Unauthenticated(_))
    ));
}
