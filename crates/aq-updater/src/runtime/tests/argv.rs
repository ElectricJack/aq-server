use super::*;

fn strs(v: &[OsString]) -> Vec<&str> {
    v.iter().map(|a| a.to_str().unwrap()).collect()
}

const PREFIX: [&str; 13] = [
    "compose",
    "--project-name",
    "agent-q",
    "--project-directory",
    "/opt/agent-q",
    "-f",
    "/opt/agent-q/compose.yaml",
    "-f",
    "/opt/agent-q/compose.prod.yaml",
    "--env-file",
    "/etc/agent-q/compose.env",
    "--env-file",
    "/var/lib/agent-q/updater/release.env",
];

fn with_prefix(rest: &[&'static str]) -> Vec<&'static str> {
    PREFIX.iter().chain(rest).copied().collect()
}

#[test]
fn compose_args_are_fixed_vectors() {
    let c = cli();
    assert_eq!(
        c.release_env_path(),
        Path::new("/var/lib/agent-q/updater/release.env")
    );
    assert_service_argv(&c);
    assert_database_argv(&c);
    assert_restore_argv(&c);
    assert_no_image_in_argv(&c);
    assert_release_env_without_extras();
}

fn assert_service_argv(c: &DockerCli) {
    assert_eq!(
        strs(&c.start_server_argv()),
        with_prefix(&["up", "-d", "--force-recreate", "server"])
    );
    assert_eq!(
        strs(&c.provision_database_argv()),
        with_prefix(&["run", "--rm", "-T", "server-db-init"])
    );
    assert_eq!(
        strs(&c.stop_server_argv()),
        with_prefix(&["stop", "server"])
    );
    assert_eq!(
        strs(&c.server_container_argv()),
        with_prefix(&["ps", "-q", "server"])
    );
    assert_eq!(
        strs(&c.readiness_argv()),
        with_prefix(&["exec", "-T", "server", "aq-server", "readiness"])
    );
}

fn assert_database_argv(c: &DockerCli) {
    assert_eq!(
        strs(&c.backup_argv()),
        with_prefix(&[
            "exec",
            "-T",
            "postgres",
            "pg_dump",
            "-U",
            "agent_q",
            "-Fc",
            "aq_server"
        ])
    );
    assert_eq!(
        strs(&c.state_format_argv()),
        with_prefix(&[
            "exec",
            "-T",
            "postgres",
            "psql",
            "-U",
            "agent_q",
            "-d",
            "aq_server",
            "-tAc",
            "SELECT value FROM meta WHERE key = 'state_format_version'",
        ])
    );
}

fn assert_restore_argv(c: &DockerCli) {
    let [drop, create, restore] = c.restore_argvs();
    assert_eq!(
        strs(&drop),
        with_prefix(&[
            "exec",
            "-T",
            "postgres",
            "psql",
            "-U",
            "agent_q",
            "-d",
            "postgres",
            "-c",
            "DROP DATABASE IF EXISTS aq_server WITH (FORCE)",
        ])
    );
    assert_eq!(
        strs(&create),
        with_prefix(&[
            "exec",
            "-T",
            "postgres",
            "psql",
            "-U",
            "agent_q",
            "-d",
            "postgres",
            "-c",
            "CREATE DATABASE aq_server OWNER aq_server",
        ])
    );
    assert_eq!(
        strs(&restore),
        with_prefix(&[
            "exec",
            "-T",
            "postgres",
            "pg_restore",
            "-U",
            "agent_q",
            "-d",
            "aq_server"
        ])
    );
}

fn assert_no_image_in_argv(c: &DockerCli) {
    // The server image never appears as an argument of a Compose call.
    for argv in [
        c.start_server_argv(),
        c.provision_database_argv(),
        c.stop_server_argv(),
        c.readiness_argv(),
        c.backup_argv(),
        c.state_format_argv(),
    ] {
        assert!(
            strs(&argv).iter().all(|a| !a.contains("sha256")),
            "{argv:?}"
        );
    }
}

fn assert_release_env_without_extras() {
    // Without extra env files only the release env is passed.
    let mut bare = cli();
    bare.compose.env_files.clear();
    let argv = bare.stop_server_argv();
    let argv = strs(&argv);
    assert_eq!(argv.iter().filter(|a| **a == "--env-file").count(), 1);
}

#[test]
fn docker_argvs_take_only_validated_refs() {
    let c = cli();
    assert_image_refs(&c);
    assert_release_tags(&c);
    assert_fetch_container_argv(&c);
    assert_repository_argv(&c);
    assert_health_argv(&c);
}

fn assert_image_refs(c: &DockerCli) {
    assert_eq!(
        strs(&c.pull_argv(IMAGE).unwrap()),
        ["pull", "--quiet", "--", IMAGE]
    );
    // A local registry with a port is a valid digest ref.
    let local = IMAGE.replace("ghcr.io/electricjack", "127.0.0.1:5000");
    assert_eq!(
        strs(&c.pull_argv(&local).unwrap()),
        ["pull", "--quiet", "--", local.as_str()]
    );
    assert_eq!(
        strs(&c.labels_argv(IMAGE).unwrap()),
        [
            "image",
            "inspect",
            "--format",
            "{{json .Config.Labels}}",
            "--",
            IMAGE
        ]
    );
    for bad in [
        "ghcr.io/electricjack/agent-q-server:latest",
        "--help",
        "ghcr.io/x@sha256:00",
        &format!("{IMAGE}\nAQ_OTHER=1"),
    ] {
        assert!(c.pull_argv(bad).is_err(), "{bad}");
        assert!(c.labels_argv(bad).is_err(), "{bad}");
    }
}

fn assert_release_tags(c: &DockerCli) {
    let v = ReleaseSelector::Version("1.2.3".parse().unwrap());
    assert_eq!(
        c.release_ref(&v).unwrap(),
        "ghcr.io/electricjack/agent-q-release:1.2.3"
    );
    assert_eq!(
        c.release_ref(&ReleaseSelector::Channel).unwrap(),
        "ghcr.io/electricjack/agent-q-release:stable"
    );
    // Build metadata is not a valid tag character.
    let v = ReleaseSelector::Version("1.2.3+x".parse().unwrap());
    assert!(c.release_ref(&v).is_err());
    let mut odd = cli();
    odd.channel = "-bad".into();
    assert!(odd.release_ref(&ReleaseSelector::Channel).is_err());
}

fn assert_fetch_container_argv(c: &DockerCli) {
    let name = fetch_container_name();
    assert!(name.starts_with("aq-release-fetch-"), "{name}");
    assert_ne!(name, fetch_container_name());
    assert_eq!(
        strs(
            &c.release_create_argv(&name, "ghcr.io/electricjack/agent-q-release:1.2.3")
                .unwrap()
        ),
        [
            "create",
            "--name",
            name.as_str(),
            "ghcr.io/electricjack/agent-q-release:1.2.3",
            "none"
        ]
    );
    assert_eq!(
        strs(&c.cp_argv(&name, "/manifest.json").unwrap()),
        ["cp", format!("{name}:/manifest.json").as_str(), "-"]
    );
    assert_eq!(
        strs(&c.rm_argv(&name).unwrap()),
        ["rm", "-f", name.as_str()]
    );
    for bad in ["0123456789ab", "aq-release-fetch-x", "--rm", ""] {
        assert!(c.release_create_argv(bad, "r:1").is_err(), "{bad}");
        assert!(c.cp_argv(bad, "/manifest.json").is_err(), "{bad}");
        assert!(c.rm_argv(bad).is_err(), "{bad}");
    }
}

fn assert_repository_argv(c: &DockerCli) {
    assert_eq!(
        strs(
            &c.image_list_argv("ghcr.io/electricjack/agent-q-server")
                .unwrap()
        ),
        [
            "image",
            "ls",
            "--digests",
            "--no-trunc",
            "--format",
            "{{.Repository}}@{{.Digest}}",
            "--",
            "ghcr.io/electricjack/agent-q-server"
        ]
    );
    assert_eq!(
        strs(&c.image_rm_argv(IMAGE).unwrap()),
        ["image", "rm", "--", IMAGE]
    );
    for bad in [
        "",
        "--help",
        "ghcr.io/electricjack/agent-q-server:latest",
        IMAGE,
    ] {
        assert!(c.image_list_argv(bad).is_err(), "{bad}");
    }
    assert!(
        c.image_rm_argv("ghcr.io/electricjack/agent-q-server:latest")
            .is_err()
    );
    let listed = format!("{IMAGE}\nghcr.io/electricjack/agent-q-server@<none>\n{IMAGE}\n");
    assert_eq!(parse_image_list(listed.as_bytes()), [IMAGE]);
}

fn assert_health_argv(c: &DockerCli) {
    let id = "0123456789ab";
    assert_eq!(
        strs(&c.health_argv(id).unwrap()),
        ["inspect", "--format", "{{.State.Health.Status}}", id]
    );
    for bad in [
        "",
        "abc",
        "0123456789AB",
        "0123456789ab:/etc/passwd",
        "--rm",
    ] {
        assert!(c.cp_argv(bad, "/manifest.json").is_err(), "{bad}");
        assert!(c.rm_argv(bad).is_err(), "{bad}");
        assert!(c.health_argv(bad).is_err(), "{bad}");
    }
}
