use super::*;

/// A failed `unit` check whose job (id 9) has `log`, behind GitHub's redirect.
fn failed_with_log(mock: &Mock, log: Option<&str>) {
    let open = || pull(7, json!({}));
    let failed = json!({"total_count": 1, "check_runs": [
        {"id": 9, "name": "unit", "status": "completed", "conclusion": "failure"}]});
    poll(mock, open(), open(), failed.clone());
    // The log read lists the runs again, then follows the job's redirect.
    mock.reply(200, vec![], failed);
    match log {
        Some(log) => {
            mock.reply(
                302,
                vec![("location", "/blob/job-9.log".to_string())],
                json!({}),
            );
            mock.reply(200, vec![], json!(log));
        }
        None => mock.reply(404, vec![], json!({"message": "Not Found"})),
    }
}
async fn log_poll(delivery: &Delivery, log_bytes: u64) -> CheckPoll {
    let required = ["unit".to_string()];
    delivery
        .await_checks_step(
            &target(),
            CheckRequest::new(7, Some(&required[..]), log_bytes),
            (1_000, 5_000),
        )
        .await
}

#[tokio::test]
async fn a_failed_check_records_the_redacted_bounded_tail_of_its_job_log() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let key = format!("{}{}", "sk-", "Zy9Xw8Vu7Ts6Rq5Po4Nm3Lk2");
    let mut log: String = (0..400).map(|i| format!("step output {i:04}\n")).collect();
    log.push_str(&format!(
        "using {key}\nAuthorization: Bearer abcdef0123456789\nFAILED test_x\n"
    ));
    failed_with_log(&mock, Some(&log));
    let p = log_poll(&delivery, 300).await;

    let c = p.completion.unwrap();
    assert_eq!(
        (c.outcome, c.output),
        ("failed", json!({"names": ["unit"]}))
    );
    let seen = p.observed.unwrap();
    assert_eq!(seen.checks_failed, ["unit"]);
    let log = seen.checks_log.expect("a tail is recorded");
    assert_eq!(log.head, sha('a'));
    assert_bounded_redaction(&log.tail, &key);
    check_log_credentials(&mock);
}
fn assert_bounded_redaction(tail: &str, key: &str) {
    assert!(tail.len() <= 300, "{} bytes", tail.len());
    assert!(tail.starts_with("== unit ==\n"), "{}", tail);
    assert!(tail.ends_with("FAILED test_x"), "{}", tail);
    assert!(tail.contains("using [redacted]"), "{}", tail);
    assert!(tail.contains("Authorization: [redacted]"), "{}", tail);
    for secret in [key, "abcdef0123456789"] {
        assert!(!tail.contains(secret), "{secret} kept: {}", tail);
    }
}
fn check_log_credentials(mock: &Mock) {
    // The job's logs are asked of the API with the installation token; the
    // signed download does not carry it.
    let requests = mock.seen();
    let asked = &requests[requests.len() - 2];
    assert_eq!(asked.path, "/api/v3/repos/Acme/widgets/actions/jobs/9/logs");
    assert!(asked.headers.contains_key("authorization"));
    let blob = &requests[requests.len() - 1];
    assert_eq!(blob.path, "/blob/job-9.log");
    assert!(!blob.headers.contains_key("authorization"));
}

#[tokio::test]
async fn a_failed_test_log_records_the_test_its_panic_and_the_job_page() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let open = || pull(7, json!({}));
    let page = "https://github.com/Acme/widgets/actions/runs/5/job/9";
    let failed = json!({"total_count": 1, "check_runs": [{"id": 9, "name": "unit",
        "status": "completed", "conclusion": "failure", "html_url": page}]});
    poll(&mock, open(), open(), failed.clone());
    mock.reply(200, vec![], failed);
    mock.reply(
        302,
        vec![("location", "/blob/job-9.log".to_string())],
        json!({}),
    );
    let mut log: String = (0..400)
        .map(|i| format!("2026-11-27T09:32:01.9700000Z    Compiling crate_{i:04}\r\n"))
        .collect();
    log.push_str(concat!(
        "2026-11-27T09:32:02.0000000Z \u{1b}[31m        FAIL\u{1b}[0m [   0.010s] widgets::it unit::adds\r\n",
        "2026-11-27T09:32:02.0000000Z thread 'unit::adds' panicked at tests/unit.rs:12:5:\r\n",
        "2026-11-27T09:32:02.0000000Z assertion `left == right` failed\r\n",
        "2026-11-27T09:32:02.0000000Z   left: 1\r\n",
        "2026-11-27T09:32:02.0000000Z  right: 2\r\n",
        "2026-11-27T09:32:02.0000000Z \r\n",
    ));
    log.push_str(&"2026-11-27T09:32:03.0000000Z Post job cleanup.\r\n".repeat(50));
    mock.reply(200, vec![], json!(log));
    let tail = log_poll(&delivery, 300).await.observed.unwrap().checks_log;
    assert_eq!(
        tail.unwrap().tail,
        format!(
            "== unit ==\n{page}\nFAIL [   0.010s] widgets::it unit::adds\n\
             thread 'unit::adds' panicked at tests/unit.rs:12:5:\n\
             assertion `left == right` failed\n  left: 1\n right: 2"
        )
    );
}

#[tokio::test]
async fn a_log_that_cannot_be_read_records_the_failed_check_without_a_tail() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    failed_with_log(&mock, None);
    let p = log_poll(&delivery, 300).await;
    let c = p.completion.unwrap();
    assert_eq!(
        (c.outcome, c.output),
        ("failed", json!({"names": ["unit"]}))
    );
    // The check's name stays (with its page, when GitHub gives one).
    let seen = p.observed.unwrap();
    let log = seen.checks_log.map(|l| l.tail);
    assert_eq!(
        (seen.checks_state, log.as_deref()),
        ("failed", Some("== unit =="))
    );

    // A transport failure is the same.
    let open = || pull(7, json!({}));
    poll(
        &mock,
        open(),
        open(),
        runs(&[("unit", "completed", Some("failure"))]),
    );
    mock.reply(500, vec![], json!({"message": "boom"}));
    let p = log_poll(&delivery, 300).await;
    assert_eq!(p.completion.unwrap().outcome, "failed");
    assert_eq!(p.observed.unwrap().checks_log, None);
}

#[tokio::test]
async fn no_log_is_read_for_a_passing_check_or_a_zero_limit() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let open = || pull(7, json!({}));
    poll(
        &mock,
        open(),
        open(),
        runs(&[("unit", "completed", Some("success"))]),
    );
    let p = log_poll(&delivery, 300).await;
    assert_eq!(p.completion.unwrap().outcome, "passed");
    assert_eq!(p.observed.unwrap().checks_log, None);
    let before = mock.seen().len();

    poll(
        &mock,
        open(),
        open(),
        runs(&[("unit", "completed", Some("failure"))]),
    );
    let p = log_poll(&delivery, 0).await;
    assert_eq!(p.completion.unwrap().outcome, "failed");
    assert_eq!(p.observed.unwrap().checks_log, None);
    assert!(
        mock.seen()[before..]
            .iter()
            .all(|r| !r.path.contains("/actions/")),
        "nothing asked for a log"
    );
}
