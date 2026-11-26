use super::*;

fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        b.append_data(&mut h, name, *data).unwrap();
    }
    b.into_inner().unwrap()
}

#[test]
fn extracts_the_named_file_from_a_docker_cp_tar() {
    let t = tar_of(&[("manifest.json", b"{\"a\":1}")]);
    assert_eq!(extract_tar_file(&t, "manifest.json").unwrap(), b"{\"a\":1}");
    // Missing or wrongly named entries fail.
    assert!(extract_tar_file(&t, "manifest.sigstore.json").is_err());
    assert!(extract_tar_file(b"not a tar", "manifest.json").is_err());
    // A symlink is not a file.
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    b.append_link(&mut h, "manifest.json", "/etc/passwd")
        .unwrap();
    let link = b.into_inner().unwrap();
    assert!(extract_tar_file(&link, "manifest.json").is_err());
    // Oversized content is refused.
    let big = vec![b'x'; MAX_RELEASE_FILE + 1];
    let t = tar_of(&[("manifest.json", &big)]);
    assert!(extract_tar_file(&t, "manifest.json").is_err());
}

#[tokio::test]
async fn fetch_release_removes_the_container_even_on_error() {
    let t = tempfile::tempdir().unwrap();
    let fake = t.path().join("docker");
    let log = t.path().join("log");
    // Every call is logged; `create` and `cp` behave per case.
    let script = |create: &str, cp: &str| {
        format!(
            "#!/bin/sh\necho \"$*\" >> {log}\ncase \"$1\" in\n  create) {create} ;;\n  cp) {cp} ;;\nesac\n",
            log = log.display()
        )
    };
    let cases = [
        // cp fails after a good create.
        (
            script("echo 0123456789abcdef", "echo 'no such file' >&2; exit 1"),
            "docker cp",
            true,
        ),
        // create fails: the container may still exist; remove it by name.
        (
            script("echo 'conflict' >&2; exit 1", "exit 1"),
            "docker create",
            false,
        ),
    ];
    for (body, failing, reaches_cp) in cases {
        let _ = std::fs::remove_file(&log);
        crate::test_support::write_stub(&fake, &body);
        let c = cli().with_docker_binary(fake.clone());
        let e = c.fetch_release(ReleaseSelector::Channel).await.unwrap_err();
        assert!(e.to_string().contains(failing), "{e}");
        let calls = std::fs::read_to_string(&log).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        assert_eq!(
            calls[0],
            "pull --quiet -- ghcr.io/electricjack/agent-q-release:stable"
        );
        let name = calls[1]
            .strip_prefix("create --name ")
            .and_then(|r| r.strip_suffix(" ghcr.io/electricjack/agent-q-release:stable none"))
            .unwrap_or_else(|| panic!("{calls:?}"));
        assert!(name.starts_with("aq-release-fetch-"), "{calls:?}");
        let mut want = vec![calls[0].to_string(), calls[1].to_string()];
        if reaches_cp {
            want.push(format!("cp {name}:/manifest.json -"));
        }
        want.push(format!("rm -f {name}"));
        assert_eq!(calls, want);
    }
}
