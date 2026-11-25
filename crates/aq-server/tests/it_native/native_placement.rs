//! Sandbox placement facts and their fence re-check (moved from the
//! `profiled_execution::placement` unit tests; public API only).
use aq_admission_sandbox::sandbox::{BackendAdvertisement, BackendCapability, BackendKind, Money};
use aq_selection::NodeCeiling;
use aq_server::error::AppError;
use aq_server::profiled_execution::placement::*;
use uuid::Uuid;

fn adv(max: u32, spend_ceiling: Option<(&str, u64)>) -> BackendAdvertisement {
    serde_json::from_value(serde_json::json!({
        "kind": "boat",
        "capabilities": ["vm-isolation"],
        "max_concurrent_sandboxes": max,
        "spend_ceiling": spend_ceiling.map(|(c, m)| serde_json::json!({"currency": c, "micros": m})),
        "templates": [],
    }))
    .unwrap()
}
fn money(currency: &str, micros: u64) -> Money {
    Money {
        currency: currency.into(),
        micros,
    }
}

#[test]
fn placement_prefers_local_then_boat_then_cloudflare() {
    use BackendKind::*;
    let peer = |kind, eligible| Peer { kind, eligible };
    let all = [
        peer(DockerLocal, true),
        peer(Boat, true),
        peer(CloudflareContainers, true),
    ];
    // Default order: only the most preferred eligible backend gets offers.
    assert_eq!(preferred_peer(DockerLocal, &DEFAULT_PREFERENCE, &all), None);
    assert_eq!(
        preferred_peer(Boat, &DEFAULT_PREFERENCE, &all),
        Some(DockerLocal)
    );
    assert_eq!(
        preferred_peer(CloudflareContainers, &DEFAULT_PREFERENCE, &all),
        Some(DockerLocal)
    );
    // An ineligible (offline, full or unsupported) local node holds nothing.
    let no_local = [
        peer(DockerLocal, false),
        peer(Boat, true),
        peer(CloudflareContainers, true),
    ];
    assert_eq!(preferred_peer(Boat, &DEFAULT_PREFERENCE, &no_local), None);
    assert_eq!(
        preferred_peer(CloudflareContainers, &DEFAULT_PREFERENCE, &no_local),
        Some(Boat)
    );
    let only_cloud = [peer(DockerLocal, false), peer(Boat, false)];
    assert_eq!(
        preferred_peer(CloudflareContainers, &DEFAULT_PREFERENCE, &only_cloud),
        None
    );
    // ProjectPolicy override reverses the order; unlisted kinds rank last.
    let override_ = [CloudflareContainers, Boat];
    assert_eq!(preferred_peer(CloudflareContainers, &override_, &all), None);
    assert_eq!(
        preferred_peer(DockerLocal, &override_, &all),
        Some(CloudflareContainers)
    );
    assert_eq!(
        preferred_peer(Boat, &override_, &all),
        Some(CloudflareContainers)
    );
    // A peer of the same backend never holds.
    assert_eq!(
        preferred_peer(Boat, &DEFAULT_PREFERENCE, &[peer(Boat, true)]),
        None
    );
    // Policy validation.
    let p = |preference: Vec<BackendKind>| PlacementPolicy {
        preference,
        required_capabilities: vec![],
    };
    assert!(p(vec![Boat, DockerLocal]).validate().is_ok());
    assert!(p(vec![Boat, Boat]).validate().is_err());
    let policy: PlacementPolicy = serde_json::from_value(serde_json::json!({
        "preference": ["cloudflare-containers"],
        "required_capabilities": ["nested-docker"]
    }))
    .unwrap();
    assert_eq!(policy.preference, [CloudflareContainers]);
}

#[test]
fn node_at_spend_ceiling_refuses_with_reason() {
    let usd = |m| money("USD", m);
    assert_eq!(ceiling(&adv(2, None), 1, None), None);
    assert_eq!(
        ceiling(&adv(2, None), 2, None),
        Some(NodeCeiling::Concurrency)
    );
    let limited = adv(2, Some(("USD", 1_000)));
    assert_eq!(ceiling(&limited, 0, Some(&usd(999))), None);
    assert_eq!(
        ceiling(&limited, 0, Some(&usd(1_000))),
        Some(NodeCeiling::Spend)
    );
    assert_eq!(NodeCeiling::Spend.reason(), "node_spend_ceiling_reached");
    // Unaccounted or incomparable spend never degrades to "under ceiling".
    assert_eq!(
        ceiling(&limited, 0, None),
        Some(NodeCeiling::SpendUnaccounted)
    );
    assert_eq!(
        ceiling(&limited, 0, Some(&money("EUR", 0))),
        Some(NodeCeiling::SpendUnaccounted)
    );
    // Concurrency is checked first.
    assert_eq!(
        ceiling(&limited, 2, Some(&usd(5_000))),
        Some(NodeCeiling::Concurrency)
    );
}

