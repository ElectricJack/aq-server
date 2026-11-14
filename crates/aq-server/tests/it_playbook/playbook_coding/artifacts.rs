//! In-memory owner + fake Host8 peer; no daemon, no database.
use super::{Log, authority, checks, owner, request, settle};
use aq_playbook_host::model::{Artifact, Registry};
use aq_playbook_host::{
    MemoryStorage, Owner, Request,
    runner::{Activation, InspectionKind, InspectionQuery},
};
use aq_server::playbooks::{
    artifacts::*,
    coding::{CodingArtifacts, CodingPolicy, ROOT},
    domain::{Error, Result},
    review_domain::CatalogProfile,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

const DEADLINE: Duration = Duration::from_secs(5);

struct OwnerCatalog<'a>(&'a Owner);
impl Catalog for OwnerCatalog<'_> {
    fn registry(&self) -> Registry {
        CatalogProfile::Host8.registry()
    }
    async fn install(&self, artifact: Artifact) -> Result<()> {
        let artifact = Box::new(artifact);
        self.0
            .call(Request::Install { artifact }, DEADLINE)
            .await
            .map(|_| ())
            .map_err(|_| Error::Unknown)
    }
    async fn activate(&self, binding: &str, digest: &str) -> Result<()> {
        let request = Request::Activate {
            binding: binding.into(),
            artifact: digest.into(),
            authority: authority(),
        };
        self.0
            .call(request, DEADLINE)
            .await
            .map(|_| ())
            .map_err(|_| Error::Invalid)
    }
    async fn current(&self, binding: &str) -> Result<Option<String>> {
        let binding = binding.into();
        match self.0.call(Request::Binding { binding }, DEADLINE).await {
            Ok(v) => Ok(Some(serde_json::from_value::<Activation>(v)?.artifact)),
            Err(aq_playbook_host::Error::Invalid) => Ok(None),
            Err(_) => Err(Error::Unknown),
        }
    }
}

async fn install_all(c: &OwnerCatalog<'_>, l: &MemoryLedger, tag: &str, a: &CodingArtifacts) {
    for (i, artifact) in a.install_order().into_iter().enumerate() {
        let bytes = bytes_of(&serde_json::to_value(artifact).unwrap()).unwrap();
        install(c, l, &format!("{tag}-{i}"), &bytes).await.unwrap();
    }
}

async fn admit(o: &Owner, policy: &CodingPolicy, key: &str) -> String {
    let request = Request::Admit {
        binding: ROOT.into(),
        entry: "main".into(),
        input: policy.input(&request(key)),
        authority: authority(),
        now_ms: 1,
    };
    let run = o.call(request, DEADLINE).await.unwrap();
    run.as_str().unwrap().into()
}

#[tokio::test]
async fn activation_switches_future_runs_only() {
    let policy_a = CodingPolicy::project_default(checks());
    let mut policy_b = policy_a.clone();
    policy_b.max_repairs = 1;
    let a = policy_a.build().unwrap();
    let b = policy_b.build().unwrap();
    assert_ne!(a.root.digest, b.root.digest);

    let store = MemoryStorage::default();
    let log: Log = Arc::default();
    let o = owner("activation", store.clone(), log, vec![]).await;
    let c = OwnerCatalog(&o);
    let l = MemoryLedger::default();
    install_all(&c, &l, "install-a", &a).await;
    install_all(&c, &l, "install-b", &b).await;

    let first = activate(&c, &l, "act-a", ROOT, &a.root.digest)
        .await
        .unwrap();
    assert_eq!(first["previous"], Value::Null);
    let mut runs = vec![admit(&o, &policy_a, "run-1").await];

    let switched = activate(&c, &l, "act-b", ROOT, &b.root.digest)
        .await
        .unwrap();
    assert_eq!(switched["previous"], a.root.digest.as_str());
    assert_eq!(switched["duplicate"], false);
    runs.push(admit(&o, &policy_b, "run-2").await);

    let back = rollback(&c, &l, "rollback-1", ROOT).await.unwrap();
    assert_eq!(back["artifact"], a.root.digest.as_str());
    assert_eq!(back["previous"], b.root.digest.as_str());
    runs.push(admit(&o, &policy_a, "run-3").await);

    // Idempotent: replaying an op id returns the record and changes nothing;
    // reusing it for a different request is a conflict.
    let replay = activate(&c, &l, "act-b", ROOT, &b.root.digest)
        .await
        .unwrap();
    assert_eq!(replay["duplicate"], true);
    assert_eq!(c.current(ROOT).await.unwrap(), Some(a.root.digest.clone()));
    assert_eq!(
        activate(&c, &l, "act-b", ROOT, &a.root.digest).await,
        Err(Error::Conflict)
    );
    // Nothing left below the bottom of the stack to roll back to.
    assert_eq!(
        rollback(&c, &l, "rollback-2", ROOT).await,
        Err(Error::Conflict)
    );

    // Every run kept the artifact pinned at its admission. Finished runs
    // leave the State, so read them through inspection (history).
    settle(&o, &store, "activation").await;
    let mut roots = Vec::new();
    for run in runs {
        let query = InspectionQuery {
            kind: InspectionKind::Runs,
            run: Some(run),
            after: None,
            limit: 1,
        };
        let page = o.call(Request::Inspect { query }, DEADLINE).await.unwrap();
        let item = &page["items"][0];
        roots.push((item["artifact"].clone(), item["outcome"].clone()));
    }
    let pinned: Vec<_> = roots
        .iter()
        .map(|(artifact, outcome)| (artifact.as_str().unwrap(), outcome.as_str()))
        .collect();
    assert_eq!(
        pinned,
        [
            (a.root.digest.as_str(), Some("published")),
            (b.root.digest.as_str(), Some("published")),
            (a.root.digest.as_str(), Some("published")),
        ]
    );
}

#[test]
fn validate_refuses_tampered_bytes() {
    let a = CodingPolicy::project_default(checks()).build().unwrap();
    let bytes = bytes_of(&serde_json::to_value(&a.root).unwrap()).unwrap();
    assert!(validate(&CatalogProfile::Host8.registry(), &bytes).is_ok());
    let mut tampered = bytes.clone();
    *tampered.last_mut().unwrap() = b' ';
    assert_eq!(
        validate(&CatalogProfile::Host8.registry(), &tampered).map(|_| ()),
        Err(Error::Invalid)
    );
}

mod pg;
