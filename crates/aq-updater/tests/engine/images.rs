use super::*;

#[test]
fn image_prune_selects_all_but_current_and_previous() {
    use aq_updater::engine::{image_repository, images_to_prune};
    let img = |d: char| {
        format!(
            "ghcr.io/electricjack/agent-q-server@sha256:{}",
            d.to_string().repeat(64)
        )
    };
    let (old, prev, cur) = (img('a'), img('b'), img('c'));
    assert_eq!(
        image_repository(&cur),
        Some("ghcr.io/electricjack/agent-q-server")
    );
    let present = vec![old.clone(), prev.clone(), cur.clone(), old.clone()];
    let keep = vec![cur.clone(), prev.clone()];
    assert_eq!(images_to_prune(&present, &keep), vec![old]);
    assert!(images_to_prune(&keep, &keep).is_empty());
}

#[tokio::test]
async fn image_prune_after_update_keeps_current_and_previous_release() {
    let (h, mut e, a) = with_a().await;
    let stale = "ghcr.io/electricjack/agent-q-server@sha256:9999999999999999999999999999999999999999999999999999999999999999";
    let postgres = "docker.io/library/postgres@sha256:8888888888888888888888888888888888888888888888888888888888888888";
    h.fake.add_images(&[stale, postgres]);

    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    let images = h.fake.images();
    assert!(!images.iter().any(|i| i == stale), "{images:?}");
    for kept in [&a.images.server, &b.images.server] {
        assert!(images.contains(kept), "{kept}: {images:?}");
    }
    assert!(
        images.iter().any(|i| i == postgres),
        "other repositories untouched"
    );

    let c = manifest("1.2.0", 1, &[1]);
    h.publish(&c);
    assert_eq!(
        e.install(req("1.2.0")).await.unwrap().outcome,
        Outcome::Succeeded
    );
    let images = h.fake.images();
    assert!(!images.contains(&a.images.server), "{images:?}");
    assert!(images.contains(&b.images.server) && images.contains(&c.images.server));
}

#[tokio::test]
async fn image_prune_failure_does_not_change_the_outcome() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("list_images", 0, "docker down");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_eq!(active_version(&e), Some(ver(B)));
}