#[test]
fn accounted_node_spend_never_assumes_unknown_costs_are_zero() {
    use aq_accounting::pricing::Summary;
    use aq_accounting::wire::{ArtifactRef, Cost, Digest, Text, UnknownReason};
    let text = |s: &str| Text::new(s).unwrap();
    let reported = |micros| Cost::ProviderReported {
        amount_microunits: micros,
        currency: text("USD"),
        receipt: ArtifactRef {
            id: text("receipt"),
            version: text("1"),
            digest: Digest::of(b"receipt"),
        },
    };
    let unknown = Cost::Unknown {
        reason: UnknownReason::NotReported,
    };
    let summary =
        |costs: Vec<Cost>, spent: Option<u64>, currency: Option<&str>, complete| Summary {
            costs,
            spent_microunits: spent,
            currency: currency.map(text),
            complete,
            reason: None,
        };
    let limit = money("USD", 1_000);
    // Money has no PartialEq; compare its parts.
    let acc = |s: &Summary| accounted(s, &limit).map(|m| (m.currency, m.micros));
    let usd = |m| Some(("USD".to_owned(), m));
    // No reported calls: the node has spent nothing, in the limit currency.
    assert_eq!(acc(&summary(vec![], Some(0), None, false)), usd(0));
    // Complete accounting counts as is, under or at the limit.
    let complete = |m| summary(vec![reported(m)], Some(m), Some("USD"), true);
    assert_eq!(acc(&complete(999)), usd(999));
    assert_eq!(acc(&complete(1_000)), usd(1_000));
    // An incomplete lower bound counts only once it reaches the limit.
    let partial = |m| {
        summary(
            vec![reported(m), unknown.clone()],
            Some(m),
            Some("USD"),
            false,
        )
    };
    assert_eq!(acc(&partial(999)), None);
    assert_eq!(acc(&partial(1_000)), usd(1_000));
    // Only unknown costs, mixed currencies or another currency: unaccounted.
    assert_eq!(
        acc(&summary(vec![unknown.clone()], Some(0), None, false)),
        None
    );
    assert_eq!(
        acc(&summary(vec![reported(1), reported(2)], None, None, false)),
        None
    );
    assert_eq!(
        acc(&summary(vec![reported(5)], Some(5), Some("EUR"), true)),
        None
    );
    // Fed into the ceiling: under receives offers, at refuses with a reason.
    let limited = adv(2, Some(("USD", 1_000)));
    let spent = accounted(&complete(999), &limit);
    assert_eq!(ceiling(&limited, 0, spent.as_ref()), None);
    let spent = accounted(&complete(1_000), &limit);
    assert_eq!(
        ceiling(&limited, 0, spent.as_ref()),
        Some(NodeCeiling::Spend)
    );
    let spent = accounted(&partial(999), &limit);
    assert_eq!(
        ceiling(&limited, 0, spent.as_ref()),
        Some(NodeCeiling::SpendUnaccounted)
    );
}

