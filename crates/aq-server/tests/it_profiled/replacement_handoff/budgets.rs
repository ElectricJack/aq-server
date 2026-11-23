use super::*;

/// The next attempt of the same task: a new attempt, session, offer and
/// generation on the fixture's offer (as `replacement_source` does).
fn next_attempt(f: &mut common::profiled::Fixture, generation: u64) {
    let attempt = Uuid::new_v4();
    let session = aq_session_protocol::SessionIdentity::Remote {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
        attempt_id: attempt,
        assignment_generation: generation,
        session_id: Uuid::new_v4(),
    };
    let offer = &mut f.profiled.offer;
    offer.binding.session = session.clone();
    offer.binding.attempt_id = attempt;
    offer.binding.assignment_generation = generation;
    offer.binding.offer_id = Uuid::new_v4();
    offer.manifest.bindings.session = session;
    offer.manifest.bindings.attempt_id = attempt;
    offer.manifest.bindings.assignment_generation = generation;
    offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&offer.manifest).unwrap();
    f.decision = Uuid::new_v4();
}

/// Spec 0007 Q1 (Review Focus 6): owner and service handoffs are not charged
/// to the B4 retry budget; a crash restart is.
#[tokio::test]
async fn owner_stop_handoffs_do_not_exhaust_the_retry_budget() {
    use crate::replacement_release::{fail, granted_at};
    use aq_server::profiled_execution::replacement::{self, Assessment};
    let mut f = common::profiled::fixture().await;
    let task = f.selection.lineage.task_id;
    let pool = f.db.pool().await;
    let mut attempts = vec![];
    for generation in 1..=3 {
        if generation > 1 {
            next_attempt(&mut f, generation);
        }
        let (attempt, _, mut hot) = granted_at(&f, NOW - 10_000 + generation * 1_000).await;
        fail(&pool, attempt, &mut hot).await;
        sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) VALUES($1,$2,'owner_stop',now())")
            .bind(attempt).bind(f.node.node_id).execute(&pool).await.unwrap();
        attempts.push(attempt);
        if generation < 3 {
            let assessed = replacement::assess_task(&f.state, task, NOW).await.unwrap();
            let Assessment::Replace(r) = assessed else {
                panic!("attempt {generation} is replaceable: {assessed:?}");
            };
            replacement::release(&f.state, task, &r).await.unwrap();
        }
    }
    let assess = || replacement::assess_task(&f.state, task, NOW);
    let reason = |attempt: Uuid, reason: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query("UPDATE attempt_handoffs SET reason=$2 WHERE attempt_id=$1")
                .bind(attempt)
                .bind(reason)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    // Three owner stops: none is charged, the lineage is still replaceable.
    let assessed = assess().await.unwrap();
    let Assessment::Replace(r) = assessed else {
        panic!("owner stops never exhaust the retry budget: {assessed:?}");
    };
    assert_eq!((r.attempts_started, r.exempt_attempts), (3, 3));
    // A service stop is exempt as well; a crash restart is charged.
    reason(attempts[1], "service_stop").await;
    reason(attempts[0], "restart").await;
    let assessed = assess().await.unwrap();
    let Assessment::Replace(r) = assessed else {
        panic!("one charged crash restart stays within the budget: {assessed:?}");
    };
    assert_eq!((r.attempts_started, r.exempt_attempts), (3, 2));
    // Three crashes are charged in full and exhaust the budget.
    reason(attempts[1], "restart").await;
    reason(attempts[2], "restart").await;
    assert_eq!(
        assess().await.unwrap(),
        Assessment::Blocked("lineage_retry_budget_exhausted")
    );
}

/// Review I1(c): only the first exempt handoffs of a lineage are free, up to
/// Settings › Selection defaults `max_uncharged_continuations`; later owner
/// stops are charged as normal attempts against `max_attempts`. The fixture's
/// attempts pin no policy, so the current Settings values apply.
#[tokio::test]
async fn exempt_handoffs_are_capped_at_five_per_lineage() {
    use crate::replacement_release::{fail, granted_at};
    use aq_server::profiled_execution::replacement::{self, Assessment};
    let mut f = common::profiled::fixture().await;
    let task = f.selection.lineage.task_id;
    let pool = f.db.pool().await;
    let limits = aq_profiles::roles::SelectionDefaults::default();
    let (cap, last) = (
        u64::from(limits.max_uncharged_continuations),
        u64::from(limits.max_uncharged_continuations + limits.max_attempts),
    );
    for generation in 1..=last {
        if generation > 1 {
            next_attempt(&mut f, generation);
        }
        let (attempt, _, mut hot) = granted_at(&f, NOW - 10_000 + generation * 1_000).await;
        fail(&pool, attempt, &mut hot).await;
        sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) VALUES($1,$2,'owner_stop',now())")
            .bind(attempt).bind(f.node.node_id).execute(&pool).await.unwrap();
        let assessed = replacement::assess_task(&f.state, task, NOW).await.unwrap();
        let Assessment::Replace(r) = assessed else {
            assert_eq!(generation, last, "{assessed:?}");
            assert_eq!(
                assessed,
                Assessment::Blocked("lineage_retry_budget_exhausted")
            );
            return;
        };
        let exempt = generation.min(cap) as u32;
        assert_eq!(
            (r.attempts_started, r.exempt_attempts),
            (generation as u32, exempt)
        );
        replacement::release(&f.state, task, &r).await.unwrap();
    }
    panic!("the last owner stop leaves `max_attempts` charged attempts");
}
