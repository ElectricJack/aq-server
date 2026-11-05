use aq_protocol::SUPPORTED_WORKER_PROTOCOLS;
use aq_server::STATE_FORMAT_VERSION;

fn dockerfile_arg(name: &str) -> u32 {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Dockerfile");
    let dockerfile = std::fs::read_to_string(path).expect("read server Dockerfile");
    let prefix = format!("ARG {name}=");
    let defaults: Vec<_> = dockerfile
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix))
        .collect();
    assert_eq!(defaults.len(), 1, "expected one default for {name}");
    defaults[0].parse().expect("numeric Dockerfile default")
}

#[test]
fn dockerfile_state_format_matches_source() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = std::process::Command::new("python3")
        .arg(root.join("scripts/migration_catalog.py"))
        .output()
        .expect("read image state format from the migration catalog");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap(),
        STATE_FORMAT_VERSION,
        "image labels and compiled migration catalog must agree"
    );
    let dockerfile = std::fs::read_to_string(root.join("Dockerfile")).unwrap();
    assert!(dockerfile.lines().any(|line| line == "ARG AQ_STATE_FORMAT"));
    assert!(!dockerfile.contains("ARG AQ_STATE_FORMAT="));
    assert!(dockerfile.contains("AQ_IMAGE_STATE_FORMAT=\"${AQ_STATE_FORMAT}\" cargo build"));
}

#[test]
fn dockerfile_protocol_max_matches_source() {
    assert_eq!(
        dockerfile_arg("AQ_PROTOCOL_MAX"),
        SUPPORTED_WORKER_PROTOCOLS.1,
        "bump Dockerfile's AQ_PROTOCOL_MAX default with SUPPORTED_WORKER_PROTOCOLS"
    );
}
