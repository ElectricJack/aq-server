use super::{
    transfer::{post, register},
    *,
};
use crate::common::profiled::Fixture;
use aq_admission::{
    RetainedArtifact,
    transport::{ArtifactFetch, WorkerIdentity},
};
use aq_server::{execution::authority::hot::HotState, profiled_execution::artifact_api};
use axum::{Router, http::StatusCode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use http_body_util::BodyExt;
use std::sync::atomic::Ordering;
const BODY: &[u8] = b"retained context";
pub(super) struct Case {
    f: Fixture,
    pool: sqlx::PgPool,
    worker: Router,
    request: ArtifactFetch,
    retained: RetainedArtifact,
    id: Uuid,
}
impl Case {
    pub(super) async fn setup() -> Self {
        let mut f = fixture().await;
        let project = f.selection.lineage.project_id;
        let pool = register(
            &mut f,
            artifact("synthetic-runtime", b"runtime"),
            artifact("synthetic-runtime-qualification", b"qualified"),
        )
        .await;
        let attempt = insert(&f).await;
        let hot = HotState::from_compat(&f.profiled).unwrap();
        sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(&pool).await.unwrap();
        let body = BODY;
        let id = Uuid::new_v4();
        let reference = artifact("synthetic-context", body);
        let retained = artifacts::retain(
            &f.state,
            Retain {
                artifact_id: id,
                project_id: project,
                owner: OwnerKind::GraphSnapshot,
                artifact: reference.clone(),
                kind: ArtifactKind::Context,
                bytes: body,
                retain_until_ms: NOW + 60_000,
            },
        )
        .await
        .unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(f.node.node_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        artifacts::link_locked(
            &mut tx,
            artifacts::LinkScope::new(attempt, project, NOW..NOW + 30_000),
            &[id],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let worker = aq_server::build_worker_router(f.state.clone());
        let request = ArtifactFetch {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity: WorkerIdentity {
                node_id: f.node.node_id,
                boot_id: f.node.boot_id,
            },
            binding: f.profiled.offer.binding.clone(),
            artifact: reference,
            offset: 0,
            length: 5,
        };
        Self {
            pool,
            f,
            worker,
            request,
            retained,
            id,
        }
    }
    pub(super) async fn assert_range_download(&self) {
        let mut request = self.request.clone();
        let mut collected = vec![];
        for offset in [0, 5, 10, 15] {
            request.offset = offset;
            let response = post(self.worker.clone(), &request, &self.f.credential).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert_eq!(
                response.headers()[artifact_api::OFFSET_HEADER],
                offset.to_string()
            );
            let descriptor: RetainedArtifact = serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(response.headers()[artifact_api::RETENTION_HEADER].as_bytes())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(json!(descriptor), json!(self.retained));
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            assert!(bytes.len() <= 5);
            collected.extend_from_slice(&bytes);
        }
        assert_eq!(collected, BODY);
        self.retained.verify(&collected, NOW).unwrap();
    }
    pub(super) async fn assert_refusals(&self) {
        let mut request = self.request.clone();
        request.offset = BODY.len() as u64;
        assert_eq!(
            post(self.worker.clone(), &request, &self.f.credential)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        request.offset = 0;
        let original = request.binding.graph_access_profile_digest.clone();
        request.binding.graph_access_profile_digest = Digest::of(b"wrong-graph-binding");
        assert_eq!(
            post(self.worker.clone(), &request, &self.f.credential)
                .await
                .status(),
            StatusCode::CONFLICT
        );
        request.binding.graph_access_profile_digest = original;
        assert_eq!(
            post(self.worker.clone(), &request, "wrong-credential")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    pub(super) async fn assert_expiry_while_blocked(self) {
        let Self {
            pool,
            id,
            f,
            worker,
            request,
            ..
        } = self;
        let mut held = pool.begin().await.unwrap();
        sqlx::query("SELECT artifact_id FROM admission_artifacts WHERE artifact_id=$1 FOR UPDATE")
            .bind(id)
            .execute(&mut *held)
            .await
            .unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *held)
            .await
            .unwrap();
        let credential = f.credential.clone();
        let pending = tokio::spawn(async move { post(worker, &request, &credential).await });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
            .bind(blocker).fetch_one(&pool).await.unwrap();
            if waiting {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fetch did not reach retained artifact lock"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        f.time.store((NOW + 30_000) as i64, Ordering::SeqCst);
        held.commit().await.unwrap();
        let response = pending.await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            !response
                .headers()
                .contains_key(artifact_api::RETENTION_HEADER)
        );
    }
}
