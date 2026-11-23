use super::*;

pub(super) struct Harness {
    pub(super) app: Router,
    pub(super) admin: Router,
    pub(super) token: String,
    pub(super) identity: NodeIdentity,
    pub(super) offer: Offer,
    pub(super) manifest: TransferManifest,
    pub(super) bytes: Vec<u8>,
    pub(super) dir: tempfile::TempDir,
    pub(super) pool: sqlx::PgPool,
    pub(super) db: TestDb,
}
impl Harness {
    pub(super) fn finalize(&self) -> tokio::task::JoinHandle<axum::response::Response> {
        let app = self.app.clone();
        let token = self.token.clone();
        let input = self.status_body();
        tokio::spawn(async move {
            app.oneshot(
                Request::post("/v1/execution/sources/finalize")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(input.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
        })
    }

    pub(super) async fn upload_manifest(&self) {
        assert_eq!(
            self.json(
                "/v1/execution/sources/begin",
                self.begin_body(Uuid::new_v4())
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            self.upload(Uuid::new_v4(), 0, self.bytes.clone()).await.0,
            StatusCode::OK
        );
    }

    pub(super) async fn new() -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        let pool = db.pool().await;
        let (token, identity) = enrolled_node(&store).await;
        let (source, offer) = offered_source(&store, &identity).await;
        let (manifest, bytes) = checkpoint(&offer, source);
        let dir = tempfile::tempdir().unwrap();
        let state = source_state(store, dir.path());
        Self {
            admin: aq_server::build_admin_router(state.clone()),
            app: build_worker_router(state),
            token,
            identity,
            offer,
            manifest,
            bytes,
            dir,
            pool,
            db,
        }
    }
    pub(super) async fn json(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.request(
            Request::post(path).header("content-type", "application/json"),
            body.to_string().into_bytes(),
            Some(&self.token),
        )
        .await
    }
    pub(super) async fn request(
        &self,
        request: axum::http::request::Builder,
        body: Vec<u8>,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut r = request.header("x-aq-request", "1");
        if let Some(t) = token {
            r = r.header("authorization", format!("Bearer {t}"));
        }
        let request = r.body(Body::from(body)).unwrap();
        let app = if request.uri().path().starts_with("/api/") {
            &self.admin
        } else {
            &self.app
        };
        let res = app.clone().oneshot(request).await.unwrap();
        let code = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    pub(super) fn begin_body(&self, op: Uuid) -> Value {
        json!({"identity":self.identity,"operation_id":op,"manifest":self.manifest})
    }
    pub(super) fn status_body(&self) -> Value {
        json!({"identity":self.identity,"operation_id":Uuid::new_v4(),"transfer_id":self.manifest.transfer_id})
    }
    pub(super) async fn upload(
        &self,
        op: Uuid,
        offset: u64,
        bytes: Vec<u8>,
    ) -> (StatusCode, Value) {
        self.request(
            Request::put(&format!(
                "/v1/execution/sources/{}/manifest?operation_id={op}&offset={offset}&digest={}",
                self.manifest.transfer_id,
                hash(&bytes)
            ))
            .header("content-type", "application/octet-stream"),
            bytes,
            Some(&self.token),
        )
        .await
    }
}

async fn enrolled_node(store: &aq_server::store::Store) -> (String, NodeIdentity) {
    let now = Utc::now();
    let owner = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap()
        .unwrap();
    store
        .create_enrollment("enroll", owner.id, now, now + Duration::minutes(5))
        .await
        .unwrap();
    let token = new_node_credential();
    let node = store
        .redeem_enrollment(
            "enroll",
            NewNode {
                name: "source".into(),
                credential_hash: verifier(&token),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    let identity = NodeIdentity {
        node_id: node.id,
        boot_id: Uuid::new_v4(),
    };
    store
        .execution_register(&RegisterRequest {
            identity: identity.clone(),
            operation_id: Uuid::new_v4(),
            protocol_version: EXECUTION_PROTOCOL_VERSION,
            worker_version: "0.1.0".into(),
            capabilities: vec![FAKE_HARNESS_CAPABILITY.into(), SOURCE_CAPABILITY.into()],
        })
        .await
        .unwrap();
    (token, identity)
}
async fn offered_source(
    store: &aq_server::store::Store,
    identity: &NodeIdentity,
) -> (SourceBinding, Offer) {
    let source = SourceBinding {
        repository_id: Uuid::new_v4(),
        binding_generation: 1,
        logical_identity: "example/repository".into(),
        base: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "a".repeat(40),
        },
    };
    let spec = ExecutionSpec {
        bindings: Some(ExecutionBindings {
            source: Some(source.clone()),
            graph: None,
        }),
        project_id: Uuid::new_v4(),
        harness: FAKE_HARNESS_CAPABILITY.into(),
        resolved_spec_digest: String::new(),
        fixture_output: "source".into(),
        resources: Resources {
            slots: 1,
            cpu_millis: 100,
            memory_bytes: 1024,
            disk_bytes: 1024,
        },
        required_capabilities: vec![FAKE_HARNESS_CAPABILITY.into(), SOURCE_CAPABILITY.into()],
    }
    .seal();
    let offer = Offer {
        offer_id: Uuid::new_v4(),
        identity: identity.clone(),
        attempt: AttemptRef {
            attempt_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            generation: 1,
        },
        spec,
        offer_expires_at_ms: 100000,
        reservation_ttl_ms: 30000,
    };
    store.execution_offer(Uuid::new_v4(), &offer).await.unwrap();
    (source, offer)
}
fn checkpoint(offer: &Offer, source: SourceBinding) -> (TransferManifest, Vec<u8>) {
    let artifact = Uuid::new_v4();
    let checkpoint = ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: "b".repeat(40),
    };
    // reference-only lets transport tests isolate exact central receipt semantics.
    let bytes=serde_json::to_vec_pretty(&json!({"format_version":1,"repository_id":source.repository_id,"repository_identity":source.logical_identity,"binding_generation":1,"transfer_id":artifact,"object_format":"sha1","base":source.base,"source":checkpoint,"prerequisites":[],"bundle_size":0,"bundle_sha256":null,"producer_installation_id":Uuid::new_v4(),"reference_only":true,"caller_correlation":null})).unwrap();
    let manifest = TransferManifest {
        transfer_id: Uuid::new_v4(),
        role: SourceRole::Checkpoint,
        checkpoint,
        attempt: offer.attempt.clone(),
        resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
        source,
        artifact_id: artifact,
        manifest_digest: hash(&bytes),
        bundle_digest: None,
        manifest_bytes: bytes.len() as u64,
        bundle_bytes: 0,
    };
    (manifest, bytes)
}
fn source_state(store: aq_server::store::Store, dir: &std::path::Path) -> AppState {
    let settings = SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: dir.join("staging"),
        git_root: dir.join("git"),
        git_socket: dir.join("git.sock"),
        max_staging_bytes: 2 * 1024 * 1024,
        max_bundle_bytes: 4096,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 1,
        git_deadline_ms: 30_000,
    };
    let config:Config=toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
    AppState {
        internal_work: Default::default(),
        agent_access: None,
        source_retention: Some(Arc::new(Runtime::new(settings).unwrap())),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(Utc::now),
    }
}
