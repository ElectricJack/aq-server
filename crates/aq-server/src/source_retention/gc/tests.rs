use super::*;

fn config() -> SourceRetentionConfig {
    toml::from_str("staging_root='/tmp/staging'\ngit_root='/tmp/git'\ngit_socket='/tmp/git.sock'")
        .unwrap()
}

#[test]
fn defaults_keep_two_until_thirty_days_after_settlement() {
    let config = config();
    assert_eq!(config.keep_per_task, 2);
    assert_eq!(config.max_age, 30 * 86400);
    let settled = 1_000_000;
    let recent = DateTime::from_timestamp_millis(settled + 1000).unwrap();
    assert_eq!(release_reason(settled, recent, 0, &config), None);
    assert_eq!(release_reason(settled, recent, 1, &config), None);
    assert_eq!(
        release_reason(settled, recent, 2, &config),
        Some("source_retention.keep_per_task")
    );
    let expired = DateTime::from_timestamp_millis(settled + config.max_age as i64 * 1000).unwrap();
    assert_eq!(
        release_reason(settled, expired, 0, &config),
        Some("source_retention.max_age")
    );
    let future = DateTime::from_timestamp_millis(settled - 1).unwrap();
    assert_eq!(release_reason(settled, future, 0, &config), None);
    let mut invalid = config.clone();
    invalid.max_age = 0;
    assert!(invalid.validate().is_err());
    invalid.max_age = u64::MAX;
    assert!(invalid.validate().is_err());
}

#[test]
fn open_uncertain_and_preserved_attempts_remain_protected() {
    let finished = json!({"status":"finished","stop":{"kind":"stopped"}});
    assert!(!protected(false, "profiled_v1", &finished));
    // A preserved stopped attempt still holds its task/source claim.
    assert!(protected(true, "profiled_v1", &finished));
    for state in [
        json!(null),
        json!({"status":"granted"}),
        json!({"status":"unknown","stop":{"kind":"unknown"}}),
        json!({"status":"finished","stop":{"kind":"unknown"}}),
    ] {
        assert!(protected(false, "profiled_v1", &state));
    }
    let legacy = json!({"status":"finished","stopped":true,"disposition":{"retention_release":true},"report":{"pending_operation_ids":[]}});
    assert!(!protected(false, "legacy_fixture", &legacy));
    let mut uncertain = legacy.clone();
    uncertain["report"]["pending_operation_ids"] = json!([Uuid::new_v4()]);
    assert!(protected(false, "legacy_fixture", &uncertain));
    assert!(protected(false, "future_kind", &finished));
}
