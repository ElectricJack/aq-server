use super::*;
use aq_central_authority::owner::OwnerError;
use aq_central_authority::{DispatchState, HandoffDisposition};
use sqlx::PgConnection;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Recovery {
    ReplyRecorded,
    RetryUnstarted,
    OwnerRestart,
    RetryLimit,
    EvidenceUnavailable,
    OutcomeUnconfirmed,
}

impl Recovery {
    fn classify(
        replied: bool,
        owner_restarted: bool,
        never_started: bool,
        attempts: i64,
        limit: u8,
    ) -> Self {
        if replied {
            Self::ReplyRecorded
        } else if !never_started {
            Self::OutcomeUnconfirmed
        } else if owner_restarted {
            Self::OwnerRestart
        } else if attempts > i64::from(limit) {
            Self::RetryLimit
        } else {
            Self::RetryUnstarted
        }
    }
}

pub(super) async fn recovery(
    pool: &PgPool,
    state: &Snapshot,
    owner_restarted: bool,
    limit: u8,
) -> Result<Recovery> {
    let mut tx = pool.begin().await?;
    let dispatch_id = state.run.plan().dispatch_id;
    let replied: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM central_supervisor_replies WHERE dispatch_id=$1)",
    )
    .bind(dispatch_id)
    .fetch_one(&mut *tx)
    .await?;
    if replied {
        return Ok(Recovery::ReplyRecorded);
    }

    let never_started = match state.run.dispatch() {
        DispatchState::NotAttempted => state.run.phase() == Phase::CancelledBeforeStart,
        DispatchState::Resolved { evidence } => {
            evidence.disposition == HandoffDisposition::NotHandedOff
        }
        DispatchState::Unknown { .. } => {
            match crate::verifier::not_started(&mut tx, &state.run).await {
                Ok(evidence) => evidence.is_some(),
                Err(OwnerError::Evidence) => false,
                Err(OwnerError::Storage) => return Ok(Recovery::EvidenceUnavailable),
                Err(error) => return Err(error.into()),
            }
        }
        DispatchState::IntentCommitted => false,
    };
    let attempts = if never_started && !owner_restarted {
        dispatch_attempts(&mut tx, dispatch_id).await?
    } else {
        0
    };
    Ok(Recovery::classify(
        false,
        owner_restarted,
        never_started,
        attempts,
        limit,
    ))
}

