use super::*;

enum Journal {
    Dashboard,
    Native,
    Both,
}

impl Journal {
    fn for_sample(n: u128) -> Self {
        match n {
            1 | 104 | 106.. => Self::Both,
            103 | 105 => Self::Native,
            0..=99 if n.is_multiple_of(3) => Self::Native,
            _ => Self::Dashboard,
        }
    }
}

async fn seed_operations(f: &common::profiled::Fixture) -> Vec<String> {
    use aq_server::dashboard_api::actions::{Disposition, Operation, Recovery};
    use aq_server::dashboard_api::contract::Scope;
    let pool = f.db.pool().await;
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(other).execute(&pool).await.unwrap();
    let mut expected = vec![];
    for n in 0..106u128 {
        let id = Uuid::from_u128(n + 1);
        let principal = if n >= 104 { other } else { f.actor };
        let disposition = match n {
            102 => Disposition::Confirmed,
            103 => Disposition::Rejected,
            n if n % 2 == 0 => Disposition::Pending,
            _ => Disposition::Unknown,
        };
        if n < 100 {
            expected.push(id.to_string());
        }
        let op = Operation {
            operation_id: id,
            disposition,
            request_digest: format!("sha256:{}", "a".repeat(64)),
            scope: Scope::AllAuthorized,
            created_at: "1970-01-01T00:16:40.000Z".into(),
            updated_at: "1970-01-01T00:16:40.000Z".into(),
            subject_refs: vec![],
            receipt_refs: vec![],
            affected_entities: vec![],
            recovery: Recovery::LookupOriginal,
            current_versions: Default::default(),
            reason: None,
            activation: None,
        };
        let source = Journal::for_sample(n);
        if matches!(source, Journal::Dashboard | Journal::Both) {
            sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at) VALUES($1,$2,'supervisor.message.send',$3,$4,$5,$6,to_timestamp(1000),to_timestamp(1000))")
            .bind(id).bind(principal).bind(json!(op.scope)).bind(&op.request_digest).bind(serde_json::to_value(disposition).unwrap().as_str().unwrap()).bind(json!(op)).execute(&pool).await.unwrap();
        }
        // Discovery reads only native admission metadata. These rows deliberately
        // omit the rest of the native evidence; real decoding is tested with Graph.
        if matches!(source, Journal::Native | Journal::Both) {
            sqlx::query("INSERT INTO human_graph_operations(operation_id,actor_id,project_id,project_revision,project_binding,workspace_id,task_id,request_bytes,request_digest,payload_bytes,payload_digest,service_identity,status,created_at_ms,expires_at_ms,retained_bytes) VALUES($1,$2,$3,1,'{}',$4,$1,$5,$6,$5,$6,'{}',$7,1000000,1001000,1)")
                .bind(id).bind(principal).bind(f.selection.lineage.project_id).bind(f.selection.lineage.workspace_id).bind(b"{}".as_slice()).bind(format!("sha256:{}", "a".repeat(64))).bind(json!({"state":serde_json::to_value(disposition).unwrap()})).execute(&pool).await.unwrap();
        }
    }
    expected
}

#[tokio::test]
async fn bootstrap_lists_pending_operations() {
    let f = fixture().await;
    common::profiled::insert(&f).await;
    let expected = seed_operations(&f).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, b) = get(&app, "/api/v1/bootstrap").await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["api_version"], "dashboard-read-1");
    assert_eq!(b["pending_operations"], json!(expected));
    assert_eq!(b["coverage"]["state"], "partial");
    assert_eq!(b["data"]["contracts"][1]["name"], "dashboard-actions");
    assert_eq!(b["data"]["contracts"][1]["version"], "dashboard-actions-1");
    let bytes = aq_server::dashboard_api::actions::contract::schema_bytes();
    use sha2::{Digest, Sha256};
    assert_eq!(
        b["data"]["contracts"][1]["digest"],
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    );
    let (_, other_bootstrap) = call(&app, "/api/v1/bootstrap", None, "other@localhost").await;
    assert_eq!(
        other_bootstrap["pending_operations"],
        json!([Uuid::from_u128(105), Uuid::from_u128(106)])
    );
}
