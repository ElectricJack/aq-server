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

#[tokio::test]
async fn launch_backoff_uses_saved_settings_for_scheduling_and_work() {
    use crate::replacement_release::{finish_with_stop, granted_at};
    use aq_profiles::roles::LaunchRetryBackoff;
    use aq_server::{
        execution::profiled::Stop,
        profiled_execution::replacement::{self, Assessment},
    };
    use std::sync::atomic::Ordering;

    let mut f = common::profiled::fixture().await;
    let task = f.selection.lineage.task_id;
    let pool = f.db.pool().await;
    aq_role_settings::startup(&pool, NOW as i64).await.unwrap();
    for generation in 1..=2 {
        if generation > 1 {
            next_attempt(&mut f, generation);
        }
        let (attempt, _, mut hot) = granted_at(&f, NOW + (generation - 1) * 1_000).await;
        let stop = Stop::NotStarted {
            receipt: common::profiled::artifact(
                &format!("aq.launch-closure/{attempt}"),
                b"nonstart",
            ),
        };
        finish_with_stop(&pool, attempt, &mut hot, stop).await;
        let at = NOW + generation * 1_000;
        f.time.store(at as i64, Ordering::SeqCst);
        let assessment = replacement::assess_task(&f.state, task, at).await.unwrap();
        let r = assessment.replacement().expect("the nonstart can release");
        assert!(r.waiting_for_launch_release());
        assert_eq!(r.exempt_attempts, generation as u32);
        replacement::release(&f.state, task, r).await.unwrap();
        if generation == 1 {
            assert!(
                aq_human_work::launch_retry_wait(&pool, task, at as i64)
                    .await
                    .is_none()
            );
            let ready = replacement::assess_task(&f.state, task, at).await.unwrap();
            assert!(!ready.replacement().unwrap().needs_release);
        }
    }
    let last = f.profiled.offer.binding.attempt_id;
    let released_at = NOW + 2_000;
    let original = aq_human_work::launch_retry_wait(&pool, task, released_at as i64)
        .await
        .unwrap();
    assert_eq!(original["next_check_at_ms"], released_at + 30_000);
    let settings = aq_role_settings::settings(&pool).await.unwrap();
    let mut defaults = settings.selection_defaults;
    defaults.launch_retry_backoff = LaunchRetryBackoff {
        initial_delay_ms: 10_000,
        max_delay_ms: 15_000,
    };
    let edit = aq_role_settings::Writer::new(&pool, f.actor, released_at as i64)
        .edit(settings.selection_defaults_revision, Uuid::new_v4());
    aq_role_settings::save_selection_defaults(edit, &defaults)
        .await
        .unwrap();

    let wake_at = released_at + 10_000;
    let waiting = aq_human_work::launch_retry_wait(&pool, task, (wake_at - 1) as i64)
        .await
        .unwrap();
    assert_eq!(waiting["attempt_id"], serde_json::json!(last));
    assert_eq!(waiting["next_check_at_ms"], wake_at);
    assert_eq!(
        replacement::assess_task(&f.state, task, wake_at - 1)
            .await
            .unwrap(),
        Assessment::Blocked(replacement::LAUNCH_BACKOFF)
    );
    assert!(
        aq_human_work::launch_retry_wait(&pool, task, wake_at as i64)
            .await
            .is_none()
    );
    let ready = replacement::assess_task(&f.state, task, wake_at)
        .await
        .unwrap();
    assert_eq!(ready.replacement().unwrap().predecessor, last);
}
