//! Spec 0014: every offer is built from the active role snapshot. The role is
//! the task's or the project's default, the level is the routed class or the
//! project's default, and the startup text is platform text, role, project
//! notes, task and reporting, in that order. No offer needs a context fact, a
//! pin or a measurement. The actual mounted poll, selection and publication
//! run; the produced offer is checked by the worker's own validators.
use crate::common::native_offer::{Case, poll, poll_offered, post, setup};
use crate::common::roles;
use aq_admission::{
    control::{ControlReply, decode_reply},
    startup::NativeOffer,
};
use aq_profiles::roles::Kind;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use uuid::Uuid;

/// The one offer of a successful poll, checked as the worker checks it. Its
/// token numbers are the rendered byte counts: no model window is compared.
async fn offered(c: &Case) -> NativeOffer {
    let (status, body) = poll_offered(c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    page.validate_for(&c.request).unwrap();
    assert_eq!(page.offers.len(), 1, "{body}");
    let native = page.offers.remove(0);
    native.validate().unwrap();
    let manifest = &native.offered.offer.manifest;
    aq_session_protocol::Validate::validate(manifest).unwrap();
    let (startup, plan) = (&native.startup, &manifest.context);
    assert_eq!(startup.payload_tokens_upper_bound, startup.payload_bytes);
    assert_eq!(startup.implicit_tokens_upper_bound, 0);
    assert_eq!(plan.startup_byte_budget, startup.payload_bytes);
    assert_eq!(plan.startup_token_budget, startup.payload_bytes);
    assert_eq!(
        plan.reserved_output_tokens,
        manifest.model.max_output_tokens
    );
    assert_eq!(
        plan.context_window_tokens,
        plan.startup_token_budget + plan.reserved_output_tokens
    );
    let tokens: u64 = plan.references.iter().map(|r| r.tokens).sum();
    assert_eq!(tokens, plan.startup_token_budget);
    assert!(
        (startup.renderer_qualification.id.as_str()).starts_with("aq.startup-renderer/"),
        "{:?}",
        startup.renderer_qualification
    );
    native
}
/// The startup text the offer delivers: the retained bytes of its payload.
async fn startup_text(c: &Case, native: &NativeOffer) -> String {
    let body: Vec<u8> = sqlx::query_scalar(
        "SELECT body FROM admission_artifacts WHERE descriptor#>>'{artifact,digest}'=$1 AND body IS NOT NULL LIMIT 1",
    )
    .bind(native.startup.payload.digest.as_str())
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    String::from_utf8(body).unwrap()
}
fn route(c: &Case, class: &str) {
    let route =
        json!({"c":class,"h":[],"p":[],"pf":[],"l":null,"i":"any","d":null,"s":"router","n":1});
    *c.metadata.lock().unwrap() = json!({"aq:route": route});
}
fn operations(native: &NativeOffer) -> Vec<&str> {
    let plan = &native.offered.offer.manifest.permissions;
    plan.effective
        .iter()
        .map(|p| p.operation.as_str())
        .collect()
}
async fn send(c: &Case, path: &str, body: &Value) -> (u16, Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    let r = post(&c.env.env.state, &c.env.env.credential, path, bytes).await;
    let status = r.status().as_u16();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn ran_with(c: &Case, attempt: Uuid) -> Option<aq_role_settings::attempts::RanWith> {
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    aq_role_settings::attempts::ran_with(&mut conn, attempt)
        .await
        .unwrap()
}
async fn pins(c: &Case, attempt: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT digest FROM role_snapshot_pins WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_all(&c.env.env.pool)
        .await
        .unwrap()
}

/// An unrouted task with no role: the project's default role on the
/// project's default level, the startup text in the spec's order, and what
/// the attempt ran with on its record.
#[tokio::test]
async fn an_unrouted_task_with_no_role_runs_as_the_project_default_on_the_default_level() {
    let c = setup().await;
    let native = offered(&c).await;
    let manifest = &native.offered.offer.manifest;
    let digest = c.env.snapshot.digest();
    // The unchanged wire: role, level and execution profile by document id,
    // each versioned by the snapshot digest; the catalog is the snapshot.
    for (reference, id) in [
        (&manifest.agent_profile, "role:developer"),
        (
            &manifest.intelligence_profile,
            "intelligence:standard-medium",
        ),
        (&manifest.execution_profile, "execution:sandbox"),
        (&manifest.catalog, "aq.role-snapshot"),
    ] {
        assert_eq!(reference.id.as_str(), id);
        assert_eq!(reference.version.as_str(), digest);
    }
    assert_eq!(manifest.model.model.as_str(), "model-a");
    assert_eq!(manifest.model.reasoning.as_str(), "high");
    assert_eq!(manifest.harness.harness.as_str(), roles::HARNESS);
    // The role's operations within the project's ceilings.
    assert_eq!(
        operations(&native),
        ["task.read", "knowledge.read", "source.write"]
    );
    // Platform text first, then the role and its rules, then the task; each
    // joined to the next by one blank line and nothing else.
    let text = startup_text(&c, &native).await;
    let role = c.env.snapshot.role("developer").unwrap();
    let platform = aq_profiles::roles::platform_text(roles::ENVIRONMENT);
    let head = format!(
        "{platform}\n\n## Role\n{}\n\n## Rules\n{}\n\n{{",
        role.role_md.trim(),
        role.rules_md.trim()
    );
    assert!(text.starts_with(&head), "{text}");
    assert!(platform.contains("`AGENTS.md`") && platform.contains("`.aq/PRIME.md`"));
    assert!(
        text.ends_with('}'),
        "the task is last without a reporting contract"
    );
    assert!(text.contains("Synthetic native admission task"));
    let provenance: Vec<&str> = (manifest.context.references.iter())
        .map(|r| r.provenance.as_str())
        .collect();
    assert_eq!(
        provenance[..3],
        ["aq.platform-text", "role:developer", "role:developer:rules"]
    );
    assert!(provenance[3].starts_with("aq.authorized-task-context/"));
    assert_eq!(provenance.len(), 4);
    // Only what the worker requires is a module: no agent or module body.
    for module in &manifest.modules {
        let id = module.id.as_str();
        assert!(
            !id.starts_with("module:") && !id.starts_with("agent:"),
            "{id}"
        );
    }
    // Spec 0014 §7.1: the summary is on the attempt's record and its snapshot is pinned.
    let attempt = native.offered.offer.binding.attempt_id;
    let summary = ran_with(&c, attempt).await.expect("a summary");
    assert_eq!(
        serde_json::to_value(&summary).unwrap(),
        json!({"role":"developer","level":"standard-medium","provider":"fixture-a","model":"model-a",
            "reasoning":"high","harness":roles::HARNESS,"execution_profile":"sandbox","snapshot_digest":digest})
    );
    assert_eq!(pins(&c, attempt).await, [digest.to_string()]);
    // The task's work read shows it on the attached execution (schema-checked).
    let work = super::holds::full_work(&c, c.env.lineage.task_id).await;
    assert_eq!(
        work["execution"]["ran_with"],
        serde_json::to_value(&summary).unwrap()
    );
    // The selection decision records role, level, candidate, harness and snapshot.
    let decision: Value = sqlx::query_scalar(
        "SELECT d.decision FROM intelligence_decisions d JOIN profiled_launches l USING(decision_id) WHERE l.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(decision["project"]["role"], "developer");
    assert_eq!(
        decision["project"]["intelligence_profile"],
        "standard-medium"
    );
    assert_eq!(decision["project"]["snapshot_digest"], digest);
    let choice = &decision["outcome"]["resolved"]["choice"];
    assert_eq!(choice["profile_candidate_id"], "fixture-a:model-a:high");
    assert_eq!(choice["harness"], roles::HARNESS);
    assert_eq!(
        decision["inputs"]["project"]["role_snapshot_digest"],
        digest
    );
    assert!(decision["inputs"].get("compositions").is_none());

    // A document saved after the offer does not change what the attempt
    // resolved from: its snapshot stays stored, and names the old model.
    let pool = &c.env.env.pool;
    let level = roles::level("standard-medium", &[("fixture-a", "model-later", "low")]);
    roles::save(
        pool,
        c.env.actor,
        Kind::Intelligence,
        "standard-medium",
        &level,
    )
    .await;
    assert_ne!(roles::active(pool).await.digest(), digest);
    assert_eq!(ran_with(&c, attempt).await, Some(summary));
    let kept = aq_role_settings::snapshot(pool, digest)
        .await
        .unwrap()
        .expect("pinned");
    let launch = kept.resolve("developer", "standard-medium").unwrap();
    assert_eq!(launch.candidates[0].model, "model-a");
    // When the attempt's record goes, the summary and the pin go with it.
    let mut conn = pool.acquire().await.unwrap();
    aq_role_settings::attempts::forget(&mut conn, attempt)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(ran_with(&c, attempt).await, None);
    assert!(pins(&c, attempt).await.is_empty());
}

/// Spec 0014 §6.1: a task whose startup text is over the execution profile's
/// byte limit gets no offer, and its work read says so with both numbers and
/// the limit that was exceeded. Raising the limit lets the next poll offer
/// it, and the recorded reason is removed.
#[tokio::test]
async fn a_startup_over_its_limit_is_a_readiness_reason_until_an_offer_composes() {
    let mut c = setup().await;
    let (pool, task) = (c.env.env.pool.clone(), c.env.lineage.task_id);
    roles::save(
        &pool,
        c.env.actor,
        Kind::Execution,
        "sandbox",
        &roles::execution(64),
    )
    .await;
    let (status, page) = poll(&c).await;
    assert_eq!(status, 200, "{page}");
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "{page}");
    // Schema-checked by the work helper.
    let work = super::holds::full_work(&c, task).await;
    assert_eq!(work["readiness"]["state"], "held", "{work}");
    let reason = &work["readiness"]["reasons"][0];
    assert_eq!(reason["code"], "startup_over_limit", "{work}");
    assert_eq!(reason["limit"], 64);
    assert_eq!(reason["profile_limit"], 64);
    assert_eq!(reason["limiting"], "startup_context_bytes");
    assert_eq!(reason["execution_profile"], "sandbox");
    let bytes = reason["bytes"].as_u64().unwrap();
    assert!(bytes > 64, "{reason}");
    let message = reason["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("{bytes} bytes"))
            && message.contains("limit of 64 bytes")
            && message.contains("startup_context_bytes of execution profile `sandbox`"),
        "{message}"
    );

    roles::save(
        &pool,
        c.env.actor,
        Kind::Execution,
        "sandbox",
        &roles::execution(8192),
    )
    .await;
    c.request.operation_id = Uuid::new_v4();
    offered(&c).await;
    let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM task_offer_refusals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(recorded, 0);
    let work = super::holds::full_work(&c, task).await;
    let codes: Vec<&str> = (work["readiness"]["reasons"].as_array().unwrap().iter())
        .filter_map(|reason| reason["code"].as_str())
        .collect();
    assert!(!codes.contains(&"startup_over_limit"), "{work}");
}

/// An attempt recorded before roles and profiles (spec 0014 §10): its stored
/// selection has the old catalog's shape and its summary names no snapshot.
/// The task's work read still answers and shows what it ran with.
#[tokio::test]
async fn an_attempt_recorded_before_roles_is_still_read() {
    let c = setup().await;
    let native = offered(&c).await;
    let attempt = native.offered.offer.binding.attempt_id;
    let pool = &c.env.env.pool;
    // As migration 0134 leaves such an attempt: the old key in the stored
    // selection and decision, a null snapshot digest and no pin.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let old = "jsonb_build_object('catalog_digest',{0}->'role_snapshot_digest','catalog_revision',1)||({0}-'role_snapshot_digest')";
    sqlx::query(&format!(
        "UPDATE intelligence_attempt_selections SET selection={} WHERE attempt_id=$1",
        old.replace("{0}", "selection")
    ))
    .bind(attempt)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(&format!(
        "UPDATE intelligence_decisions d SET decision=jsonb_set(decision,'{{outcome,resolved}}',{}) FROM profiled_launches l WHERE l.decision_id=d.decision_id AND l.attempt_id=$1",
        old.replace("{0}", "(decision#>'{outcome,resolved}')")
    ))
    .bind(attempt)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE attempt_ran_with SET ran_with=ran_with||'{\"snapshot_digest\":null}' WHERE attempt_id=$1")
        .bind(attempt)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM role_snapshot_pins WHERE attempt_id=$1")
        .bind(attempt)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let stored: Value = sqlx::query_scalar(
        "SELECT selection FROM intelligence_attempt_selections WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(serde_json::from_value::<aq_selection::ResolvedSelection>(stored).is_err());

    // The upgraded server starts with nothing verified in memory.
    c.env.env.state.store.forget_verified_profiled_facts();

    let summary = ran_with(&c, attempt).await.expect("a summary");
    assert_eq!(summary.snapshot_digest, None);
    let work = super::holds::full_work(&c, c.env.lineage.task_id).await;
    assert_eq!(work["execution"]["ran_with"]["role"], "developer");
    assert_eq!(
        work["execution"]["ran_with"]["snapshot_digest"],
        Value::Null
    );
    // The attempt's authority record is still readable: its stored selection
    // digest stands without the old shape being parsed.
    let mut tx = pool.begin().await.unwrap();
    let node = c.env.env.node;
    let view = aq_server::execution::authority::read_attempt(
        &c.env.env.state,
        &mut tx,
        node,
        attempt,
        aq_server::execution::authority::Policy::TerminalHistorical,
    )
    .await
    .expect("the authority view of an attempt recorded before roles");
    assert!(view.selection_digest.is_some());
    // The swarm's occupancy and pill reads count it without an error.
    aq_swarm_settings::admission::counts(&mut tx).await.unwrap();
}

/// A routed task runs on the level document of its class id: another model,
/// and the harness's own reasoning setting (`default`).
#[tokio::test]
async fn a_routed_task_runs_on_the_level_document_of_its_class() {
    let c = setup().await;
    route(&c, "simple-low");
    let native = offered(&c).await;
    let manifest = &native.offered.offer.manifest;
    assert_eq!(
        manifest.intelligence_profile.id.as_str(),
        "intelligence:simple-low"
    );
    assert_eq!(manifest.model.model.as_str(), "model-small");
    assert_eq!(manifest.model.reasoning.as_str(), "default");
    let summary = ran_with(&c, native.offered.offer.binding.attempt_id)
        .await
        .unwrap();
    assert_eq!(
        (summary.level.as_str(), summary.reasoning.as_str()),
        ("simple-low", "default")
    );
}

/// A task filed with the reviewer role gets the reviewer's prompt and only
/// its operations; the project's notes for that role follow the role text.
#[tokio::test]
async fn a_reviewer_task_gets_the_reviewer_prompt_its_operations_and_the_project_notes() {
    let mut c = setup().await;
    let notes = "Run the slow suite before you approve.";
    c.env.env.body["role_notes"] = json!({"reviewer": notes, "developer": "Not for a reviewer."});
    let bind =
        json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"binding":c.env.env.body});
    let path = crate::common::selection::path();
    let (status, body) =
        crate::common::selection::call(&c.env.env.app, &path, Some(bind), "owner@localhost").await;
    assert_eq!(status, 200, "{body}");
    *c.metadata.lock().unwrap() = json!({"aq:role":"reviewer"});
    let native = offered(&c).await;
    let manifest = &native.offered.offer.manifest;
    assert_eq!(manifest.agent_profile.id.as_str(), "role:reviewer");
    assert_eq!(operations(&native), ["task.read", "knowledge.read"]);
    assert_eq!(
        manifest.permissions.requested.len(),
        2,
        "it asks for no source.write"
    );
    let text = startup_text(&c, &native).await;
    let role = c.env.snapshot.role("reviewer").unwrap();
    let after_platform = format!(
        "\n\n## Role\n{}\n\n## Rules\n{}\n\n{notes}\n\n{{",
        role.role_md.trim(),
        role.rules_md.trim()
    );
    assert!(text.contains(&after_platform), "{text}");
    assert!(text.contains("You are a code reviewer."));
    assert!(!text.contains("You are a software developer."));
    assert!(!text.contains("Not for a reviewer."));
}

/// A startup text over the execution profile's byte limit is refused: no
/// offer and no attempt; the next ready task is not blocked by it.
#[tokio::test]
async fn a_startup_text_over_its_byte_limit_is_refused() {
    let c = setup().await;
    let pool = &c.env.env.pool;
    roles::save(
        pool,
        c.env.actor,
        Kind::Execution,
        "sandbox",
        &roles::execution(600),
    )
    .await;
    for _ in 0..2 {
        let (status, body) = poll(&c).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["body"]["offers"], json!([]), "{body}");
    }
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempts")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
    // Within the limit again, the same task is offered.
    roles::save(
        pool,
        c.env.actor,
        Kind::Execution,
        "sandbox",
        &roles::execution(8192),
    )
    .await;
    offered(&c).await;
}

/// No offer needs a fact, a pin or a measurement: the control response has
/// no qualification and the measurement route is gone.
#[tokio::test]
async fn control_pins_no_qualification_and_no_measurement_route_exists() {
    let c = setup().await;
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    aq_machine_control::claim_key(&mut conn, c.env.env.node, "desk")
        .await
        .unwrap();
    drop(conn);
    let (status, body) = send(&c, "/v1/worker/control", &json!({"schema_version":1})).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("qualification").is_none(), "{body}");
    let (status, _) = send(
        &c,
        "/v1/worker/qualifications",
        &json!({"schema_version":1,"calls":[]}),
    )
    .await;
    assert_eq!(status, 404);
    offered(&c).await;
}