/// Count the most-dispatched message in the failed turn, including that turn.
/// A new arrival can change the batch digest without changing the retry
/// history of any original message.
async fn dispatch_attempts(conn: &mut PgConnection, dispatch_id: Uuid) -> Result<i64> {
    let attempts = sqlx::query_scalar(
        "SELECT max(used.attempts) FROM (
            SELECT count(*) AS attempts
            FROM central_supervisor_dispatches d
            JOIN central_supervisor_mail_members m USING (actor_id, reservation_id)
            JOIN central_supervisor_mail_members p
              ON p.actor_id=m.actor_id AND p.message_id=m.message_id
                 AND p.cursor=m.cursor AND p.payload_digest=m.payload_digest
            JOIN central_supervisor_dispatches a
              ON a.actor_id=p.actor_id AND a.reservation_id=p.reservation_id
                 AND a.generation<=d.generation
            WHERE d.dispatch_id=$1
            GROUP BY m.message_id
        ) used",
    )
    .bind(dispatch_id)
    .fetch_one(conn)
    .await?;
    let attempts: i64 = Option::context(attempts, "missing retry history for stopped turn")?;
    Ok(attempts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_central_authority::{
        CancelReason, Clock, Decision, Document, Event, HandoffEvidence, HostObservationRef, Run,
        StopEvidenceRef, decode,
    };
    use sqlx::postgres::PgPoolOptions;

    #[test]
    fn confirmed_reply_wins_over_restart_and_retry() {
        assert_eq!(
            Recovery::classify(true, true, true, 1, 1),
            Recovery::ReplyRecorded,
        );
    }

    #[test]
    fn only_verified_no_start_retries_automatically() {
        assert_eq!(
            Recovery::classify(false, false, false, 1, 1),
            Recovery::OutcomeUnconfirmed,
        );
        assert_eq!(
            Recovery::classify(false, false, true, 1, 1),
            Recovery::RetryUnstarted,
        );
        assert_eq!(
            Recovery::classify(false, false, true, 2, 1),
            Recovery::RetryLimit,
        );
        assert_eq!(
            Recovery::classify(false, false, true, 1, 0),
            Recovery::RetryLimit,
        );
        assert_eq!(
            Recovery::classify(false, true, false, 2, 1),
            Recovery::OutcomeUnconfirmed,
        );
        assert_eq!(
            Recovery::classify(false, true, true, 2, 1),
            Recovery::OwnerRestart,
        );
    }

    #[tokio::test]
    async fn recovery_uses_exact_mail_history_and_does_not_replay_a_handoff() {
        let db = aq_server::test_support::TestDb::create().await;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(db.options())
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        sqlx::raw_sql(
            "CREATE TEMP TABLE central_supervisor_dispatches (
                dispatch_id uuid, actor_id uuid, reservation_id uuid, generation bigint
            );
            CREATE TEMP TABLE central_supervisor_mail_members (
                actor_id uuid, reservation_id uuid, message_id uuid, cursor bigint, payload_digest text
            );
            CREATE TEMP TABLE central_supervisor_replies (dispatch_id uuid);",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        let examples: Value = serde_json::from_str(include_str!(
            "../../../aq-central-authority/artifacts/examples.json"
        ))
        .unwrap();
        let Document::Plan(plan) = decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
        else {
            panic!("expected plan fixture")
        };
        let actor = plan.subject.actor_id();
        let first_dispatch = plan.dispatch_id;
        let clock = Clock {
            machine_boot_id: Uuid::new_v4(),
            boot_ms: 1,
        };
        let run = Run::new(plan, clock.clone())
            .unwrap()
            .transition(
                &Event::Cancel {
                    reason: CancelReason::Operator,
                },
                clock,
            )
            .unwrap()
            .state;
        let stopped = Snapshot { revision: 2, run };
        let original = Uuid::new_v4();
        for generation in 1..=2_i64 {
            let reservation = Uuid::new_v4();
            let dispatch = if generation == 1 {
                first_dispatch
            } else {
                Uuid::new_v4()
            };
            sqlx::query("INSERT INTO central_supervisor_dispatches VALUES ($1,$2,$3,$4)")
                .bind(dispatch)
                .bind(actor)
                .bind(reservation)
                .bind(generation)
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query("INSERT INTO central_supervisor_mail_members VALUES ($1,$2,$3,1,'old')")
                .bind(actor)
                .bind(reservation)
                .bind(original)
                .execute(&mut *conn)
                .await
                .unwrap();
            if generation == 2 {
                sqlx::query(
                    "INSERT INTO central_supervisor_mail_members VALUES ($1,$2,$3,2,'new')",
                )
                .bind(actor)
                .bind(reservation)
                .bind(Uuid::new_v4())
                .execute(&mut *conn)
                .await
                .unwrap();
            }
            let attempts = dispatch_attempts(&mut conn, dispatch).await.unwrap();
            assert_eq!(attempts, generation);
            let expected = if generation == 1 {
                Recovery::RetryUnstarted
            } else {
                Recovery::RetryLimit
            };
            assert_eq!(
                Recovery::classify(false, false, true, attempts, 1),
                expected,
            );
        }
        drop(conn);
        assert_eq!(
            recovery(&pool, &stopped, false, 1).await.unwrap(),
            Recovery::RetryUnstarted,
        );
        let ambiguous = stopped_turn(&examples, false);
        assert_eq!(
            recovery(&pool, &ambiguous, false, 1).await.unwrap(),
            Recovery::EvidenceUnavailable,
        );
        sqlx::query("CREATE TEMP TABLE central_host_facts (launch_id uuid, kind text, digest text, body text)")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            recovery(&pool, &ambiguous, false, 1).await.unwrap(),
            Recovery::OutcomeUnconfirmed,
        );
        let handed_off = stopped_turn(&examples, true);
        assert_eq!(handed_off.run.plan().dispatch_id, first_dispatch);
        for owner_restarted in [false, true] {
            assert_eq!(
                recovery(&pool, &handed_off, owner_restarted, 1)
                    .await
                    .unwrap(),
                Recovery::OutcomeUnconfirmed,
            );
        }
        sqlx::query("INSERT INTO central_supervisor_replies VALUES ($1)")
            .bind(first_dispatch)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            recovery(&pool, &handed_off, false, 1).await.unwrap(),
            Recovery::ReplyRecorded,
        );
    }

    fn stopped_turn(examples: &Value, completed_handoff: bool) -> Snapshot {
        let Document::Transition(started) =
            decode(&serde_json::to_vec(&examples["start_transition"]).unwrap()).unwrap()
        else {
            panic!("expected start transition fixture")
        };
        let Document::Event(Event::CommitStart { launch, .. }) =
            decode(&serde_json::to_vec(&examples["commit_start_event"]).unwrap()).unwrap()
        else {
            panic!("expected commit start event fixture")
        };
        let permit = started.state.permit().unwrap();
        let clock = Clock {
            machine_boot_id: permit.machine_boot_id,
            boot_ms: permit.issued_boot_ms + 1,
        };
        let mut state = started.state;
        if completed_handoff {
            let handed_off = state
                .transition(
                    &Event::Handoff {
                        evidence: HandoffEvidence {
                            association: state.association().unwrap().clone(),
                            disposition: HandoffDisposition::HandedOff,
                            receipt: launch.intent.clone(),
                        },
                    },
                    clock.clone(),
                )
                .unwrap();
            assert_eq!(handed_off.decision, Decision::Applied);
            state = handed_off.state;
        }
        let stopped = state
            .transition(
                &Event::Stopped {
                    evidence: StopEvidenceRef {
                        host: HostObservationRef {
                            binding: launch.binding,
                            launch_id: launch.launch_id,
                            guardian: launch.intent.clone(),
                            observation: launch.intent.clone(),
                        },
                        stopped_writers: launch.intent,
                    },
                },
                clock,
            )
            .unwrap();
        assert_eq!(stopped.decision, Decision::Applied);
        Snapshot {
            revision: 4,
            run: stopped.state,
        }
    }
}
