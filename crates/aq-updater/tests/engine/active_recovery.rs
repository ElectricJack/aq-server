use super::*;

/// How history looks when recovery finds the journal at `Active`.
#[derive(Debug, Clone, Copy)]
enum BadHistory {
    /// The finished operation's line carries an outcome this updater
    /// does not know.
    Unknown,
    /// history.jsonl cannot be read (nor appended to).
    Unreadable,
}

fn spoil_history(h: &H, how: BadHistory, request_id: Uuid) {
    let path = h.state_dir.join("history.jsonl");
    match how {
        BadHistory::Unknown => {
            let text = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = text.lines().map(String::from).collect();
            let mut last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
            assert_eq!(last["request_id"], serde_json::json!(request_id));
            last["outcome"] = "some_future_outcome".into();
            *lines.last_mut().unwrap() = last.to_string();
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        }
        BadHistory::Unreadable => {
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
    }
}

/// Crash in the final leave() of an install of B (format 2, so its
/// in-flight backup holds format-1 data) or of restore_previous back to A
/// (whose in-flight backup holds B's format-2 data). The journal is at
/// `Active` either way. Returns the harness, the expected active release
/// and the in-flight request id.
async fn crash_at_active(restore_mode: bool) -> (H, ReleaseManifest, Uuid) {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 2, &[1, 2]);
    h.publish(&b);
    if restore_mode {
        assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
        h.fake.clear_log();
        h.fake.crash("leave", 0);
        assert!(e.restore_previous(true).await.is_err());
    } else {
        h.fake.crash("leave", 0);
        assert!(e.install(req(B)).await.is_err());
    }
    drop(e);
    let cur = Journal::read(&h.state_dir).unwrap().current.unwrap();
    assert_eq!(cur.step, Step::Active);
    assert!(cur.needs_restore_on_rollback && cur.backup.is_some());
    let expected = if restore_mode { a } else { b };
    assert!(h.fake.maintenance_on());
    (h, expected, cur.request_id)
}

#[tokio::test]
async fn active_step_finishes_from_the_journal_whatever_history_says() {
    for restore_mode in [false, true] {
        for how in [BadHistory::Unknown, BadHistory::Unreadable] {
            let label = format!("restore_mode {restore_mode}, {how:?}");
            let (h, expected, id) = crash_at_active(restore_mode).await;
            spoil_history(&h, how, id);
            h.fake.clear_log();

            let mut e = h.engine();
            let out = e.recover().await.unwrap().expect("an outcome");
            assert_eq!(out.outcome, Outcome::Succeeded, "{label}: {}", out.detail);
            assert_eq!(out.request_id, Some(id), "{label}");
            assert_eq!(h.fake.count("restore"), 0, "{label}: {:?}", h.fake.log());
            assert_eq!(h.fake.count("start_server"), 0, "{label}");
            assert_eq!(
                active_version(&e),
                Some(expected.version.clone()),
                "{label}"
            );
            assert_eq!(
                h.fake.running().as_ref(),
                Some(&expected.images.server),
                "{label}"
            );
            assert_eq!(h.fake.db_format(), Some(expected.state_format), "{label}");
            assert!(!h.fake.maintenance_on(), "{label}");
            assert!(e.journal_state().current.is_none(), "{label}");
            if let BadHistory::Unknown = how {
                assert_eq!(h.history()[0], out, "{label}: outcome recorded");
            }
        }
    }
}

#[tokio::test]
async fn retry_active_never_restores_at_active_step() {
    for restore_mode in [false, true] {
        let label = format!("restore_mode {restore_mode}");
        let (h, expected, _id) = crash_at_active(restore_mode).await;
        // The state an older build could leave: Active held for the operator.
        let mut state = Journal::read(&h.state_dir).unwrap();
        state.current.as_mut().unwrap().rollback_attempted = true;
        Journal::open(lock(&h.state_dir).unwrap())
            .unwrap()
            .record(state)
            .unwrap();
        h.fake.clear_log();

        let mut e = h.engine();
        let r = e.restore_previous(true).await.unwrap();
        assert_eq!(r.outcome, Outcome::RolledBack, "{label}: {}", r.detail);
        assert_eq!(h.fake.count("restore"), 0, "{label}: {:?}", h.fake.log());
        assert_eq!(h.fake.count("stop_server"), 0, "{label}");
        assert_eq!(
            active_version(&e),
            Some(expected.version.clone()),
            "{label}"
        );
        assert_eq!(
            h.fake.running().as_ref(),
            Some(&expected.images.server),
            "{label}"
        );
        assert_eq!(h.fake.db_format(), Some(expected.state_format), "{label}");
        assert!(!h.fake.maintenance_on(), "{label}");
        assert!(e.journal_state().current.is_none(), "{label}");
    }
}
