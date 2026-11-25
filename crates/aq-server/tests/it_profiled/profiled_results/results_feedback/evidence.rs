use super::*;

#[tokio::test]
async fn capture_policy_is_pinned_and_waiver_is_exact() {
    let (f, _, report) = prepare(true, true).await;
    let pool = f.db.pool().await;
    let requirement = json!({"id":"desktop","policy_revision":"1","kind":"image","scenario":"results","steps":["Open Results"],"viewport":{"width_css":1280,"height_css":800,"dpr_milli":1000},"environment":"test","count":1,"required":true,"applicability":"Results page changed","gate_binding":"completion"});
    sqlx::query("INSERT INTO result_task_policies(project_id,task_id,revision,requirements) VALUES($1,$2,1,$3)").bind(f.selection.lineage.project_id).bind(f.selection.lineage.task_id).bind(json!([requirement])).execute(&pool).await.unwrap();
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    sqlx::query("INSERT INTO result_task_policies(project_id,task_id,revision,requirements) VALUES($1,$2,2,'[]')").bind(f.selection.lineage.project_id).bind(f.selection.lineage.task_id).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let root = format!(
        "/api/v1/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let (_, detail) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(
        detail["data"]["revision"]["capture_policy"]["revision"],
        "1"
    );
    assert_eq!(detail["data"]["assessment"]["evidence"]["satisfied"], false);
    let path = format!("/api/v1/results/{}/evidence-waivers", report.report_id);
    let bad = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","revision":receipt.report_digest.as_str(),"requirement_id":"desktop","policy_revision":"2","reason":"Owner waiver"});
    assert_eq!(
        send(&app, "POST", &path, bad).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let good = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","revision":receipt.report_digest.as_str(),"requirement_id":"desktop","policy_revision":"1","reason":"Owner waiver"});
    assert_eq!(
        send(&app, "POST", &path, good).await.1["data"]["disposition"],
        "confirmed"
    );
    let (_, detail) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(detail["data"]["assessment"]["evidence"]["satisfied"], true);
    assert_eq!(detail["data"]["assessment"]["acceptance"], "unreviewed");
    let duplicate = json!({"operation_id":Uuid::new_v4(),"expected_version":detail["data"]["assessment"]["version"],"revision":receipt.report_digest.as_str(),"requirement_id":"desktop","policy_revision":"1","reason":"Second waiver cannot replace immutable decision"});
    let (code, refused) = send(&app, "POST", &path, duplicate).await;
    assert_eq!(code, StatusCode::OK, "{refused}");
    assert_eq!(refused["data"]["disposition"], "conflict", "{refused}");
    let retained: i64 =
        sqlx::query_scalar("SELECT count(*) FROM result_evidence_waivers WHERE report_id=$1")
            .bind(report.report_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retained, 1);
}

#[tokio::test]
async fn feedback_retained_image_anchor_and_principal_fence() {
    let (f, report, revision, owner, _root, actor) = super::super::media_owner::setup().await;
    let id = super::super::media_owner::retained(&owner, &actor, report.report_id, &revision).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let path = format!(
        "/api/v1/results/{}/revisions/{revision}/comments",
        report.report_id
    );
    let digest = aq_result_media::Digest::of(&super::super::media_owner::png())
        .as_str()
        .to_string();
    let mut body = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"image feedback","anchor":{"kind":"image_region","artifact_id":id,"digest":digest,"x":0.1,"y":0.1,"width":0.2,"height":0.3}});
    let (code, created) = send(&app, "POST", &path, body.clone()).await;
    assert_eq!(code, StatusCode::OK, "{created}");
    assert_eq!(created["data"]["disposition"], "confirmed");
    body["operation_id"] = json!(Uuid::new_v4());
    body["expected_version"] = json!("2");
    body["anchor"]["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    assert_eq!(
        send(&app, "POST", &path, body).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let pool = f.db.pool().await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(actor.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        send(&app, "GET", &path, Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, "GET", "/api/v1/results", Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn capture_counts_finalized_source_retention_and_keeps_revision_immutable() {
    let (f, _, report) = prepare(true, true).await;
    let pool = f.db.pool().await;
    let requirement = json!({"id":"desktop","policy_revision":"1","kind":"image","scenario":"results","steps":["Open Results"],"viewport":{"width_css":1280,"height_css":800,"dpr_milli":1000},"environment":"test","count":2,"required":true,"applicability":"Results changed","gate_binding":"completion"});
    sqlx::query("INSERT INTO result_task_policies VALUES($1,$2,1,$3)")
        .bind(f.selection.lineage.project_id)
        .bind(f.selection.lineage.task_id)
        .bind(json!([requirement]))
        .execute(&pool)
        .await
        .unwrap();
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let revision = receipt.report_digest.as_str();
    let app = aq_server::build_admin_router(f.state.clone());
    let root = format!("/api/v1/results/{}/revisions/{revision}", report.report_id);
    let (_, initial) = send(&app, "GET", &root, Value::Null).await;
    let actor = f
        .state
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let dir = private_media_dir();
    let owner = aq_server::result_media::Owner::open(f.state.clone(), dir.path(), Uuid::new_v4())
        .await
        .unwrap();
    let source = initial["data"]["revision"]["source"]["base"].clone();

    for index in 0..3 {
        let id = Uuid::new_v4();
        let mut request: Value =
            serde_json::from_slice(&super::super::media_owner::request(id)).unwrap();
        request["capture"] = json!({"scenario":"results","captured_at_ms":NOW,"viewport":{"width_css":1280,"height_css":800,"dpr_milli":1000},"environment":"test","claimed_source":if index==0{Value::Null}else{source.clone()}});
        owner
            .reserve(
                &actor,
                report.report_id,
                revision,
                &serde_json::to_vec(&request).unwrap(),
            )
            .await
            .unwrap();
        let (_, before) = send(&app, "GET", &root, Value::Null).await;
        assert_eq!(before["data"]["assessment"]["evidence"]["satisfied"], false);
        owner
            .put(&actor, id, super::super::media_owner::png())
            .await
            .unwrap();
        owner.finalize(&actor, id).await.unwrap();
    }
    let version = complete_assessment(&app, &root, &initial).await;
    let expires: i64 = sqlx::query_scalar("SELECT max(retain_until_ms) FROM result_media_intents")
        .fetch_one(&pool)
        .await
        .unwrap();
    f.time.store(expires + 1, Ordering::SeqCst);
    check_expired(&app, &root, &initial, version).await;
    assert!(
        sqlx::query("UPDATE result_task_policies SET requirements='[]'")
            .execute(&pool)
            .await
            .is_err()
    );
}

async fn complete_assessment(app: &Router, root: &str, initial: &Value) -> u64 {
    let (_, complete) = send(app, "GET", root, Value::Null).await;
    assert_eq!(
        complete["data"]["assessment"]["evidence"]["satisfied"],
        true
    );
    assert_eq!(
        complete["data"]["assessment"]["evidence"]["requirements"][0]["artifact_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(initial["data"]["revision"], complete["data"]["revision"]);
    assert_eq!(complete["data"]["assessment"]["acceptance"], "unreviewed");
    complete["data"]["assessment"]["version"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap()
}

async fn check_expired(app: &Router, root: &str, initial: &Value, version: u64) {
    let (_, expired) = send(app, "GET", root, Value::Null).await;
    assert_eq!(
        expired["data"]["assessment"]["evidence"]["satisfied"],
        false
    );
    assert!(
        expired["data"]["assessment"]["version"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > version
    );
    assert_eq!(initial["data"]["revision"], expired["data"]["revision"]);
}

fn private_media_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}
