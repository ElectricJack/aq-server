use super::*;
fn range(offset: u64, limit: u32) -> ChunkRange {
    ChunkRange { offset, limit }
}
use crate::profiled_execution::source_exports::synthetic_reserved_export;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    artifact: PathBuf,
    token: ReservedExport,
    response: Value,
    retention: Value,
}
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("git");
    let request = Request {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        project_revision: 1,
        repository_id: Uuid::new_v4(),
        logical_identity: "source-fixture".into(),
        binding_generation: 1,
        base: aq_worker_protocol::ObjectId {
            algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
            hex: "a".repeat(40),
        },
        source_retention_handle: Uuid::new_v4(),
        source_observation: aq_admission::ArtifactRef {
            id: aq_admission::Text::new("source-observation").unwrap(),
            version: aq_admission::Text::new("1").unwrap(),
            digest: Digest::of(b"observation"),
        },
    };
    let artifact = root
        .join("artifacts")
        .join(request.operation_id.to_string());
    fs::create_dir_all(&artifact).unwrap();
    let producer = Uuid::new_v4();
    fs::write(
        root.join("format.json"),
        json!({"format_version":1,"installation_id":producer}).to_string(),
    )
    .unwrap();
    let bundle = b"synthetic bundle bytes, not a real Git export";
    fs::write(artifact.join("objects.bundle"), bundle).unwrap();
    let manifest = json!({"format_version":1,"repository_id":request.repository_id,"repository_identity":request.logical_identity,"binding_generation":1,"transfer_id":request.operation_id,"object_format":"sha1","base":request.base,"source":request.base,"prerequisites":[],"bundle_size":bundle.len(),"bundle_sha256":Digest::of(bundle),"producer_installation_id":producer,"reference_only":false,"caller_correlation":correlation(&request)});
    fs::write(
        artifact.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let response = json!({"artifact_handle":request.operation_id,"retention_handle":request.operation_id,"manifest":manifest});
    let retention = json!({"version":1,"id":request.operation_id,"repository_id":request.repository_id,"binding_generation":1,"generation":1,"resource":"transfer","refs":[{"name":format!("refs/aq-git/transfers/{}",request.operation_id),"oid":request.base.hex}],"state":"active","dependencies":[],"release_operation":null,"release_reason":null});
    Fixture {
        _dir: dir,
        root,
        artifact,
        token: synthetic_reserved_export(request, 2_097_152).unwrap(),
        response,
        retention,
    }
}
fn fact(f: &Fixture) -> VerifiedExport {
    let v = read_files(&f.root, f.token.request(), &f.response, 4096).unwrap();
    VerifiedExport {
        schema_version: 1,
        request_digest: f.token.request_digest().clone(),
        export_operation_id: f.token.request().operation_id,
        manifest_json: v.0,
        manifest_digest: v.1,
        bundle_bytes: v.2,
        bundle_digest: v.3,
        producer_installation_id: v.4,
        export_response: f.response.clone(),
        retention_inspection: f.retention.clone(),
        observed_at_ms: 1000,
        expires_at_ms: 31000,
    }
}
#[test]
fn historical_fact_is_canonical_bound_and_cannot_refresh_itself() {
    let f = fixture();
    let v = fact(&f);
    let bytes = v.canonical_bytes().unwrap();
    let decoded = VerifiedExport::decode_stored(&f.token, &bytes).unwrap();
    assert_eq!(decoded.observed_at_ms(), 1000);
    assert_eq!(decoded.expires_at_ms(), 31000);
    let mut alternate = bytes.clone();
    alternate.push(b' ');
    assert!(VerifiedExport::decode_stored(&f.token, &alternate).is_err());
    let other = fixture();
    assert!(VerifiedExport::decode_stored(&other.token, &bytes).is_err());
    for field in ["request_digest", "manifest_digest", "bundle_digest"] {
        let mut changed = serde_json::to_value(&v).unwrap();
        changed[field] = json!(Digest::of(b"different"));
        assert!(VerifiedExport::decode_stored(&f.token, &canonical(&changed).unwrap()).is_err());
    }
    for expiry in [1000, 31001, u64::MAX] {
        let mut changed = v.clone();
        changed.expires_at_ms = expiry;
        assert!(changed.validate_for(&f.token).is_err());
    }
}
#[test]
fn verification_refuses_corrupt_size_digest_manifest_and_installation() {
    for case in 0..6 {
        let mut f = fixture();
        match case {
            0 => fs::write(f.artifact.join("objects.bundle"), b"wrong").unwrap(),
            1 => { let path = f.artifact.join("objects.bundle"); let mut bytes = fs::read(&path).unwrap(); bytes[0] ^= 1; fs::write(path, bytes).unwrap(); }
            2 => f.response["manifest"]["binding_generation"] = json!(2),
            3 => fs::write(f.root.join("format.json"), json!({"format_version":1,"installation_id":Uuid::new_v4()}).to_string()).unwrap(),
            4 => fs::write(f.root.join("format.json"), json!({"format_version":2,"installation_id":f.response["manifest"]["producer_installation_id"]}).to_string()).unwrap(),
            _ => fs::write(f.artifact.join("manifest.json"), vec![b'x';MAX_MANIFEST+1]).unwrap(),
        }
        assert!(
            read_files(&f.root, f.token.request(), &f.response, 4096).is_err(),
            "case {case}"
        );
    }
    let f = fixture();
    assert!(read_files(&f.root, f.token.request(), &f.response, 1).is_err());
}
#[test]
fn nofollow_applies_to_every_artifact_component_and_special_files_do_not_block() {
    for member in [
        "",
        "artifacts",
        "artifact",
        "manifest.json",
        "objects.bundle",
        "format.json",
    ] {
        let f = fixture();
        let path = match member {
            "" => f.root.clone(),
            "artifacts" => f.root.join(member),
            "artifact" => f.artifact.clone(),
            "format.json" => f.root.join(member),
            _ => f.artifact.join(member),
        };
        let original = path.with_extension("original");
        fs::rename(&path, &original).unwrap();
        symlink(&original, &path).unwrap();
        assert!(
            read_files(&f.root, f.token.request(), &f.response, 4096).is_err(),
            "{member}"
        );
    }
    let f = fixture();
    let bundle = f.artifact.join("objects.bundle");
    fs::remove_file(&bundle).unwrap();
    nix::unistd::mkfifo(&bundle, nix::sys::stat::Mode::S_IRUSR).unwrap();
    assert!(read_files(&f.root, f.token.request(), &f.response, 4096).is_err());
    let f = fixture();
    fs::set_permissions(
        f.artifact.join("objects.bundle"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    assert!(read_files(&f.root, f.token.request(), &f.response, 4096).is_err());
}
#[test]
fn chunk_reads_are_bounded_exact_and_refuse_changed_members() {
    let f = fixture();
    let v = fact(&f);
    let expected = fs::read(f.artifact.join("objects.bundle")).unwrap();
    let mut actual = Vec::new();
    for offset in (0..expected.len()).step_by(7) {
        actual
            .extend(read_chunk_file(&f.root, &v, Member::Bundle, range(offset as u64, 7)).unwrap());
    }
    assert_eq!(actual, expected);
    assert!(
        read_chunk_file(&f.root, &v, Member::Bundle, range(expected.len() as u64, 7))
            .unwrap()
            .is_empty()
    );
    assert!(
        read_chunk_file(
            &f.root,
            &v,
            Member::Bundle,
            range(expected.len() as u64 + 1, 7)
        )
        .is_err()
    );
    assert!(read_chunk_file(&f.root, &v, Member::Bundle, range(u64::MAX, 7)).is_err());
    for limit in [0, 1048577, u32::MAX] {
        assert!(read_chunk_file(&f.root, &v, Member::Bundle, range(0, limit)).is_err());
    }
    assert_eq!(
        read_chunk_file(&f.root, &v, Member::Manifest, range(0, 1048576)).unwrap(),
        v.manifest_bytes()
    );
    fs::write(f.artifact.join("manifest.json"), b"{}").unwrap();
    assert!(read_chunk_file(&f.root, &v, Member::Bundle, range(0, 7)).is_err());
    let f = fixture();
    let v = fact(&f);
    let path = f.artifact.join("objects.bundle");
    let saved = path.with_extension("saved");
    fs::rename(&path, &saved).unwrap();
    symlink(&saved, &path).unwrap();
    assert!(read_chunk_file(&f.root, &v, Member::Bundle, range(0, 7)).is_err());
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"short").unwrap();
    assert!(read_chunk_file(&f.root, &v, Member::Bundle, range(0, 7)).is_err());
}
#[test]
fn retention_requires_exact_active_transfer_and_finite_observation() {
    let f = fixture();
    let observation = RetentionObservation {
        request_digest: f.token.request_digest().clone(),
        export_operation_id: f.token.request().operation_id,
        inspection: f.retention.clone(),
        observed_at_ms: 1000,
        expires_at_ms: 31000,
    };
    observation.validate_for(&f.token).unwrap();
    for (field, value) in [
        ("state", json!("releasing")),
        ("release_operation", json!(Uuid::new_v4())),
        ("resource", json!("workspace")),
        ("refs", json!([])),
        ("generation", json!(0)),
        ("repository_id", json!(Uuid::new_v4())),
        ("binding_generation", json!(2)),
    ] {
        let mut changed = observation.clone();
        changed.inspection[field] = value;
        assert!(changed.validate_for(&f.token).is_err(), "{field}");
    }
    let mut changed = observation.clone();
    changed.expires_at_ms = 31001;
    assert!(changed.validate_for(&f.token).is_err());
    assert!(observation.validate_for(&fixture().token).is_err());
}
fn runtime_for(f: &Fixture) -> Runtime {
    Runtime::new(super::super::SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: f._dir.path().join("staging"),
        git_root: f.root.clone(),
        git_socket: f._dir.path().join("git.sock"),
        max_staging_bytes: 4_194_304,
        max_bundle_bytes: 4096,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 60,
        git_deadline_ms: 30_000,
    })
    .unwrap()
}
#[tokio::test]
async fn socket_replies_are_bounded_and_correlated_without_inventing_success() {
    for case in 0..5 {
        let f = fixture();
        let rt = runtime_for(&f);
        let listener = tokio::net::UnixListener::bind(&rt.config.git_socket).unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let r: Value = serde_json::from_str(&line).unwrap();
            let mut reply = json!({"protocol_version":1,"request_id":r["request_id"],"operation_id":r["operation_id"],"ok":true,"result":{"done":true}});
            match case {
                1 => reply["request_id"] = json!("different"),
                2 => reply["operation_id"] = json!(Uuid::new_v4()),
                3 => reply["result"] = json!("x".repeat(1024)),
                4 => reply["ok"] = json!(false),
                _ => {}
            }
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            reader.get_mut().write_all(&bytes).await.unwrap();
        });
        let result = exchange(
            &rt,
            &json!({"request_id":"request","operation_id":Uuid::new_v4()}),
            512,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.is_ok(), case == 0, "case {case}");
        task.await.unwrap();
    }
}
