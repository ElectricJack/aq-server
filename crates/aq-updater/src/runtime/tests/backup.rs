use super::*;

#[tokio::test]
async fn failures_name_the_command_and_keep_stderr() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let fake = t.path().join("docker");
    crate::test_support::write_stub(&fake, "#!/bin/sh\necho \"boom: $1\" >&2\nexit 7\n");
    let c = cli().with_docker_binary(fake.clone());
    let e = c.pull(IMAGE).await.unwrap_err();
    let RuntimeError::Failed(m) = e else {
        panic!("{e:?}")
    };
    assert!(m.contains("docker pull") && m.contains("boom: pull"), "{m}");
    let e = c.pull("nginx:latest").await.unwrap_err();
    assert!(matches!(e, RuntimeError::Failed(_)), "{e:?}");

    // backup streams stdout into a new 0600 file and fsyncs it.
    crate::test_support::write_stub(&fake, "#!/bin/sh\nprintf 'PGDMP-data'\n");
    let dest = t.path().join("b.dump");
    let c = DockerCli {
        release_env: t.path().join("release.env"),
        ..cli().with_docker_binary(fake.clone())
    };
    assert_eq!(c.backup(&dest).await.unwrap(), 10);
    assert_eq!(std::fs::read(&dest).unwrap(), b"PGDMP-data");
    let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    // ...and never touches an existing file.
    assert!(c.backup(&dest).await.is_err());
    assert_eq!(std::fs::read(&dest).unwrap(), b"PGDMP-data");
    // A failing dump removes the partial file it created.
    crate::test_support::write_stub(
        &fake,
        "#!/bin/sh\nprintf 'partial'\necho 'pg_dump: error: disk full' >&2\nexit 1\n",
    );
    let dest2 = t.path().join("c.dump");
    let e = c.backup(&dest2).await.unwrap_err();
    assert!(
        e.to_string().contains("pg_dump") && e.to_string().contains("disk full"),
        "{e}"
    );
    assert!(!dest2.exists());
}