#[tokio::test]
async fn sandbox_facts_pin_template_and_hold_for_preferred_peer() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let hex = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
    let user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,email,role,created_at) VALUES($1,'o@example.test','owner',now())",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();
    let node = |kind: &str, expires: u64| {
        let pool = pool.clone();
        let kind = kind.to_owned();
        let template = hex('a');
        async move {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'n',$2,'t',1,now())")
                .bind(id).bind(id.to_string()).execute(&pool).await.unwrap();
            let backend = serde_json::json!({"backend":{"kind":kind,"capabilities":["vm-isolation"],"max_concurrent_sandboxes":1,"spend_ceiling":null,
                "templates":[{"digest":template,"harnesses":[{"harness":"codex","version":"1"}],"estimated_cold_start_ms":1,"estimated_cold_start_cost":{"currency":"USD","micros":0}}]}});
            sqlx::query(
                "INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)",
            )
            .bind(id)
            .bind(Uuid::new_v4())
            .bind(backend)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)")
                .bind(id).bind(Uuid::new_v4()).bind(serde_json::json!({"expires_at_ms":expires})).execute(&pool).await.unwrap();
            id
        }
    };
    let cloud = node("cloudflare-containers", 2_000).await;
    // The template the operator's allowlist resolves `ci@1` to.
    let template = hex('a');
    let book = aq_accounting::pricing::RateCardBook::default();
    let ledger = Ledger {
        pool: &pool,
        book: &book,
    };
    let facts = |id, now| facts_in(&pool, &ledger, None, id, Some(&template), "ci@1", now);
    let only = facts(cloud, 1_000).await.unwrap().unwrap();
    assert_eq!(only.preferred_peer, None);
    assert_eq!(only.ceiling, None);
    assert_eq!(only.capabilities, ["vm-isolation"]);
    assert_eq!(only.environment_templates[0].template.as_str(), hex('a'));
    assert_eq!(only.templates[0].harnesses[0].version, "1");
    let boat = node("boat", 1_500).await;
    let held = facts(cloud, 1_000).await.unwrap().unwrap();
    assert_eq!(held.preferred_peer, Some(aq_selection::BackendKind::Boat));
    // The hold is bounded by the peer's snapshot expiry.
    assert_eq!(
        facts(cloud, 1_600).await.unwrap().unwrap().preferred_peer,
        None
    );
    // A required capability the peer lacks makes it ineligible; no hold.
    let nested = PlacementPolicy {
        preference: vec![],
        required_capabilities: vec![BackendCapability::NestedDocker],
    };
    let refused = facts_in(
        &pool,
        &ledger,
        Some(&nested),
        cloud,
        Some(&template),
        "ci@1",
        1_000,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(refused.preferred_peer, None);
    assert_eq!(refused.required_capabilities, ["nested-docker"]);
    assert!(
        facts(boat, 1_000)
            .await
            .unwrap()
            .unwrap()
            .preferred_peer
            .is_none()
    );
    // SBX25: a node above 85% used refuses with node_disk_pressure, and
    // a pressured peer is no reason to hold work for it.
    let disk = |id: Uuid, used: u64| {
        let pool = pool.clone();
        async move {
            let report = aq_server::node_storage::StorageReport {
                disk_total_bytes: 100,
                disk_used_bytes: used,
                disk_free_bytes: 100 - used,
                ..Default::default()
            };
            let mut conn = pool.acquire().await.unwrap();
            aq_server::node_storage::record(&mut conn, Some(id), &report, 1)
                .await
                .unwrap();
        }
    };
    disk(boat, 85).await;
    assert_eq!(facts(boat, 1_000).await.unwrap().unwrap().ceiling, None);
    disk(boat, 86).await;
    let pressured = facts(boat, 1_000).await.unwrap().unwrap();
    assert_eq!(pressured.ceiling, Some(NodeCeiling::DiskPressure));
    assert_eq!(NodeCeiling::DiskPressure.reason(), "node_disk_pressure");
    assert_eq!(
        facts(cloud, 1_000).await.unwrap().unwrap().preferred_peer,
        None
    );
    // Host-native nodes carry no placement facts.
    assert_eq!(facts(Uuid::new_v4(), 1_000).await.unwrap(), None);
}

fn refusal(result: Result<(), AppError>) -> Option<String> {
    match result {
        Ok(()) => None,
        Err(AppError::Conflict(reason)) => Some(reason),
        Err(other) => panic!("unexpected fence error {other:?}"),
    }
}

