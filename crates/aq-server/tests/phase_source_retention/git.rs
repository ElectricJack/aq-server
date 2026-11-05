pub use crate::actual_git_owner::git;
use crate::{actual_git_owner::Owner, common::presence};
use aq_admission::{Digest, ObjectFormat, ObjectId, SourceSelection};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
pub struct Actual {
    pub central: Owner,
    pub worker: Owner,
    pub repo: Uuid,
    pub base: String,
    pub workspace: Value,
}
impl Actual {
    pub async fn prepare(ready: &mut presence::Case) -> Self {
        crate::actual_git_owner::verify_pins();
        let c = &mut ready.c;
        // Source is deliberately preserved on ANY unknown/failed process drain.
        // Even successful cases retain this small private state for audit; the
        // external whole-session proof runs after the test process has exited.
        c.env.env._dir.disable_cleanup(true);
        let parent = c.env.env._dir.path();
        let seed = parent.join("source-seed");
        std::fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-b", "main"]).await;
        std::fs::write(seed.join("source"), "base fixture bytes\n").unwrap();
        git(&seed, &["add", "."]).await;
        git(&seed, &["commit", "-m", "base"]).await;
        let base = git(&seed, &["rev-parse", "HEAD"]).await;
        let repo = c.env.frozen.project.binding.repository_id;
        let central = Owner::start(parent, "rc").await;
        central.call(repo,"repo.register",json!({"identity":"phase-retention-actual-fixture","mode":"central","remote_url":seed,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
        central.call(repo, "repo.sync", json!({})).await;
        let oid = json!({"algorithm":"sha1","hex":base});
        let work = central
            .call(repo, "workspace.create", json!({"base":oid}))
            .await;
        let checkpoint=central.call(repo,"workspace.checkpoint",json!({"workspace_id":work["workspace_id"],"expected_generation":1,"expected_head":oid,"quiescent":true,"mode":"head"})).await;
        let export=central.call(repo,"artifact.export",json!({"base":oid,"source":oid,"retention_handle":checkpoint["retention_handle"],"confirmed_prerequisites":[]})).await;
        let worker = Owner::start(parent, "rw").await;
        worker.call(repo,"repo.register",json!({"identity":"phase-retention-actual-fixture","mode":"worker","binding_generation":1})).await;
        let artifact: Uuid = serde_json::from_value(export["artifact_handle"].clone()).unwrap();
        let incoming = worker
            .root
            .join("artifacts/incoming")
            .join(artifact.to_string());
        std::fs::create_dir_all(&incoming).unwrap();
        for file in ["manifest.json", "objects.bundle"] {
            std::fs::copy(central.artifact(&export).join(file), incoming.join(file)).unwrap();
        }
        worker
            .call(repo, "artifact.import", json!({"artifact_handle":artifact}))
            .await;
        let workspace = worker
            .call(repo, "workspace.create", json!({"base":oid}))
            .await;
        let mut config = (*c.env.env.state.config).clone();
        let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
        profiled.revision += 1;
        profiled.projects[0].retention_handle =
            serde_json::from_value(checkpoint["retention_handle"].clone()).unwrap();
        profiled.projects[0].source = SourceSelection::Immutable {
            object: ObjectId {
                format: ObjectFormat::Sha1,
                hex: base.clone(),
            },
        };
        config.profiled_execution = Some(Arc::new(profiled));
        c.env.env.state.config = Arc::new(config);
        c.env.env.state.source_retention = Some(Arc::new(
            aq_server::source_retention::Runtime::new(
                aq_server::source_retention::SourceRetentionConfig {
                    keep_per_task: 2,
                    max_age: 30 * 86400,
                    git_root: central.root.clone(),
                    git_socket: central.socket.clone(),
                    staging_root: parent.join("retention-staging"),
                    max_staging_bytes: 8 * 1024 * 1024,
                    max_bundle_bytes: 1024 * 1024,
                    bulk_concurrency: 1,
                    incomplete_ttl_secs: 60,
                },
            )
            .unwrap(),
        ));
        Self {
            central,
            worker,
            repo,
            base,
            workspace,
        }
    }
    pub async fn export_changed(&self) -> (Value, Vec<u8>, Vec<u8>) {
        let checkpoint=self.worker.call(self.repo,"workspace.checkpoint",json!({"workspace_id":self.workspace["workspace_id"],"expected_generation":1,"expected_head":{"algorithm":"sha1","hex":self.base},"quiescent":true,"mode":"all","commit":{"name":"Fixture","email":"fixture@example.test","message":"actual changed checkpoint"}})).await;
        assert_ne!(checkpoint["source"]["hex"], self.base);
        let export=self.worker.call(self.repo,"artifact.export",json!({"base":{"algorithm":"sha1","hex":self.base},"source":checkpoint["source"],"retention_handle":checkpoint["retention_handle"],"confirmed_prerequisites":[]})).await;
        let path = self.worker.artifact(&export);
        let manifest = std::fs::read(path.join("manifest.json")).unwrap();
        let bundle = std::fs::read(path.join("objects.bundle")).unwrap();
        assert!(manifest.len() < 65536 && !bundle.is_empty() && bundle.len() < 1024 * 1024);
        assert_eq!(
            export["manifest"]["bundle_sha256"],
            Digest::of(&bundle).as_str()
        );
        (export, manifest, bundle)
    }
}
