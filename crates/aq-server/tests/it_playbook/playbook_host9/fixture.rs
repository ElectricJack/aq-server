use crate::common::profiled::Fixture;
use aq_server::playbooks::{
    domain::GrantScope,
    service::{Namespace, RuntimeOwner, Service},
};
use aq_server::protected_storage::{KeyFile, Keyring, ProtectionConfig};
use serde_json::Value;
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, sync::Arc};
use uuid::Uuid;

pub(super) fn keys() -> Arc<Keyring> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("key");
    std::fs::write(&path, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    Arc::new(
        Keyring::load(&ProtectionConfig {
            active_key_id: "test".into(),
            keys: vec![KeyFile {
                id: "test".into(),
                file: path,
            }],
        })
        .unwrap(),
    )
}

pub(super) fn namespace(f: &Fixture, prefix: &str) -> Namespace {
    let lineage = &f.selection.lineage;
    Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: format!("{prefix}-{}", Uuid::new_v4()),
        workspace_id: lineage.workspace_id,
        project_id: lineage.project_id,
        owner_user_id: f.actor,
    }
}

pub(super) struct Grant {
    pub scope: GrantScope,
    pub id: Uuid,
    pub runtime: RuntimeOwner,
}

impl Grant {
    pub(super) async fn initialize(
        f: &Fixture,
        service: &Service,
        capabilities: BTreeSet<String>,
        ttl_ms: i64,
    ) -> Self {
        service.initialize(f.actor).await.unwrap();
        let now = (f.state.clock)().timestamp_millis();
        let scope = GrantScope {
            workspace_id: f.selection.lineage.workspace_id,
            project_id: f.selection.lineage.project_id,
            recipients: vec![],
            capabilities,
        };
        let id = Uuid::new_v4();
        service
            .grant(f.actor, id, scope.clone(), now + ttl_ms)
            .await
            .unwrap();
        let runtime = service.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
        Self { scope, id, runtime }
    }
}

pub(super) fn assert_pinned_files(pin: &Value, fingerprint: &str, directory: &str) {
    assert_eq!(pin["catalog_fingerprint"], fingerprint);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor")
        .join(directory);
    let files = pin["files"].as_array().unwrap();
    assert_eq!(files.len(), std::fs::read_dir(&root).unwrap().count());
    for file in files {
        let bytes = std::fs::read(root.join(file["path"].as_str().unwrap())).unwrap();
        assert_eq!(file["bytes"], bytes.len());
        assert_eq!(
            file["sha256"].as_str().unwrap(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
        );
    }
}
