//! Opt-in real component fixture. No provider/worker/Git/AQ settlement is implied.
#[path = "support/real_graph.rs"]
mod real_graph;
use anyhow::{Context, Result, ensure};
use real_graph::{BinaryPin, EXISTING_GRAPH_SHA256, RealGraphFixture, TaskSeed};
use serde_json::json;
use uuid::Uuid;
#[tokio::test]
#[ignore = "requires explicitly pinned existing aq-graph executable and disposable test PostgreSQL55432; no provider"]
async fn actual_pinned_graph_fixture_cas_receipts_context_and_owned_cleanup() -> Result<()> {
    let path = std::env::var_os("AQ_GATE04_GRAPH_BINARY")
        .context("explicit pinned Graph executable required")?;
    let expected = std::env::var("AQ_GATE04_GRAPH_SHA256")
        .context("explicit reviewed Graph digest required")?;
    ensure!(
        expected == EXISTING_GRAPH_SHA256,
        "reviewed Graph pin required"
    );
    let fixture = RealGraphFixture::start(
        BinaryPin {
            path: path.into(),
            sha256: expected,
        },
        TaskSeed {
            workspace_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            title: "Disposable Graph fixture smoke".into(),
            body: "Component-only task. This is not an admitted model task or AQ completion proof."
                .into(),
            acceptance_criteria: "Verify actual CAS/idempotency receipt and owned cleanup.".into(),
        },
    )
    .await?;
    let task = fixture.task_pin().clone();
    let created = fixture.creation_receipt().clone();
    let before = fixture.owner_evidence().await?;
    let runtime = fixture.agent_access_config();
    ensure!(
        runtime.mutation_actor_id.is_some() && runtime.graph_url == fixture.origin(),
        "actual service runtime config"
    );
    let key = Uuid::new_v4().to_string();
    let body = json!({"idempotency_key":key,"reason":"Graph-only CAS component smoke, not B2/C settlement","operations":[{"op":"update_node","id":task.task_id,"expected_version":task.version,"patch":{"task_state":"completed"}}]});
    let (status, receipt) = fixture.mutation(body.clone()).await?;
    ensure!(
        status.as_u16() == 200,
        "actual Graph completion CAS refused"
    );
    ensure!(
        receipt["change_set_id"] != created["change_set_id"]
            && receipt["results"][0]["version"] == task.version + 1
            && receipt["results"][0]["content_revision"] == task.content_revision + 1
            && receipt["results"][0]["task_state"] == "completed",
        "actual Graph CAS receipt mismatch"
    );
    let (status, replayed) = fixture.mutation(body).await?;
    ensure!(
        status.as_u16() == 200 && replayed == receipt,
        "exact Graph mutation receipt replay"
    );
    let (status,_)=fixture.mutation(json!({"idempotency_key":Uuid::new_v4().to_string(),"operations":[{"op":"update_node","id":task.task_id,"expected_version":task.version,"patch":{"task_state":"active"}}]})).await?;
    ensure!(status.as_u16() == 409, "stale task CAS must conflict");
    let actual = fixture.task().await?;
    ensure!(
        actual == receipt["results"][0],
        "post-CAS actual task read mismatch"
    );
    let after = fixture.owner_evidence().await?;
    let cleanup = fixture.shutdown().await?;
    ensure!(
        cleanup.child_reaped
            && cleanup.successful_exit
            && !cleanup.kill_fallback
            && cleanup.database_removed
            && cleanup.directory_removed,
        "explicit graceful owned cleanup required"
    );
    let evidence = json!({"scope":"actual pinned Graph component only; no GATE04 harness/settlement/admission claim","before":before,"creation_receipt":created,"completion_receipt":receipt,"after":after,"cleanup":cleanup});
    if let Some(path) = std::env::var_os("AQ_GATE04_GRAPH_EVIDENCE") {
        let path = std::path::PathBuf::from(path);
        ensure!(!path.exists(), "fresh evidence path required");
        std::fs::write(path, serde_json::to_vec_pretty(&evidence)?)?;
    }
    println!(
        "actual Graph component PASS: task={} CAS={}→{}, actual receipts retained; exact child/database/tempdir cleanup confirmed",
        task.task_id,
        task.version,
        task.version + 1
    );
    Ok(())
}
