use super::{Env, api, protected_factory, pure_artifact, setup, source_url};
use aq_playbook_host::{Limits, model::Artifact};
use aq_server::{build_admin_router_with_playbooks, playbooks::driver::Driver};
use axum::Router;
use serde_json::{Value, json};
use std::sync::Arc;

struct OwnerApi {
    app: Router,
    driver: Arc<Driver>,
    env: Env,
}

impl OwnerApi {
    async fn start() -> Self {
        let env = setup().await;
        let driver = Driver::start(
            env.service.clone(),
            Limits::default(),
            protected_factory(source_url(&env)),
        )
        .await
        .unwrap();
        let driver = Arc::new(driver);
        let app = build_admin_router_with_playbooks(env.f.state.clone(), Some(driver.clone()));
        Self { app, driver, env }
    }

    async fn post(&self, path: &str, body: Value, expected_status: u16) -> Value {
        let path = format!("/api/playbooks/{path}");
        let (status, response) = api(self.app.clone(), "POST", &path, true, body).await;
        assert_eq!(status.as_u16(), expected_status, "{path}: {response}");
        response
    }

    async fn install(&self, id: &str, artifact: &Artifact) {
        let checked = self
            .post("artifacts/validate", json!({"artifact": artifact}), 200)
            .await;
        assert_eq!(checked["digest"], artifact.digest);
        let receipt = self
            .post("artifacts", json!({"op_id":id,"artifact":artifact}), 200)
            .await;
        assert_eq!(receipt["artifact"], artifact.digest);
        assert_eq!(receipt["kind"], "install");
    }

    async fn activate(&self, id: &str, artifact: &Artifact) {
        let body = json!({"op_id":id, "artifact":artifact.digest, "grant_id":self.env.grant, "grant_revision":1});
        let receipt = self.post("bindings/pure/activate", body, 200).await;
        assert_eq!(receipt["artifact"], artifact.digest);
        assert_eq!(receipt["kind"], "activate");
    }
}

#[tokio::test]
async fn granted_binding_routes_replay_and_restore_the_prior_artifact() {
    let owner = OwnerApi::start().await;
    let first = pure_artifact();
    let mut content = first.content.clone();
    content.compiler.push_str(" revision two");
    let second = Artifact::seal(content).unwrap();
    owner.install("install-first", &first).await;
    owner.install("install-second", &second).await;
    owner.activate("activate-first", &first).await;
    owner.activate("activate-second", &second).await;

    let rollback = json!({"op_id":"restore-first", "grant_id":owner.env.grant, "grant_revision":1});
    let restored = owner
        .post("bindings/pure/rollback", rollback.clone(), 200)
        .await;
    assert_eq!(restored["artifact"], first.digest);
    assert_eq!(restored["previous"], second.digest);
    assert_eq!(restored["kind"], "rollback");
    let replay = owner.post("bindings/pure/rollback", rollback, 200).await;
    assert_eq!(replay["duplicate"], true);
    assert_eq!(replay["artifact"], first.digest);
    owner.driver.stop().await.unwrap();
}

#[tokio::test]
async fn coding_default_route_retains_strict_input_and_policy_refusals() {
    let owner = OwnerApi::start().await;
    let mut request = json!({
        "op_id":"coding", "grant_id":owner.env.grant, "grant_revision":1,
        "policy":{"implementer_family":"one","reviewer_family":"two","checks":[],"max_repairs":99}
    });
    let invalid = owner.post("coding/default", request.clone(), 400).await;
    assert_eq!(invalid["disposition"], "refused");
    request["unexpected"] = true.into();
    owner.post("coding/default", request, 422).await;
    owner.driver.stop().await.unwrap();
}
