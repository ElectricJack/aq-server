//! Actual mounted human router + real result/SQL/files/codec, no API mocks.
use super::media_owner::{Gate, request, retained, setup};
use super::*;
use aq_result_media as media;
use aq_server::{
    AppState, build_admin_router_with_optional_owners,
    result_media::{Error, config::MediaConfig, runtime::Runtime},
    store::User,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use std::os::unix::fs::PermissionsExt;
use tower::ServiceExt;

async fn configured(
    upload: bool,
) -> (
    Fixture,
    ResultReport,
    String,
    Arc<Runtime>,
    tempfile::TempDir,
    User,
) {
    let (f, report, revision, owner, root, actor) = setup().await;
    let pool = f.db.pool().await;
    let storage = sqlx::query_scalar("SELECT storage_id FROM result_media_storage")
        .fetch_one(&pool)
        .await
        .unwrap();
    drop(owner);
    let runtime = Runtime::start(
        f.state.clone(),
        &MediaConfig {
            root: root.path().into(),
            storage_id: storage,
            uploads_enabled: upload,
        },
    )
    .await
    .unwrap();
    (f, report, revision, runtime, root, actor)
}
fn app(state: &AppState, runtime: &Arc<Runtime>) -> Router {
    build_admin_router_with_optional_owners(state.clone(), None, None, None, Some(runtime.clone()))
}
async fn caps(api: &Router) -> serde_json::Value {
    value(
        api.clone()
            .oneshot(req("GET", "/api/v1/bootstrap", "application/json", vec![]))
            .await
            .unwrap(),
    )
    .await
}
fn cap(value: &serde_json::Value, name: &str) -> bool {
    value["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name)
        .unwrap()["enabled"]
        .as_bool()
        .unwrap()
}
fn req(method: &str, path: &str, mime: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("x-aq-request", "1")
        .header("content-type", mime)
        .body(Body::from(body))
        .unwrap()
}
async fn value(response: Response) -> serde_json::Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), media::MAX_PAGE_BYTES)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn large_png() -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc = !0u32;
        for n in &out[start..] {
            crc ^= u32::from(*n);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    0xedb88320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }
        out.extend_from_slice(&(!crc).to_be_bytes());
    }
    let (width, height) = (1024u32, 8u32);
    let mut raw = Vec::new();
    let mut seed = 91u32;
    for _ in 0..height {
        raw.extend_from_slice(&[0]);
        for _ in 0..width * 3 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            raw.push((seed >> 24) as u8);
        }
    }
    let len = u16::try_from(raw.len()).unwrap();
    let mut deflate = vec![0x78, 1, 1];
    deflate.extend_from_slice(&len.to_le_bytes());
    deflate.extend_from_slice(&(!len).to_le_bytes());
    deflate.extend_from_slice(&raw);
    let (mut a, mut b) = (1u32, 0u32);
    for n in &raw {
        a = (a + u32::from(*n)) % 65521;
        b = (b + a) % 65521;
    }
    deflate.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = width.to_be_bytes().to_vec();
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &deflate);
    chunk(&mut out, b"IEND", &[]);
    out
}
#[tokio::test]
async fn actual_http_reserves_replays_retains_lists_and_stream_frames_hold_all_guards() {
    let (f, report, revision, runtime, _root, _actor) = configured(true).await;
    let api = app(&f.state, &runtime);
    let capabilities = caps(&api).await;
    assert!(cap(&capabilities, "results.media.read"));
    assert!(cap(&capabilities, "results.media.upload"));
    let id = Uuid::new_v4();
    let input = large_png();
    let raw = serde_json::to_vec(&media::UploadCreate {
        operation_id: media::Id::new(id).unwrap(),
        input_digest: media::Digest::of(&input),
        input_bytes: input.len() as u64,
        filename: "human-large.png".into(),
        capture: None,
    })
    .unwrap();
    let base = format!(
        "/api/human/results/{}/revisions/{revision}",
        report.report_id
    );
    let uploads = format!("{base}/media-uploads");
    let response = api
        .clone()
        .oneshot(req("POST", &uploads, "application/json", raw.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let admitted = value(response).await;
    assert_eq!(admitted["operation_id"], id.to_string());
    let response = api
        .clone()
        .oneshot(req("POST", &uploads, "application/json", raw))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_path = format!("/api/human/result-media-uploads/{id}/content");
    for _ in 0..2 {
        let response = api
            .clone()
            .oneshot(req("PUT", &content_path, "image/png", input.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(value(response).await["state"], "received");
    }
    let response = api
        .clone()
        .oneshot(req(
            "POST",
            &format!("/api/human/result-media-uploads/{id}/finalize"),
            "application/json",
            b"{}".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let retained = value(response).await;
    assert_eq!(retained["state"], "retained");
    let receipt = retained["artifact"]["receipt"].clone();
    let response = api
        .clone()
        .oneshot(req(
            "GET",
            &format!("{base}/media"),
            "application/json",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page = value(response).await;
    media::MediaPage::decode(&serde_json::to_vec(&page).unwrap()).unwrap();
    assert_eq!(page["items"][0]["receipt"], receipt);
    let path = format!("{base}/media/{id}/content?variant=render");
    let response = api
        .clone()
        .oneshot(req("GET", &path, "application/json", vec![]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(
        response.headers()["aq-media-digest"],
        receipt["variants"]["render"]["digest"].as_str().unwrap()
    );
    assert_eq!(
        response.headers()["aq-media-receipt-digest"],
        retained["artifact"]["receipt_digest"].as_str().unwrap()
    );
    assert_eq!(
        response.headers()["content-security-policy"],
        "default-src 'none'"
    );
    let total = response.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(total > 16_384);
    let full = to_bytes(response.into_body(), media::MAX_RENDER_BYTES)
        .await
        .unwrap();
    assert_eq!(full.len(), total);
    assert_eq!(
        media::Digest::of(&full).as_str(),
        receipt["variants"]["render"]["digest"].as_str().unwrap()
    );
    drop(full);
    let response = api
        .clone()
        .oneshot(req("GET", &path, "application/json", vec![]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(frame.len(), 16_384);
    let second = api
        .clone()
        .oneshot(req("GET", &path, "application/json", vec![]))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let refused = api
        .clone()
        .oneshot(req("GET", &path, "application/json", vec![]))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(second);
    drop(body);
    f.time.store(
        receipt["retain_until_ms"].as_i64().unwrap(),
        Ordering::SeqCst,
    );
    assert_eq!(
        runtime.owner().cleanup(16).await.unwrap(),
        0,
        "queued frame retains Content and admission after body drop"
    );
    drop(frame);
    assert_eq!(runtime.owner().cleanup(16).await.unwrap(), 1);
    runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn actual_http_closed_requests_shared_csrf_and_read_only_upload_optin() {
    let (f, report, revision, runtime, _root, actor) = configured(false).await;
    let api = app(&f.state, &runtime);
    let capabilities = caps(&api).await;
    assert!(cap(&capabilities, "results.media.read"));
    assert!(!cap(&capabilities, "results.media.upload"));
    let absent = aq_server::build_admin_router(f.state.clone());
    let capabilities = caps(&absent).await;
    assert!(!cap(&capabilities, "results.media.read"));
    assert!(!cap(&capabilities, "results.media.upload"));
    let id = Uuid::new_v4();
    let base = format!(
        "/api/human/results/{}/revisions/{revision}",
        report.report_id
    );
    let response = api
        .clone()
        .oneshot(req(
            "POST",
            &format!("{base}/media-uploads"),
            "application/json",
            request(id),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    runtime
        .owner()
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    let response = api
        .clone()
        .oneshot(req(
            "GET",
            &format!("/api/human/result-media-uploads/{id}"),
            "application/json",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = api
        .clone()
        .oneshot(req(
            "GET",
            &format!("{base}/media?unknown=1"),
            "application/json",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(value(response).await["error"], "bad_request");
    runtime.shutdown().await.unwrap();
    drop(api);
    drop(runtime);
    let (f, report, revision, runtime, _root, _actor) = configured(true).await;
    let api = app(&f.state, &runtime);
    let base = format!(
        "/api/human/results/{}/revisions/{revision}/media-uploads",
        report.report_id
    );
    let denied = Request::builder()
        .method("POST")
        .uri(&base)
        .header("content-type", "application/json")
        .body(Body::from(request(Uuid::new_v4())))
        .unwrap();
    assert_eq!(
        api.clone().oneshot(denied).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let denied = Request::builder()
        .method("POST")
        .uri(&base)
        .header("x-aq-request", "1")
        .header("origin", "https://foreign.invalid")
        .header("content-type", "application/json")
        .body(Body::from(request(Uuid::new_v4())))
        .unwrap();
    assert_eq!(
        api.clone().oneshot(denied).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM result_media_intents")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let malformed = b"{\"operation_id\":\"bad\",\"extra\":true}".to_vec();
    assert_eq!(
        api.clone()
            .oneshot(req("POST", &base, "application/json", malformed))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        api.clone()
            .oneshot(req(
                "POST",
                &base,
                "application/json",
                vec![b' '; media::MAX_JSON_BYTES + 1]
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn actual_http_known_missing_corrupt_withheld_expired_and_unknown_scope_are_distinct() {
    let (f, report, revision, runtime, root, actor) = configured(true).await;
    let id = retained(runtime.owner(), &actor, report.report_id, &revision).await;
    let api = app(&f.state, &runtime);
    let base = format!(
        "/api/human/results/{}/revisions/{revision}/media/{id}/content",
        report.report_id
    );
    let pool = f.db.pool().await;
    for suffix in [
        "?variant=render&unknown=1",
        "?variant=render&variant=preview",
        "?variant=download",
    ] {
        let response = api
            .clone()
            .oneshot(req(
                "GET",
                &format!("{base}{suffix}"),
                "application/json",
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(value(response).await["error"], "bad_request");
    }
    let unknown = format!(
        "/api/human/results/{}/revisions/{revision}/media/{id}/content?variant=render",
        Uuid::new_v4()
    );
    let response = api
        .clone()
        .oneshot(req("GET", &unknown, "application/json", vec![]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(value(response).await.get("contract").is_none());
    sqlx::query("UPDATE result_media_intents SET withheld_at_ms=$2 WHERE operation_id=$1")
        .bind(id)
        .bind(NOW as i64)
        .execute(&pool)
        .await
        .unwrap();
    let response = api
        .clone()
        .oneshot(req(
            "GET",
            &format!("{base}?variant=render"),
            "application/json",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GONE);
    assert_eq!(value(response).await["error"], "media_withheld");
    sqlx::query("UPDATE result_media_intents SET withheld_at_ms=NULL WHERE operation_id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let dir = root.path().join("objects").join(id.to_string());
    std::fs::remove_file(dir.join("preview.png")).unwrap();
    let response = api
        .clone()
        .oneshot(req(
            "GET",
            &format!("{base}?variant=preview"),
            "application/json",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(value(response).await["error"], "media_missing");
    let path = dir.join("render.png");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[20] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    let response = api
        .clone()
        .oneshot(req(
            "GET",
            &format!("{base}?variant=render"),
            "application/json",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(value(response).await["error"], "media_failed");
    f.time
        .store((NOW + media::MAX_RETENTION_MS) as i64, Ordering::SeqCst);
    let response = api
        .clone()
        .oneshot(req(
            "GET",
            &format!("{base}?variant=render"),
            "application/json",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GONE);
    assert_eq!(value(response).await["error"], "media_expired");
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn actual_http_denied_during_failed_content_io_stays_auth_error_and_foreign_intent_hidden() {
    let (f, report, revision, runtime, root, actor) = configured(true).await;
    let id = retained(runtime.owner(), &actor, report.report_id, &revision).await;
    let api = app(&f.state, &runtime);
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at,bound_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now(),now())").bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let mut foreign = req(
        "GET",
        &format!("/api/human/result-media-uploads/{id}"),
        "application/json",
        vec![],
    );
    foreign
        .headers_mut()
        .insert("x-aq-dev-email", "other@localhost".parse().unwrap());
    let response = api.clone().oneshot(foreign).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(value(response).await.get("operation_id").is_none());
    let gate = Gate::new();
    runtime
        .owner()
        .set_test_hook(Some(gate.hook("content_read")))
        .unwrap();
    let request = req(
        "GET",
        &format!(
            "/api/human/results/{}/revisions/{revision}/media/{id}/content?variant=render",
            report.report_id
        ),
        "application/json",
        vec![],
    );
    let task = tokio::spawn(api.clone().oneshot(request));
    gate.entered.notified().await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(actor.id)
        .execute(&pool)
        .await
        .unwrap();
    std::fs::remove_file(
        root.path()
            .join("objects")
            .join(id.to_string())
            .join("render.png"),
    )
    .unwrap();
    gate.release();
    let response = task.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let denied = value(response).await;
    assert!(denied.get("contract").is_none());
    assert!(denied.get("receipt").is_none());
    let capabilities = caps(&api).await;
    assert!(!cap(&capabilities, "results.media.read"));
    assert!(!cap(&capabilities, "results.media.upload"));
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn actual_http_retained_disposable_report_fixture_has_exact_metadata_and_no_media() {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let (f, _, mut report) = prepare(true, true).await;
    report.conclusion = Label::new("AQ disposable media fixture").unwrap();
    let recorded = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let revision = recorded.report_digest.as_str().to_owned();
    let actor = f
        .state
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let runtime = Runtime::start(
        f.state.clone(),
        &MediaConfig {
            root: root.path().into(),
            storage_id: Uuid::new_v4(),
            uploads_enabled: true,
        },
    )
    .await
    .unwrap();
    let api = app(&f.state, &runtime);
    let detail = format!(
        "/api/human/results/{}/revisions/{revision}",
        report.report_id
    );
    let response = api
        .clone()
        .oneshot(req("GET", &detail, "application/json", vec![]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let metadata = value(response).await;
    let response = api
        .clone()
        .oneshot(req(
            "GET",
            &format!("{detail}/media"),
            "application/json",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page = value(response).await;
    assert_eq!(page["items"], json!([]));
    let item = &metadata["items"][0];
    assert_eq!(page["binding"]["report_id"], item["report_id"]);
    assert_eq!(page["binding"]["report_revision"], item["revision"]);
    assert_eq!(page["binding"]["project_id"], item["project_id"]);
    assert_eq!(page["binding"]["task_id"], item["task_id"]);
    assert_eq!(page["binding"]["attempt_id"], item["attempt_id"]);
    assert_eq!(
        page["binding"]["source"]["repository_id"],
        item["repository_id"]
    );
    assert_eq!(page["binding"]["source"]["base"], item["source_base"]);
    runtime.shutdown().await.unwrap();
    if std::env::var("AQ_TEST_RETAIN_MEDIA_HTTP_DEMO").as_deref() == Ok("1") {
        let pool = f.db.pool().await;
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let storage: Uuid = sqlx::query_scalar("SELECT storage_id FROM result_media_storage")
            .fetch_one(&pool)
            .await
            .unwrap();
        let path = root.keep();
        let fixture = json!({"schema_version":1,"disposable":true,"database":database,"media_root":path,"storage_id":storage,"uploads_enabled":true,"actor_id":actor.id,"email":"owner@localhost","issuer":"dev","subject":"owner@localhost","report_id":report.report_id,"revision":revision,"binding":page["binding"],"initial_media_intents":0,"capture_label":"AQ disposable media fixture","result_path":format!("/results/{}/revisions/{revision}",report.report_id),"coverage":"recorded_legacy_native_only","provenance":"Synthetic native launch fixture, actual AQ28SQL result owner; no provider or media capture claim"});
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open("/tmp/aq-media-http-demo.json")
            .unwrap();
        output
            .write_all(&serde_json::to_vec_pretty(&fixture).unwrap())
            .unwrap();
        output.sync_all().unwrap();
        println!(
            "Retained disposable media fixture: database={database} report={} metadata=/tmp/aq-media-http-demo.json",
            report.report_id
        );
        std::mem::forget(f.db.clone());
    }
}
#[tokio::test]
async fn actual_runtime_failed_initial_cleanup_refuses_mountable_success_and_preserves_charge() {
    let (f, report, revision, owner, root, actor) = setup().await;
    let id = Uuid::new_v4();
    owner
        .reserve(&actor, report.report_id, &revision, &request(id))
        .await
        .unwrap();
    let pool = f.db.pool().await;
    let dir = root.path().join("staging").join(id.to_string());
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(dir.join("unrecognized"), b"retained unknown file").unwrap();
    f.time.store(
        (NOW + media::MAX_UPLOAD_TTL_MS + 1) as i64,
        Ordering::SeqCst,
    );
    let storage = sqlx::query_scalar("SELECT storage_id FROM result_media_storage")
        .fetch_one(&pool)
        .await
        .unwrap();
    drop(owner);
    let settings = MediaConfig {
        root: root.path().into(),
        storage_id: storage,
        uploads_enabled: false,
    };
    assert!(matches!(
        Runtime::start(f.state.clone(), &settings).await,
        Err(Error::Storage)
    ));
    let (state, charge): (String, i64) = sqlx::query_as(
        "SELECT cleanup_state,reserved_bytes FROM result_media_intents WHERE operation_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((state.as_str(), charge), ("blocked", 10_485_760));
    std::fs::remove_file(dir.join("unrecognized")).unwrap();
    let runtime = Runtime::start(f.state.clone(), &settings).await.unwrap();
    assert!(runtime.read_enabled());
    assert!(!runtime.uploads_enabled());
    runtime.shutdown().await.unwrap();
}
