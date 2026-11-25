//! Actual inline retention ownership; no source bundles or provider secrets.
use crate::common;
use aq_admission::{ArtifactKind, Digest};
use aq_server::profiled_execution::artifacts::{self, OwnerKind, Retain};
use common::profiled::{NOW, artifact, fixture, insert};
use serde_json::json;
use uuid::Uuid;
mod ranged;
pub(super) mod transfer;
#[tokio::test]
async fn immutable_definition_identity_conflict_is_named_and_preserves_retained_bytes() {
    let f = fixture().await;
    let project = f.selection.lineage.project_id;
    let original = artifact("immutable-definition", b"original definition");
    let retained = artifacts::retain(
        &f.state,
        Retain {
            artifact_id: Uuid::new_v4(),
            project_id: project,
            owner: OwnerKind::Catalog,
            artifact: original.clone(),
            kind: ArtifactKind::Definition,
            bytes: b"original definition",
            retain_until_ms: NOW + 60_000,
        },
    )
    .await
    .unwrap();
    let mut changed = original.clone();
    changed.digest = Digest::of(b"changed definition");
    let error = artifacts::retain(
        &f.state,
        Retain {
            artifact_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            owner: OwnerKind::Startup,
            artifact: changed,
            kind: ArtifactKind::Definition,
            bytes: b"changed definition",
            retain_until_ms: NOW + 60_000,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        error.to_api_error().error,
        "admission_definition_identity_conflict"
    );
    // Identical bytes under a new retention operation still work.
    let replay = artifacts::retain(
        &f.state,
        Retain {
            artifact_id: Uuid::new_v4(),
            project_id: project,
            owner: OwnerKind::Catalog,
            artifact: original,
            kind: ArtifactKind::Definition,
            bytes: b"original definition",
            retain_until_ms: NOW + 60_000,
        },
    )
    .await
    .unwrap();
    assert_eq!(retained.artifact, replay.artifact);
}
#[tokio::test]
async fn retained_bytes_have_real_replayable_immutable_receipt_and_exact_project_links() {
    let f = fixture().await;
    let attempt = insert(&f).await;
    let project = f.selection.lineage.project_id;
    let id = Uuid::new_v4();
    let a = artifact("synthetic-context", b"context bytes");
    let input = || Retain {
        artifact_id: id,
        project_id: project,
        owner: OwnerKind::GraphSnapshot,
        artifact: a.clone(),
        kind: ArtifactKind::Context,
        bytes: b"context bytes",
        retain_until_ms: NOW + 60_000,
    };
    let first = artifacts::retain(&f.state, input()).await.unwrap();
    let replay = artifacts::retain(&f.state, input()).await.unwrap();
    assert_eq!(json!(first), json!(replay));
    let mut changed = input();
    changed.retain_until_ms += 1;
    assert!(artifacts::retain(&f.state, changed).await.is_err());
    let mut changed = input();
    changed.project_id = Uuid::new_v4();
    assert!(artifacts::retain(&f.state, changed).await.is_err());
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let item = artifacts::load_locked(&mut tx, id).await.unwrap();
    assert_eq!(item.bytes, b"context bytes");
    assert_eq!(Digest::of(&item.receipt), first.retention_receipt.digest);
    let proof: artifacts::Receipt = serde_json::from_slice(&item.receipt).unwrap();
    assert_eq!(proof.artifact_id, id);
    assert_eq!(proof.project_id, project);
    assert_eq!(
        artifacts::link_locked(
            &mut tx,
            artifacts::LinkScope::new(attempt, project, NOW..NOW + 30_000),
            &[id]
        )
        .await
        .unwrap()
        .len(),
        1
    );
    tx.commit().await.unwrap();
    for sql in [
        "UPDATE admission_artifacts SET body='different' WHERE artifact_id=$1",
        "DELETE FROM admission_artifacts WHERE artifact_id=$1",
        "DELETE FROM admission_inline_receipts WHERE artifact_id=$1",
        "DELETE FROM profiled_launch_artifacts WHERE artifact_id=$1",
    ] {
        assert!(sqlx::query(sql).bind(id).execute(&pool).await.is_err());
    }
    for (owner, until) in [(Uuid::new_v4(), NOW + 30_000), (project, NOW + 60_001)] {
        let mut tx = pool.begin().await.unwrap();
        let scope = artifacts::LinkScope::new(attempt, owner, NOW..until);
        assert!(artifacts::link_locked(&mut tx, scope, &[id]).await.is_err());
        tx.rollback().await.unwrap();
    }
}
#[tokio::test]
async fn missing_or_forged_receipt_cannot_commit_and_source_bundle_cannot_use_inline_owner() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = Uuid::new_v4();
    let a = artifact("synthetic-context", b"body");
    let project = f.selection.lineage.project_id;
    let receipt = artifact("aq.inline-retention/forged", b"invented");
    let descriptor = json!({"artifact":a,"kind":"context","bytes":4,"retention_receipt":receipt,"retained_at_ms":NOW,"retain_until_ms":NOW+1000});
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms) VALUES($1,$2,$3,4,'graph_snapshot',$4,$5,$6,$7)").bind(id).bind(descriptor).bind(a.digest.as_str()).bind(project.to_string()).bind(b"body".as_slice()).bind(NOW as i64).bind((NOW+1000)as i64).execute(&mut *tx).await.unwrap();
    assert!(tx.commit().await.is_err());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admission_artifacts WHERE artifact_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    let source = Retain {
        artifact_id: Uuid::new_v4(),
        project_id: project,
        owner: OwnerKind::SourceRetention,
        artifact: a,
        kind: ArtifactKind::Source,
        bytes: b"body",
        retain_until_ms: NOW + 1000,
    };
    assert!(artifacts::retain(&f.state, source).await.is_err());
}
#[tokio::test]
async fn finite_project_history_refuses_new_rows_without_evicting_or_blocking_exact_replay() {
    let f = fixture().await;
    let project = f.selection.lineage.project_id;
    let a = artifact("synthetic-context", b"x");
    let input = |id| Retain {
        artifact_id: id,
        project_id: project,
        owner: OwnerKind::GraphSnapshot,
        artifact: a.clone(),
        kind: ArtifactKind::Context,
        bytes: b"x",
        retain_until_ms: NOW + 60_000,
    };
    let first = Uuid::new_v4();
    for n in 0..511 {
        let id = if n == 0 { first } else { Uuid::new_v4() };
        artifacts::retain(&f.state, input(id)).await.unwrap();
    }
    let (left, right) = tokio::join!(
        artifacts::retain(&f.state, input(Uuid::new_v4())),
        artifacts::retain(&f.state, input(Uuid::new_v4()))
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert!(
        artifacts::retain(&f.state, input(Uuid::new_v4()))
            .await
            .is_err()
    );
    artifacts::retain(&f.state, input(first)).await.unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admission_artifacts WHERE owner_key=$1")
            .bind(project.to_string())
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
    assert_eq!(count, 512);
}

#[tokio::test]
async fn assigned_native_http_ranges_require_exact_sealed_binding_and_live_offer() {
    let case = ranged::Case::setup().await;
    case.assert_range_download().await;
    case.assert_refusals().await;
    case.assert_expiry_while_blocked().await;
}

#[tokio::test]
async fn orphaned_and_settled_artifacts_do_not_count_toward_the_project_cap() {
    let f = fixture().await;
    let project = f.selection.lineage.project_id;
    let pool = f.db.pool().await;
    let now = NOW as i64;
    // 600 unlinked rows older than the 10-minute pending window: orphans.
    let mut tx = crate::common::sealed::replica(&pool).await;
    for _ in 0..600 {
        crate::common::sealed::artifact(&mut tx, project, now - 700_000).await;
    }
    tx.commit().await.unwrap();
    let retain = |name: &str| {
        let a = artifact(name, b"x");
        let state = f.state.clone();
        async move {
            artifacts::retain(
                &state,
                Retain {
                    artifact_id: Uuid::new_v4(),
                    project_id: project,
                    owner: OwnerKind::GraphSnapshot,
                    artifact: a,
                    kind: ArtifactKind::Context,
                    bytes: b"x",
                    retain_until_ms: NOW + 60_000,
                },
            )
            .await
        }
    };
    retain("live-cap-context-1")
        .await
        .expect("orphans are not live");
    // 511 pending rows plus the one just retained make 512 live rows.
    let mut tx = crate::common::sealed::replica(&pool).await;
    for _ in 0..511 {
        crate::common::sealed::artifact(&mut tx, project, now - 1_000).await;
    }
    tx.commit().await.unwrap();
    assert!(retain("live-cap-context-2").await.is_err());
}

#[tokio::test]
async fn a_just_inserted_unlinked_export_is_written_with_its_time_and_counted_by_the_trigger() {
    let f = fixture().await;
    let project = f.selection.lineage.project_id;
    let pool = f.db.pool().await;
    let operation = Uuid::new_v4();
    let request = serde_json::to_vec(
        &json!({"schema_version":"1","operation_id":operation,"project_id":project}),
    )
    .unwrap();
    let digest: String = sqlx::query_scalar("SELECT encode(sha256($1),'hex')")
        .bind(&request)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    // The project's binding row (this fixture has none), then triggers and keys back on.
    sqlx::query("SET session_replication_role='replica'")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{}'::jsonb,1,'{}'::jsonb) ON CONFLICT DO NOTHING")
        .bind(project).execute(&mut *conn).await.unwrap();
    sqlx::query("SET session_replication_role='origin'")
        .execute(&mut *conn)
        .await
        .unwrap();
    aq_history_caps::insert_export(
        &mut conn,
        (operation, project),
        (&request, &digest),
        (1_048_577, NOW as i64),
    )
    .await
    .unwrap();
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT created_at_ms FROM profiled_source_exports WHERE operation_id=$1",
    )
    .bind(operation)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        Some(NOW as i64),
        "written by the INSERT the trigger sees"
    );
    // Verified and not yet linked by any run: inside the window it still counts, after it does not.
    sqlx::query("SET session_replication_role='replica'")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_source_exports SET state='verified',verified_bytes=$2,verified_digest=encode(sha256($2),'hex'),charged_bytes=5000 WHERE operation_id=$1")
        .bind(operation).bind(b"{}".as_slice()).execute(&mut *conn).await.unwrap();
    for (at, rows) in [(NOW as i64 + 1_000, 1), (NOW as i64 + 700_000, 0)] {
        let u =
            crate::common::sealed::usage(&pool, "source_exports", Some(project), at, None).await;
        assert_eq!(u.2, rows, "at {at}");
    }
}
