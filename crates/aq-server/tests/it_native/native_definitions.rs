//! Public body owner tests. Synthetic definitions never qualify a real runtime.
use crate::common;
use aq_admission::{ArtifactKind, ArtifactRef, Digest, Text};
use aq_server::profiled_execution::{
    config::ProfiledConfig,
    definitions::{self, Definition},
};
use common::profiled::{NOW, fixture};
use uuid::Uuid;
fn definition(id: &str, body: &str) -> Definition {
    Definition::new(
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(body.as_bytes()),
        },
        ArtifactKind::Definition,
        body.into(),
    )
    .unwrap()
}
#[test]
fn exact_bytes_kind_identity_bounds_and_configuration_roundtrip() {
    let valid = definition("synthetic-definition", "synthetic public evidence");
    let mut raw = serde_json::to_value(&valid).unwrap();
    raw["body"] = "changed bytes".into();
    assert!(serde_json::from_value::<Definition>(raw).is_err());
    let mut raw = serde_json::to_value(&valid).unwrap();
    raw["kind"] = "source".into();
    assert!(serde_json::from_value::<Definition>(raw).is_err());
    let mut raw = serde_json::to_value(&valid).unwrap();
    raw["private_path"] = "/not-accepted".into();
    assert!(serde_json::from_value::<Definition>(raw).is_err());
    assert!(definitions::validate_catalog(&[valid.clone(), valid.clone()]).is_err());
    let bodies = (0..5)
        .map(|n| {
            definition(
                &format!("synthetic-large-{n}"),
                &"x".repeat(definitions::MAX_BODY_BYTES),
            )
        })
        .collect::<Vec<_>>();
    assert!(definitions::validate_catalog(&bodies).is_err());
    assert!(
        Definition::new(
            valid.artifact().clone(),
            ArtifactKind::Definition,
            "x".repeat(definitions::MAX_BODY_BYTES + 1)
        )
        .is_err()
    );
    let empty = ProfiledConfig {
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![],
        definition_bodies: vec![],
    };
    assert_eq!(
        empty.configuration_digest().unwrap(),
        aq_selection::digest(&empty).unwrap(),
        "empty field preserves old authority bytes"
    );
    let mut configured = empty.clone();
    configured.definition_bodies.push(valid);
    let bytes = serde_json::to_vec(&configured).unwrap();
    let replay: ProfiledConfig = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(bytes, serde_json::to_vec(&replay).unwrap());
    assert_eq!(
        configured.configuration_digest().unwrap(),
        replay.configuration_digest().unwrap()
    );
    assert_ne!(
        empty.configuration_digest().unwrap(),
        configured.configuration_digest().unwrap()
    );
    configured.definition_bodies[0] =
        definition("synthetic-definition", "different actual evidence");
    assert_ne!(
        configured.configuration_digest().unwrap(),
        replay.configuration_digest().unwrap()
    );
}
#[tokio::test]
async fn real_receipts_missing_bodies_and_immutable_definition_versions() {
    let mut f = fixture().await;
    let pool = f.db.pool().await;
    let one = definition("synthetic-definition", "public synthetic adapter evidence");
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(std::sync::Arc::new(ProfiledConfig {
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![],
        definition_bodies: vec![one.clone()],
    }));
    f.state.config = std::sync::Arc::new(config);
    let project = Uuid::new_v4();
    if aq_server::STATE_FORMAT_VERSION < 27 {
        assert!(
            definitions::retain_required(&f.state, project, &[one.artifact().clone()], NOW + 60000)
                .await
                .is_err(),
            "uninstalled identity guard cannot authorize new definition ownership"
        );
        // Draft027 explicitly applied: not ordered-chain qualification before026.
        sqlx::raw_sql(include_str!(
            "../../migrations/0027_native_definition_identities.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
    } else {
        let guard: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='admission_definition_identity')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(guard, "ordered027 identity guard missing");
    }

    assert!(
        definitions::retain_required(
            &f.state,
            project,
            &[definition("missing", "not configured").artifact().clone()],
            NOW + 60000
        )
        .await
        .is_err()
    );
    let retained =
        definitions::retain_required(&f.state, project, &[one.artifact().clone()], NOW + 60000)
            .await
            .unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].artifact, *one.artifact());
    assert_eq!(retained[0].kind, ArtifactKind::Definition);
    assert_eq!(
        definitions::retain_required(&f.state, project, &[one.artifact().clone()], NOW + 60000)
            .await
            .unwrap()[0]
            .retention_receipt,
        retained[0].retention_receipt
    );
    let two = definition(
        "synthetic-definition",
        "changed bytes with unchanged version",
    );
    let mut config = (*f.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled.revision += 1;
    profiled.definition_bodies = vec![two.clone()];
    config.profiled_execution = Some(std::sync::Arc::new(profiled));
    f.state.config = std::sync::Arc::new(config);
    assert!(
        definitions::retain_required(
            &f.state,
            Uuid::new_v4(),
            &[two.artifact().clone()],
            NOW + 60000
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn concurrent_changed_definitions_cannot_claim_same_identity_version() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    if aq_server::STATE_FORMAT_VERSION < 27 {
        sqlx::raw_sql(include_str!(
            "../../migrations/0027_native_definition_identities.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    let mut states = Vec::new();
    let mut refs = Vec::new();
    for body in [
        "synthetic first definition",
        "synthetic conflicting definition",
    ] {
        let d = definition("synthetic-concurrent-definition", body);
        refs.push(d.artifact().clone());
        let mut state = f.state.clone();
        let mut config = (*state.config).clone();
        config.profiled_execution = Some(std::sync::Arc::new(ProfiledConfig {
            revision: 1,
            qualified_runtimes: vec![],
            projects: vec![],
            definition_bodies: vec![d],
        }));
        state.config = std::sync::Arc::new(config);
        states.push(state);
    }
    let (a, b) = tokio::join!(
        definitions::retain_required(&states[0], Uuid::new_v4(), &refs[..1], NOW + 60000),
        definitions::retain_required(&states[1], Uuid::new_v4(), &refs[1..], NOW + 60000)
    );
    assert_ne!(
        a.is_ok(),
        b.is_ok(),
        "one immutable identity wins; the changed body refuses"
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM admission_artifacts WHERE descriptor#>>'{artifact,id}'='synthetic-concurrent-definition'").fetch_one(&pool).await.unwrap(),1);
}
