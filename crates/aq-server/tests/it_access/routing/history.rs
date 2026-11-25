use super::*;

#[tokio::test]
async fn write_sets_aq_route_keeps_other_metadata_and_records_history() {
    let e = setup().await;
    let first = route(Source::Router);
    assert_eq!(
        e.write(3, &first).await.unwrap(),
        Written::Routed { sequence: 1 }
    );
    let node = e.node().await;
    assert_eq!(node["version"], 4);
    assert_eq!(node["metadata"]["aq:priority"], 5);
    assert_eq!(node["metadata"]["aq:project_id"], json!(Uuid::from_u128(7)));
    assert_eq!(e.stored().await, Some(first.compact(1).unwrap()));
    let mut second = route(Source::Router);
    second.class = class("complex-high");
    assert_eq!(
        e.write(4, &second).await.unwrap(),
        Written::Routed { sequence: 2 }
    );
    assert_eq!(e.stored().await.unwrap().c, class("complex-high"));
    assert_eq!(
        e.history().await,
        vec![
            StoredRoute {
                sequence: 2,
                route: Some(second),
                at_ms: NOW_MS
            },
            StoredRoute {
                sequence: 1,
                route: Some(first),
                at_ms: NOW_MS
            },
        ]
    );
}

#[tokio::test]
async fn write_against_an_older_task_version_is_stale_and_leaves_no_history_row() {
    let e = setup().await;
    let before = e.node().await;
    // The node is at version 3: an older expected version is refused locally.
    assert_eq!(
        e.write(2, &route(Source::Router)).await.unwrap(),
        Written::Stale
    );
    assert_eq!(
        routing::clear(&e.state, e.workspace, e.task, 2, Source::Owner)
            .await
            .unwrap(),
        Written::Stale
    );
    assert_eq!(e.posts().await, 0, "a known-stale write never posts");
    assert_eq!(e.node().await, before);
    assert!(e.history().await.is_empty());
    // A concurrent edit between the read and the post: the graph answers 409
    // and the history row is removed again.
    e.fake.lock().await.race_after_get = true;
    assert_eq!(
        e.write(3, &route(Source::Router)).await.unwrap(),
        Written::Stale
    );
    assert_eq!(e.posts().await, 1, "the raced write reached the graph");
    assert_eq!(e.node().await["version"], 4);
    assert_eq!(e.node().await["metadata"], before["metadata"]);
    assert!(e.history().await.is_empty());
    // The freed sequence is used again under a fresh idempotency key.
    assert_eq!(
        e.write(4, &route(Source::Router)).await.unwrap(),
        Written::Routed { sequence: 1 }
    );
    assert_eq!(e.history().await.len(), 1);
    // A write the graph refuses outright leaves no history row either.
    e.fake
        .lock()
        .await
        .nodes
        .get_mut(&e.task.to_string())
        .unwrap()["retired"] = json!(true);
    assert!(e.write(5, &route(Source::Router)).await.is_err());
    assert_eq!(e.history().await.len(), 1);
}

#[tokio::test]
async fn router_never_overwrites_an_owner_route_but_the_owner_can_replace_a_router_route() {
    let e = setup().await;
    assert_eq!(
        e.write(3, &route(Source::Router)).await.unwrap(),
        Written::Routed { sequence: 1 }
    );
    let owner = route(Source::Owner);
    assert_eq!(
        e.write(4, &owner).await.unwrap(),
        Written::Routed { sequence: 2 }
    );
    assert_eq!(e.stored().await, Some(owner.compact(2).unwrap()));
    let posts = e.posts().await;
    assert_eq!(
        e.write(5, &route(Source::Router)).await.unwrap(),
        Written::Stale
    );
    assert_eq!(
        routing::clear(&e.state, e.workspace, e.task, 5, Source::Router)
            .await
            .unwrap(),
        Written::Stale
    );
    assert_eq!(e.posts().await, posts, "a refused router write never posts");
    assert_eq!(e.stored().await.unwrap().s, Source::Owner);
    assert_eq!(e.history().await.len(), 2);
    let mut replaced = route(Source::Owner);
    replaced.class = class("deep-max");
    assert_eq!(
        e.write(5, &replaced).await.unwrap(),
        Written::Routed { sequence: 3 }
    );
}

#[tokio::test]
async fn clear_removes_the_key_and_records_a_cleared_entry() {
    let e = setup().await;
    let first = route(Source::Router);
    assert_eq!(
        e.write(3, &first).await.unwrap(),
        Written::Routed { sequence: 1 }
    );
    assert_eq!(
        routing::clear(&e.state, e.workspace, e.task, 4, Source::Owner)
            .await
            .unwrap(),
        Written::Routed { sequence: 2 }
    );
    let node = e.node().await;
    assert_eq!(
        node["metadata"],
        json!({"aq:priority": 5, "aq:project_id": Uuid::from_u128(7)})
    );
    let history = e.history().await;
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0],
        StoredRoute {
            sequence: 2,
            route: None,
            at_ms: NOW_MS
        }
    );
    assert_eq!(history[1].route, Some(first));
}
