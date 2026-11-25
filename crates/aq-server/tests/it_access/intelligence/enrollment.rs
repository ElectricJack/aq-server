use super::*;
use aq_worker_protocol::{NodeIdentity, RegisterRequest};

pub(super) struct Node {
    pub(super) identity: NodeIdentity,
    token: String,
}

impl Node {
    pub(super) async fn enroll(db: &TestDb, state: &AppState, code: &str, name: &str) -> Self {
        let time = (state.clock)();
        let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE role='owner'")
            .fetch_one(&db.pool().await)
            .await
            .unwrap();
        let token = aq_server::tokens::new_node_credential();
        state
            .store
            .create_enrollment(code, owner, time, time + chrono::Duration::minutes(5))
            .await
            .unwrap();
        let node = state
            .store
            .redeem_enrollment(
                code,
                aq_server::store::NewNode {
                    name: name.into(),
                    credential_hash: aq_server::tokens::verifier(&token),
                    worker_version: "0.1.0".into(),
                    protocol_version: 1,
                },
                time,
            )
            .await
            .unwrap()
            .unwrap();
        Self {
            identity: NodeIdentity {
                node_id: node.id,
                boot_id: Uuid::new_v4(),
            },
            token,
        }
    }

    pub(super) async fn identify(&self, state: &AppState) {
        state
            .store
            .execution_register(&RegisterRequest {
                identity: self.identity.clone(),
                operation_id: Uuid::new_v4(),
                protocol_version: 2,
                worker_version: "0.1.0".into(),
                capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
            })
            .await
            .unwrap();
    }

    pub(super) fn inventory_body(&self) -> Value {
        let c = credential();
        json!({"operation_id":Uuid::new_v4(),"boot_id":self.identity.boot_id,"sequence":1,"ttl_ms":1000,"qualifications":[],"credentials":[{"profile":c.profile,"credential_revision":1,"available":true}]})
    }

    async fn submit(&self, worker: &Router, body: Value) -> (StatusCode, Value) {
        machine(
            worker,
            "/v1/worker/intelligence/inventory",
            &self.token,
            body,
        )
        .await
    }

    pub(super) async fn verify_inventory(&self, worker: &Router, body: &Value) -> Value {
        let (status, first) = self.submit(worker, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["credential_bindings"][0]["available"], true);
        first
    }

    pub(super) async fn verify_replay(&self, worker: &Router, body: &Value, first: Value) {
        assert_eq!(self.submit(worker, body.clone()).await.1, first);
        let mut changed = body.clone();
        changed["ttl_ms"] = 2000.into();
        assert_eq!(self.submit(worker, changed).await.0, StatusCode::CONFLICT);
        let mut stale = body.clone();
        stale["operation_id"] = json!(Uuid::new_v4());
        stale["boot_id"] = json!(Uuid::new_v4());
        assert_eq!(self.submit(worker, stale).await.0, StatusCode::CONFLICT);
    }

    pub(super) async fn verify_legacy_feedback(&self, worker: &Router) {
        let c = credential();
        let feedback = json!({"schema_version":1,"operation_id":Uuid::new_v4(),"attempt_id":Uuid::new_v4(),"selection_digest":Digest::of(b"unsealed"),"node_id":self.identity.node_id,"boot_id":self.identity.boot_id,"classifier":c.profile,"sequence":1,"failure":"quota","lifecycle_event_id":null});
        let (status, error) = machine(
            worker,
            "/v1/worker/intelligence/failure",
            &self.token,
            feedback,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(error.to_string().contains("no actual sealed"));
    }
}

/// Registers the registry first, then enrolls a node that no list names: the
/// node must not need any configuration edit or restart (spec 0007 Q3).
pub(super) async fn enrolled_inventory(
    allow_enrolled_nodes: bool,
    revoke: bool,
) -> (StatusCode, Value, StatusCode) {
    inventory_of(allow_enrolled_nodes, revoke, true).await
}
/// `coded: false` drops the node's enrollment code: a node row that was not
/// enrolled with an owner's code (review I6).
pub(super) async fn inventory_of(
    allow_enrolled_nodes: bool,
    revoke: bool,
    coded: bool,
) -> (StatusCode, Value, StatusCode) {
    let mut reg = registry(None);
    reg.credentials[0].allow_enrolled_nodes = allow_enrolled_nodes;
    let (db, state, admin) = setup(reg).await;
    assert_eq!(register(&admin, Session::Owner).await, StatusCode::OK);
    let node = Node::enroll(&db, &state, "q3-code", "q3").await;
    if !coded {
        sqlx::query("DELETE FROM enrollment_codes")
            .execute(&db.pool().await)
            .await
            .unwrap();
    }
    node.identify(&state).await;
    let revoked = if revoke {
        call(
            &admin,
            &format!("/api/workers/{}/revoke", node.identity.node_id),
            Some(json!({})),
            Session::Owner,
        )
        .await
        .0
    } else {
        StatusCode::OK
    };
    let worker = aq_server::build_worker_router(state);
    let (status, answer) = node.submit(&worker, node.inventory_body()).await;
    (status, answer, revoked)
}
