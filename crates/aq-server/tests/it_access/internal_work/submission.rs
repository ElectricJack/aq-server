use super::*;

#[tokio::test]
async fn submit_without_candidates_is_failed_no_candidate() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(&pool, &request(true), &[], NOW)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.reason, Some(Reason::NoCandidate));
    assert_eq!(row.finished_at_ms, Some(NOW));
    assert_eq!(row.workspace_kind, "none");
    assert_eq!(row.escalation, "none");
    assert_eq!(
        (row.affected_kind.as_str(), row.affected_id.as_str()),
        ("task", "T-1")
    );
    assert!(matches!(
        row.outcome(),
        Some(Outcome::Failed {
            reason: Reason::NoCandidate,
            ..
        })
    ));
    assert!(
        store::Queue::at(&pool, NOW)
            .claim(Way::Api, &[], "r")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn request_bounds_are_enforced() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    assert!(request(false).validate().is_ok());
    let zero_deadline = Request {
        deadline_ms: 0,
        ..request(false)
    };
    assert!(zero_deadline.validate().is_err());
    assert!(
        store::submit(&pool, &zero_deadline, &[api("openai")], NOW)
            .await
            .is_err()
    );
    let big_input = Request {
        input: "x".repeat(2 * 1024 * 1024),
        ..request(false)
    };
    assert!(
        store::submit(&pool, &big_input, &[api("openai")], NOW)
            .await
            .is_err()
    );
    for bad in [
        Request {
            deadline_ms: 900_001,
            ..request(false)
        },
        Request {
            instructions: "x".repeat(256 * 1024 + 1),
            ..request(false)
        },
        Request {
            max_output_bytes: 0,
            ..request(false)
        },
        Request {
            max_output_bytes: 1_048_577,
            ..request(false)
        },
    ] {
        assert!(bad.validate().is_err());
    }
    let mut unknown = serde_json::to_value(request(false)).unwrap();
    unknown["purpose"] = json!("routing_clasification");
    assert!(serde_json::from_value::<Request>(unknown).is_err());
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_tasks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(none, 0);
}
