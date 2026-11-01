//! Actual phase owners with explicit synthetic Graph/runtime/installation facts.
//! New policy versions change only new admissions; no provider or renewal claim.
mod common;
#[allow(dead_code)]
#[path = "common/phase_execution.rs"]
mod fixture;
use aq_admission::{ArtifactKind, ArtifactRef, Digest, Text};
use aq_server::profiled_execution::{
    accepted_preparation::PolicyDefinition, config::ProfiledConfig, definitions::Definition,
    phase_execution,
};
use serde_json::Value;
use uuid::Uuid;
fn lease_policy(c: &mut ProfiledConfig, maximum: u64) {
    let old_policy = c
        .definition_bodies
        .iter()
        .find(|d| {
            d.artifact()
                .id
                .as_str()
                .starts_with("aq.accepted-preparation-policy/")
        })
        .unwrap()
        .clone();
    let old_qualification = c
        .definition_bodies
        .iter()
        .find(|d| {
            d.artifact()
                .id
                .as_str()
                .starts_with("aq.phase-execution-qualification/")
        })
        .unwrap()
        .clone();
    let mut policy: PolicyDefinition = serde_json::from_str(old_policy.body()).unwrap();
    policy.phase.maximum_execution_lease_ms = maximum;
    let body = serde_json::to_string(&policy).unwrap();
    let reference = ArtifactRef {
        id: old_policy.artifact().id.clone(),
        version: Text::new("2").unwrap(),
        digest: Digest::of(body.as_bytes()),
    };
    let mut qualification: Value = serde_json::from_str(old_qualification.body()).unwrap();
    qualification["proposal_policy"] = serde_json::to_value(&reference).unwrap();
    let qbody = serde_json::to_string(&qualification).unwrap();
    let qref = ArtifactRef {
        id: old_qualification.artifact().id.clone(),
        version: Text::new("2").unwrap(),
        digest: Digest::of(qbody.as_bytes()),
    };
    c.definition_bodies.retain(|d| {
        d.artifact() != old_policy.artifact() && d.artifact() != old_qualification.artifact()
    });
    c.definition_bodies
        .push(Definition::new(reference, ArtifactKind::Definition, body).unwrap());
    c.definition_bodies
        .push(Definition::new(qref, ArtifactKind::Definition, qbody).unwrap());
}
fn thirty_seconds(c: &mut ProfiledConfig) {
    lease_policy(c, 30000)
}
fn maximum_initial(c: &mut ProfiledConfig) {
    lease_policy(c, 120000)
}
async fn original_clocks(f: &fixture::Prepared) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('proposal',p.proposal,'accepted',a.receipt) FROM profiled_v2_proposals p JOIN profiled_v2_accepts a USING(attempt_id) WHERE p.attempt_id=$1").bind(f.prepared.original.binding.attempt_id).fetch_one(&f.c.env.env.pool).await.unwrap()
}
#[tokio::test]
async fn explicit_new_policy_supports_longer_initial_lease_without_retiming_or_renewal() {
    for (requested, configure) in [
        (30000_u64, thirty_seconds as fn(&mut ProfiledConfig)),
        (120000, maximum_initial),
    ] {
        let f = Box::pin(fixture::prepared_with_config(true, configure)).await;
        assert_eq!(f.p.policy.maximum_execution_lease_ms, requested);
        let decision:Value=sqlx::query_scalar("SELECT d.decision FROM intelligence_decisions d JOIN profiled_launches l USING(decision_id) WHERE l.attempt_id=$1").bind(f.prepared.original.binding.attempt_id).fetch_one(&f.c.env.env.pool).await.unwrap();
        assert_eq!(
            decision["inputs"]["budget"]["elapsed_ms"], 0,
            "first-supported budget only; no elapsed reset"
        );
        let before = original_clocks(&f).await;
        let auth = fixture::auth(&f.c).await;
        let delivery =
            phase_execution::prepare(&f.c.env.env.state, &auth, &f.prepared, Uuid::new_v4())
                .await
                .unwrap();
        let r = &delivery.receipt;
        let wall = f.p.original.offered.offer.manifest.resources.wall_time_ms;
        assert_eq!(
            wall, 60000,
            "explicit synthetic profile's original wall ceiling"
        );
        assert_eq!(r.execution_deadline_ms - r.issued_at_ms, 60000);
        assert_eq!(r.grant.expires_at_ms - r.issued_at_ms, requested.min(60000));
        assert!(r.grant.expires_at_ms - r.issued_at_ms > 10000);
        assert!(
            r.grant.expires_at_ms > f.a.lease.expires_at_ms,
            "retrieval/preparation expiry is not running authority expiry"
        );
        assert_eq!(original_clocks(&f).await, before);
        let replay =
            phase_execution::prepare(&f.c.env.env.state, &auth, &f.prepared, Uuid::new_v4())
                .await
                .unwrap();
        assert_eq!(r.digest().unwrap(), replay.receipt.digest().unwrap());
        assert_eq!(
            replay.receipt.execution_deadline_ms,
            r.execution_deadline_ms
        );
        assert_eq!(original_clocks(&f).await, before);
    }
}
