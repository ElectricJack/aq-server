use super::*;

#[test]
fn missing_database_or_table_is_no_state_format() {
    use std::os::unix::process::ExitStatusExt;
    let ok = std::process::ExitStatus::from_raw(0);
    let bad = std::process::ExitStatus::from_raw(2 << 8);
    assert_eq!(parse_state_format(&ok, b"3\n", b"").unwrap(), Some(3));
    assert_eq!(parse_state_format(&ok, b"", b"").unwrap(), None);
    assert_eq!(
            parse_state_format(&bad, b"", b"psql: error: connection to server on socket \"/var/run/postgresql/.s.PGSQL.5432\" failed: FATAL:  database \"aq_server\" does not exist").unwrap(),
            None
        );
    assert_eq!(
        parse_state_format(
            &bad,
            b"",
            b"ERROR:  relation \"meta\" does not exist\nLINE 1: SELECT value FROM meta"
        )
        .unwrap(),
        None
    );
    assert!(parse_state_format(&bad, b"", b"service \"postgres\" is not running").is_err());
    assert!(parse_state_format(&ok, b"three", b"").is_err());
}

#[test]
fn compose_stderr_is_reduced_to_the_first_error_line() {
    let pg_restore = b"pg_restore: connecting to database for restore\n\
pg_restore: error: COPY failed for table \"users\": ERROR:  invalid input syntax for type uuid\n\
CONTEXT:  COPY users, line 3: \"42\talice@example.com\tsecret-hash\"\n";
    let got = sanitized_stderr(pg_restore).unwrap();
    assert_eq!(
        got,
        "pg_restore: error: COPY failed for table \"users\": ERROR:  invalid input syntax for type uuid"
    );
    // Lines with `=` (env files, key=value detail) and CONTEXT are never used.
    let env = b"failed to read /etc/agent-q/compose.env: line 2: error in PASSWORD=hunter2\n\
CONTEXT: error near row 7\n";
    assert_eq!(sanitized_stderr(env), None);
    // ERROR: wins over a later lowercase match, and the line is bounded.
    let long = format!("noise\nERROR:  {}\nanother error\n", "x".repeat(500));
    let got = sanitized_stderr(long.as_bytes()).unwrap();
    assert!(
        got.starts_with("ERROR:  xxx") && got.chars().count() == 200,
        "{got}"
    );
    // Control characters do not survive.
    assert_eq!(
        sanitized_stderr(b"psql: error: bad \x1b[31mthing\n").unwrap(),
        "psql: error: bad [31mthing"
    );
    assert_eq!(sanitized_stderr(b"nothing useful here\n"), None);
    assert_eq!(sanitized_stderr(b""), None);
}

#[tokio::test]
async fn compose_failures_do_not_leak_stderr_into_the_error() {
    let t = tempfile::tempdir().unwrap();
    let fake = t.path().join("docker");
    crate::test_support::write_stub(
        &fake,
        "#!/bin/sh\necho 'pg_restore: error: could not execute query: ERROR:  duplicate key' >&2\n\
echo 'DETAIL:  Key (email)=(alice@example.com) already exists.' >&2\n\
echo 'CONTEXT:  COPY users, line 1: \"alice@example.com\"' >&2\nexit 1\n",
    );
    let c = DockerCli {
        release_env: t.path().join("release.env"),
        ..cli().with_docker_binary(fake.clone())
    };
    let dump = t.path().join("a.dump");
    std::fs::write(&dump, b"PGDMP").unwrap();
    for e in [
        c.restore(&dump).await.unwrap_err().to_string(),
        c.stop_server().await.unwrap_err().to_string(),
        c.server_state_format().await.unwrap_err().to_string(),
        c.backup(&t.path().join("b.dump"))
            .await
            .unwrap_err()
            .to_string(),
    ] {
        assert!(!e.contains("alice"), "{e}");
        assert!(e.contains("ERROR:  duplicate key"), "{e}");
        assert!(e.contains("failed (exit status: 1)"), "{e}");
    }
    // An env-file error: nothing from stderr reaches the error.
    crate::test_support::write_stub(
        &fake,
        "#!/bin/sh\necho 'failed to read compose.env: line 2: unexpected character in \"SECRET=hunter2\"' >&2\nexit 15\n",
    );
    let e = c.stop_server().await.unwrap_err().to_string();
    assert_eq!(e, "docker compose stop failed (exit status: 15)");
    // Plain docker calls (registry errors) keep their stderr tail.
    let e = c.pull(IMAGE).await.unwrap_err().to_string();
    assert!(e.contains("SECRET"), "{e}");
}

#[test]
fn stderr_tail_is_bounded_and_keeps_the_end() {
    let mut long = vec![b'a'; 5000];
    long.extend_from_slice(b"the end\n");
    let t = stderr_tail(&long);
    assert!(t.len() <= 2048);
    assert!(t.ends_with("the end"));
    // A multi-byte character cut at the boundary is dropped, not mangled.
    let mut s = "é".repeat(2000).into_bytes();
    s.push(b'!');
    let t = stderr_tail(&s);
    assert!(t.len() <= 2048 && t.ends_with('!') && !t.contains('\u{FFFD}'));
}
