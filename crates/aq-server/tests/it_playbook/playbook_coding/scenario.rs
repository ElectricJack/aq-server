use super::*;

pub(super) fn authority() -> Authority {
    Authority {
        actor: "owner".into(),
        scope: "project".into(),
        capabilities: HOST7_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
    }
}
pub(super) fn checks() -> Vec<RequiredCheck> {
    vec![RequiredCheck {
        id: "cargo-test".into(),
        validator_id: "rust.cargo-test".into(),
        validator_version: "1".into(),
        validator_digest: hash_bytes(b"validator"),
    }]
}
pub(super) fn request(key: &str) -> CodingRequest {
    CodingRequest {
        subject: json!({"id": "task-1", "revision": 1}),
        context: "Implement the thing".into(),
        base: json!({"id": "main", "revision": 1}),
        destination: json!({"id": "main", "revision": 1}),
        source: json!({"format": "sha1", "hex": "0".repeat(40)}),
        visibility: "project".into(),
        key: key.into(),
        decision_expires_at_ms: None,
    }
}
pub(super) async fn owner(
    namespace: &str,
    store: MemoryStorage,
    log: Log,
    reviews: Vec<&'static str>,
) -> Owner {
    Peer::new(log, reviews).start(namespace, store).await
}
pub(super) async fn install(owner: &Owner, artifacts: &CodingArtifacts) {
    for a in artifacts.install_order() {
        owner
            .call(
                Request::Install {
                    artifact: Box::new(a.clone()),
                },
                DEADLINE,
            )
            .await
            .unwrap();
    }
}
/// Ticks until every run is terminal; returns the root runs' outcomes.
pub(super) async fn settle(
    owner: &Owner,
    store: &MemoryStorage,
    namespace: &str,
) -> Vec<(String, Option<String>)> {
    for now in 10..200 {
        owner
            .call(Request::Tick { now_ms: now }, DEADLINE)
            .await
            .unwrap();
        let state = store.clone().load(namespace).unwrap().state;
        if state.runs.values().all(|r| r.status.terminal()) {
            return root_outcomes(owner).await;
        }
    }
    panic!("coding runs did not settle");
}
/// Root runs' artifacts and outcomes through inspection, which serves live
/// runs and the history finished runs leave the State for.
pub(super) async fn root_outcomes(owner: &Owner) -> Vec<(String, Option<String>)> {
    let mut roots = Vec::new();
    let mut after = None;
    loop {
        let query = aq_playbook_host::runner::InspectionQuery {
            kind: aq_playbook_host::runner::InspectionKind::Runs,
            run: None,
            after,
            limit: 32,
        };
        let page = owner
            .call(Request::Inspect { query }, DEADLINE)
            .await
            .unwrap();
        for run in page["items"].as_array().unwrap() {
            if run["parent"].is_null() {
                let outcome = run["outcome"].as_str().map(String::from);
                roots.push((run["artifact"].as_str().unwrap().into(), outcome));
            }
        }
        match page["next"].as_str() {
            Some(next) => after = Some(next.into()),
            None => return roots,
        }
    }
}

pub(super) async fn launch(
    owner: &Owner,
    artifacts: &CodingArtifacts,
    authority: Authority,
    input: Value,
) {
    install(owner, artifacts).await;
    owner
        .call(
            Request::Activate {
                binding: ROOT.into(),
                artifact: artifacts.root.digest.clone(),
                authority: authority.clone(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Admit {
                binding: ROOT.into(),
                entry: "main".into(),
                input,
                authority,
                now_ms: 1,
            },
            DEADLINE,
        )
        .await
        .unwrap();
}
