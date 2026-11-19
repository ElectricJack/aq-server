use super::*;

fn grant(lease: Option<&str>) -> GitGrant {
    let repository = Uuid::new_v4();
    GitGrant {
        endpoint: endpoint(repository),
        repository_id: repository,
        base: "a".repeat(40),
        task_ref: task_branch(Uuid::new_v4()),
        lease: lease.map(str::to_owned),
        update_from_main: false,
    }
}
fn pkt(line: &str) -> String {
    format!("{:04x}{line}", line.len() + 4)
}
/// A body that yields one byte per frame: the parser must not depend on
/// frame boundaries.
struct Bytewise(std::collections::VecDeque<u8>);
impl Body for Bytewise {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(
            self.0
                .pop_front()
                .map(|b| Ok(Frame::data(Bytes::from(vec![b])))),
        )
    }
}
async fn check(grant: &GitGrant, body: &str) -> Result<Bytes, Refusal> {
    commands(&mut Bytewise(body.bytes().collect()), grant).await
}

#[test]
fn only_smart_http_routes_are_served() {
    let (get, post) = (Method::GET, Method::POST);
    assert_eq!(
        route(&get, "info/refs", Some("service=git-upload-pack")),
        Ok(false)
    );
    assert_eq!(
        route(&get, "info/refs", Some("service=git-receive-pack")),
        Ok(false)
    );
    assert_eq!(route(&post, "git-upload-pack", None), Ok(false));
    assert_eq!(route(&post, "git-receive-pack", None), Ok(true));
    for (method, path, query) in [
        (&get, "info/refs", None),
        (&get, "info/refs", Some("service=other")),
        (&get, "git-receive-pack", None),
        (&post, "objects/info/packs", None),
        (&post, "../x/git-receive-pack", None),
    ] {
        assert_eq!(route(method, path, query), Err(Refusal::NotFound), "{path}");
    }
}

#[test]
fn a_push_command_must_update_the_task_ref_from_the_lease() {
    let (old, new) = ("b".repeat(40), "c".repeat(40));
    let created = grant(None);
    let leased = grant(Some(&old));
    let line = |old: &str, new: &str, r: &str| format!("{old} {new} {r}\0report-status\n");
    let ok = |g: &GitGrant, old: &str| check_command(line(old, &new, &g.task_ref).as_bytes(), g);
    assert_eq!(ok(&created, ZERO_OID), Ok(()));
    assert_eq!(ok(&leased, &old), Ok(()));
    assert_eq!(ok(&created, &old), Err(Refusal::StaleLease));
    assert_eq!(ok(&leased, ZERO_OID), Err(Refusal::StaleLease));
    assert_eq!(ok(&leased, &new), Err(Refusal::StaleLease));
    for other in ["refs/heads/main", "refs/heads/aq/other", "refs/tags/x"] {
        let refused = check_command(line(&old, &new, other).as_bytes(), &leased);
        assert!(matches!(refused, Err(Refusal::Forbidden(_))), "{other}");
    }
    let delete = check_command(line(&old, ZERO_OID, &leased.task_ref).as_bytes(), &leased);
    assert!(matches!(delete, Err(Refusal::Forbidden(_))));
    for bad in ["", "shallow aaaa", "x y z", &format!("{old} {new}")] {
        let refused = check_command(bad.as_bytes(), &leased);
        assert!(matches!(refused, Err(Refusal::BadRequest(_))), "{bad}");
    }
}

#[tokio::test]
async fn the_command_list_is_read_up_to_its_flush_and_replayed() {
    let (old, new) = ("b".repeat(40), "c".repeat(40));
    let g = grant(Some(&old));
    let command = pkt(&format!("{old} {new} {}\0report-status\n", g.task_ref));
    // git's probe of a large push is a bare flush.
    assert_eq!(check(&g, "0000PACK").await.unwrap(), Bytes::from("0000"));
    let body = format!("{command}0000PACK...");
    let read = check(&g, &body).await.unwrap();
    assert_eq!(read, Bytes::from(format!("{command}0000")));
    let mut replayed = Counted {
        prefix: Some(read),
        inner: Bytewise("PACK...".bytes().collect()),
        left: 7,
    };
    let all = (&mut replayed).collect().await.unwrap().to_bytes();
    assert_eq!(all, Bytes::from(format!("{command}0000PACK...")));
    // A second command, a stale lease, a truncated or garbled list.
    let other = pkt(&format!("{old} {new} refs/heads/aq/other\n"));
    assert!(matches!(
        check(&g, &format!("{command}{other}0000")).await,
        Err(Refusal::Forbidden(_))
    ));
    let stale = pkt(&format!("{new} {new} {}\n", g.task_ref));
    assert_eq!(
        check(&g, &format!("{stale}0000")).await,
        Err(Refusal::StaleLease)
    );
    for bad in [command.as_str(), "zzzz", "0003", ""] {
        assert!(
            matches!(check(&g, bad).await, Err(Refusal::BadRequest(_))),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn the_body_fails_past_the_push_limit() {
    let mut body = Counted {
        prefix: None,
        inner: Bytewise("12345".bytes().collect()),
        left: 4,
    };
    assert!((&mut body).collect().await.is_err());
}

#[test]
fn a_result_names_a_pushed_ref_a_conflict_or_nothing() {
    let headers = |pairs: &[(&'static str, &str)]| {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    };
    let (reference, head) = ("refs/heads/aq/t", "a".repeat(40));
    let parse = |pairs: &[(&'static str, &str)]| Pushed::from_headers(&headers(pairs));
    assert_eq!(parse(&[]), Ok(None));
    let both = parse(&[("aq-pushed-ref", reference), ("aq-pushed-head", &head)]);
    let at = PushedRef {
        reference: reference.into(),
        head: head.clone(),
    };
    assert_eq!(both, Ok(Some(Pushed::At(at))));
    assert_eq!(
        parse(&[("aq-push-conflict", "true")]),
        Ok(Some(Pushed::Conflict))
    );
    for bad in [
        vec![("aq-pushed-ref", reference)],
        vec![("aq-pushed-head", head.as_str())],
        vec![("aq-push-conflict", "yes")],
        vec![
            ("aq-pushed-ref", reference),
            ("aq-pushed-head", &head),
            ("aq-push-conflict", "true"),
        ],
    ] {
        assert!(
            matches!(parse(&bad), Err(PushedRefusal::BadRequest(_))),
            "{bad:?}"
        );
    }
}
