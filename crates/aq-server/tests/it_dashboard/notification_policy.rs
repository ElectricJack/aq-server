//! F4 automatic notification policy against isolated PostgreSQL. Fake transport
//! only: these tests never dispatch HTTP.
use aq_lifecycle_api::Scope;
use aq_notifications::{RetryPolicy, Route};
use aq_server::{
    notifications::{
        config::Loaded,
        persistence::{RouteRequest, Service},
        policy::{Event, Kind, Policy, Transition},
    },
    test_support::TestDb,
};
use chrono::{TimeZone, Utc};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use uuid::Uuid;

struct Env {
    _db: TestDb,
    policy: Policy,
    scope: Scope,
    time: Arc<AtomicI64>,
}
impl Env {
    async fn new(routed: bool) -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        let pool = db.pool().await;
        let owner = store
            .bootstrap_owner(
                "policy-owner@localhost",
                "dev",
                "policy-owner@localhost",
                Utc.timestamp_millis_opt(1000).unwrap(),
            )
            .await
            .unwrap()
            .unwrap()
            .id;
        let scope = Scope {
            project_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
        };
        sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
            .bind(scope.project_id).bind(json!({"enabled":true,"graph_workspace_id":scope.workspace_id})).execute(&pool).await.unwrap();
        // Window-aligned start so a burst stays inside one flood window.
        let time = Arc::new(AtomicI64::new(600_000));
        let t = time.clone();
        let service = Service {
            pool,
            configuration: routed.then(|| {
                Arc::new(Loaded::fixture(
                    1,
                    "discord-test",
                    1,
                    "http://127.0.0.1:1/api/v10/webhooks/42/fixture_token_1234567890",
                ))
            }),
            public_origin: "https://aq.example".into(),
            clock: Arc::new(move || Utc.timestamp_millis_opt(t.load(Ordering::SeqCst)).unwrap()),
        };
        service.install(Uuid::new_v4()).await.unwrap();
        if routed {
            Self::route(&service, owner, &scope).await;
        }
        Self {
            _db: db,
            policy: Policy::new(service),
            scope,
            time,
        }
    }
    async fn route(service: &Service, owner: Uuid, scope: &Scope) {
        service
            .set_route(
                owner,
                RouteRequest {
                    operation_id: Uuid::new_v4(),
                    expected_revision: 0,
                    route: Route {
                        route_id: Uuid::new_v4(),
                        revision: 1,
                        scope: scope.clone(),
                        recipient_user_id: owner,
                        endpoint_key: "discord-test".into(),
                        endpoint_revision: 1,
                        enabled: true,
                        retry: RetryPolicy {
                            max_attempts: 3,
                            max_elapsed_ms: 30000,
                            initial_delay_ms: 250,
                            max_delay_ms: 2000,
                            allow_duplicate_risk: false,
                        },
                    },
                },
            )
            .await
            .unwrap();
    }
    fn origin(&self, source: Uuid) -> aq_notifications::policy::Origin {
        aq_notifications::policy::Origin::new(source, self.scope.clone())
    }
    fn event(&self, kind: Kind, subject: &str, transition: Transition) -> Event {
        Event {
            source_event_id: Uuid::new_v4(),
            kind,
            subject: subject.into(),
            scope: self.scope.clone(),
            transition,
            title: format!("{subject} needs attention"),
        }
    }
    async fn messages(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT definition->>'summary' FROM notification_messages ORDER BY sequence",
        )
        .fetch_all(&self.policy.service().pool)
        .await
        .unwrap()
    }
    async fn operations(&self) -> i64 {
        sqlx::query_scalar("SELECT operation_count FROM notification_budget WHERE singleton")
            .fetch_one(&self.policy.service().pool)
            .await
            .unwrap()
    }
}

mod episodes;
mod producers;
mod retention;