#[tokio::test]
async fn fence_rechecks_ceiling_and_template_after_selection() {
    let db = aq_server::test_support::TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let hex = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
    let user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,email,role,created_at) VALUES($1,'o@example.test','owner',now())",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();
    // The template the operator's allowlist resolves `ci@1` to, as each
    // derivation is handed it.
    let resolved = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let pin = |template: Option<String>| *resolved.lock().unwrap() = template;
    pin(Some(hex('a')));
    let registration = |kind: &str, templates: serde_json::Value| {
        serde_json::json!({"backend":{"kind":kind,"capabilities":["vm-isolation"],
            "max_concurrent_sandboxes":1,"spend_ceiling":null,"templates":templates}})
    };
    let template_a = serde_json::json!([{"digest":hex('a'),"harnesses":[{"harness":"codex","version":"1"}],
        "estimated_cold_start_ms":1,"estimated_cold_start_cost":{"currency":"USD","micros":0}}]);
    let node = |kind: &str| {
        let pool = pool.clone();
        let registration = registration(kind, template_a.clone());
        async move {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'n',$2,'t',1,now())")
                .bind(id).bind(id.to_string()).execute(&pool).await.unwrap();
            sqlx::query(
                "INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)",
            )
            .bind(id)
            .bind(Uuid::new_v4())
            .bind(registration)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)")
                .bind(id).bind(Uuid::new_v4()).bind(serde_json::json!({"expires_at_ms":5_000})).execute(&pool).await.unwrap();
            id
        }
    };
    let boat = node("boat").await;
    let artifact =
        |id: &str, digest: String| serde_json::json!({"id":id,"version":"1","digest":digest});
    let choice: aq_selection::ConcreteChoice = serde_json::from_value(serde_json::json!({
        "candidate_id":"c","profile_candidate_id":"p","provider":"openai","model":"m",
        "native_reasoning":"high",
        "harness":"codex","cli_version":"1",
        "adapter":artifact("adapter",hex('1')),"environment":artifact("ci",hex('a')),
        "qualification":artifact("q",hex('2')),"failure_classifier":null,
        "credential_profile":artifact("cred",hex('3')),
        "account_reference":"acct","endpoint_reference":"ep","launch_digest":hex('4'),
        "resources":{"context_bytes":1,"wall_time_ms":1,"memory_bytes":1,"cpu_millis":1,
            "process_limit":1,"cost_limit_microunits":null,"currency":null,
            "require_hard_cost_enforcement":false},
        "max_output_tokens":1,"estimate":null
    }))
    .unwrap();
    let fence_facts = |exclude_attempt| {
        let pool = pool.clone();
        let template = resolved.lock().unwrap().clone();
        async move {
            let mut conn = pool.acquire().await.unwrap();
            let book = aq_accounting::pricing::RateCardBook::default();
            let ledger = Ledger {
                pool: &pool,
                book: &book,
            };
            let derivation = Derivation {
                peers: false,
                exclude_attempt,
            };
            derive(
                &mut conn,
                &ledger,
                None,
                boat,
                template.as_deref(),
                "ci@1",
                1_000,
                derivation,
            )
            .await
            .unwrap()
        }
    };
    let book = aq_accounting::pricing::RateCardBook::default();
    let ledger = Ledger {
        pool: &pool,
        book: &book,
    };
    let selected = facts_in(&pool, &ledger, None, boat, Some(&hex('a')), "ci@1", 1_000)
        .await
        .unwrap();
    assert!(selected.is_some());
    let at_fence = |current: Option<aq_selection::Placement>| {
        refusal(recheck(selected.as_ref(), current.as_ref(), &choice))
    };
    assert_eq!(at_fence(fence_facts(None).await), None);
    // Host-native stays unchecked; a registration appearing or vanishing
    // since selection refuses.
    assert_eq!(refusal(recheck(None, None, &choice)), None);
    let changed = Some("placement_registration_changed".to_owned());
    assert_eq!(at_fence(None), changed);
    assert_eq!(
        refusal(recheck(None, fence_facts(None).await.as_ref(), &choice)),
        changed
    );

    // The preference hold is selection-only: a preferred peer coming
    // online after selection does not refuse the fence.
    node("docker-local").await;
    let held = facts_in(&pool, &ledger, None, boat, Some(&hex('a')), "ci@1", 1_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        held.preferred_peer,
        Some(aq_selection::BackendKind::DockerLocal)
    );
    assert_eq!(fence_facts(None).await.unwrap().preferred_peer, None);
    assert_eq!(at_fence(fence_facts(None).await), None);

    // The node reached its concurrency ceiling since selection.
    let attempt = Uuid::new_v4();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) VALUES($1,$2,$3,$4,$5,'{}')")
        .bind(attempt).bind(Uuid::new_v4()).bind(boat).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&pool).await.unwrap();
    assert_eq!(
        at_fence(fence_facts(None).await).as_deref(),
        Some("node_concurrency_ceiling_reached")
    );
    // The fenced attempt's own reservation does not count against it.
    assert_eq!(at_fence(fence_facts(Some(attempt)).await), None);
    sqlx::query("UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1")
        .bind(attempt)
        .execute(&pool)
        .await
        .unwrap();

    // The allowlist later resolves the environment to another template, or to none.
    pin(Some(hex('b')));
    assert_eq!(
        at_fence(fence_facts(None).await).as_deref(),
        Some("environment_template_digest_mismatch")
    );
    pin(None);
    assert_eq!(
        at_fence(fence_facts(None).await).as_deref(),
        Some("environment_template_unresolved")
    );
    pin(Some(hex('a')));
    assert_eq!(at_fence(fence_facts(None).await), None);

    // The node's registration no longer advertises the template.
    sqlx::query("UPDATE delegated_node_boots SET registration=$2 WHERE node_id=$1")
        .bind(boat)
        .bind(registration("boat", serde_json::json!([])))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        at_fence(fence_facts(None).await).as_deref(),
        Some("template_not_advertised")
    );
    assert!(!peer_scan_truncated(255));
    assert!(peer_scan_truncated(256));
}
