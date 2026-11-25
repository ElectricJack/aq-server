use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

const SPOOL: &str = "/var/lib/agent-q/updater/spool";

fn body(image: &str) -> String {
    format!("AQ_SERVER_IMAGE={image}\nAQ_UPDATER_SPOOL={SPOOL}\n")
}

#[test]
fn release_env_written_atomically_with_mode_0640() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("release.env");
    let spool = Path::new(SPOOL);
    std::fs::write(t.path().join("release.env.tmp"), "stale").unwrap();
    write_release_env(&path, IMAGE, spool).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body(IMAGE));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o640, "{mode:o}");
    let names: Vec<_> = std::fs::read_dir(t.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![OsString::from("release.env")]);
    // Only digest refs are written: no tags, no injected lines.
    for bad in ["nginx:latest", &format!("{IMAGE}\nAQ_VAULT_DIR=/"), ""] {
        assert!(write_release_env(&path, bad, spool).is_err(), "{bad:?}");
    }
    // Only spool paths Compose reads back verbatim.
    for bad in [
        "rel/spool",
        "/a b",
        "/a$HOME",
        "/a#b",
        "/a\nAQ_VAULT_DIR=/",
        "/a\"b",
    ] {
        let e = write_release_env(&path, IMAGE, Path::new(bad)).unwrap_err();
        assert!(e.to_string().contains("spool path"), "{bad:?}: {e}");
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body(IMAGE));
}

#[test]
fn no_release_placeholder_is_a_digest_ref_on_a_reserved_host() {
    assert!(aq_release::is_digest_ref(NO_RELEASE_IMAGE));
    assert!(NO_RELEASE_IMAGE.starts_with("no-release-installed.invalid/"));
    assert!(NO_RELEASE_IMAGE.ends_with(&format!("@sha256:{}", "0".repeat(64))));
}

#[test]
fn ensure_release_env_bootstraps_a_fresh_host() {
    let t = tempfile::tempdir().unwrap();
    let spool = Path::new(SPOOL);
    // No release installed: the placeholder image and the spool, so
    // compose.yaml interpolates for postgres, psql and pg_dump.
    let fresh = t.path().join("fresh.env");
    ensure_release_env(&fresh, None, spool).unwrap();
    assert_eq!(
        std::fs::read_to_string(&fresh).unwrap(),
        body(NO_RELEASE_IMAGE)
    );
    let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o640, "{mode:o}");
    // A placeholder gives way to the journal's active release.
    ensure_release_env(&fresh, Some(IMAGE), spool).unwrap();
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), body(IMAGE));
    // Missing, with an active release.
    let active = t.path().join("active.env");
    ensure_release_env(&active, Some(IMAGE), spool).unwrap();
    assert_eq!(std::fs::read_to_string(&active).unwrap(), body(IMAGE));
    // An empty file (an older updater wrote these) is completed.
    let empty = t.path().join("empty.env");
    std::fs::write(&empty, "").unwrap();
    ensure_release_env(&empty, None, spool).unwrap();
    assert_eq!(
        std::fs::read_to_string(&empty).unwrap(),
        body(NO_RELEASE_IMAGE)
    );
}

#[test]
fn ensure_release_env_keeps_the_image_and_fixes_the_spool() {
    use std::os::unix::fs::MetadataExt;
    let t = tempfile::tempdir().unwrap();
    let spool = Path::new(SPOOL);
    let path = t.path().join("release.env");
    const OTHER: &str = "ghcr.io/electricjack/agent-q-server@sha256:0000000000000000000000000000000000000000000000000000000000000200";
    // The image last started is kept over the journal's; a missing or
    // different spool line is replaced.
    std::fs::write(&path, format!("AQ_SERVER_IMAGE={OTHER}\n")).unwrap();
    ensure_release_env(&path, Some(IMAGE), spool).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body(OTHER));
    std::fs::write(
        &path,
        format!("AQ_SERVER_IMAGE={OTHER}\nAQ_UPDATER_SPOOL=/elsewhere\n"),
    )
    .unwrap();
    ensure_release_env(&path, None, spool).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body(OTHER));
    // A complete file is left alone (same inode: not rewritten).
    let ino = std::fs::metadata(&path).unwrap().ino();
    ensure_release_env(&path, Some(IMAGE), spool).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), ino);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body(OTHER));
    // An image line that is not digest-pinned is not trusted.
    std::fs::write(&path, "AQ_SERVER_IMAGE=nginx:latest\n").unwrap();
    ensure_release_env(&path, Some(IMAGE), spool).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), body(IMAGE));
}

#[test]
fn docker_cli_writes_the_spool_of_its_state_dir() {
    let t = tempfile::tempdir().unwrap();
    let c = DockerCli::new(
        cli().compose.clone(),
        t.path(),
        "ghcr.io/electricjack/agent-q-release".into(),
        "stable".into(),
    );
    c.ensure_release_env(None).unwrap();
    assert_eq!(
        std::fs::read_to_string(c.release_env_path()).unwrap(),
        format!(
            "AQ_SERVER_IMAGE={NO_RELEASE_IMAGE}\nAQ_UPDATER_SPOOL={}\n",
            t.path().join("spool").display()
        )
    );
}

#[test]
fn release_env_path_is_under_the_state_dir() {
    let c = cli();
    assert_eq!(
        c.release_env_path(),
        PathBuf::from("/var/lib/agent-q/updater/release.env")
    );
}
