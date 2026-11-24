use super::*;
use aq_central_authority::Digest;
use aq_server::agent_access::supervisor_conversation::reservations::{ActorView, Preview};
struct Ready<'a> {
    preview: Preview,
    policy: &'a Digest,
}

impl Service<'_> {
    pub(super) async fn scan(&self, ctl: &Control, cursor: &mut Poll) -> Result<()> {
        let pool = self.pool;
        let user = user(pool, ctl).await?;
        let actor = self.actor(ctl, &user).await?;
        let preview = self
            .owner
            .preview_batch(
                &user,
                PreviewRequest {
                    actor_id: ctl.actor,
                    expected_actor_revision: actor.revision,
                    scan_after: cursor.scan_after,
                    through_cursor: None,
                },
            )
            .await
            .map_err(api_error)?;
        match preview.status {
            PreviewStatus::NoEligibleMail => {
                cursor.scan_after = preview.scanned_through_cursor;
                self.report("idle", "Ready for messages", None).await?;
            }
            PreviewStatus::PartialScan => {
                cursor.scan_after = preview.scanned_through_cursor;
                self.report("waiting", "Checking queued messages", None)
                    .await?;
            }
            PreviewStatus::Ready => {
                self.reserve(
                    &user,
                    ctl,
                    Ready {
                        preview,
                        policy: &actor.policy_digest,
                    },
                )
                .await?;
            }
            PreviewStatus::ReservationOutstanding => {
                self.outstanding(&user, ctl, &actor).await?;
            }
            PreviewStatus::Disabled => {
                self.report("stopped", "Supervisor policy is disabled", None)
                    .await?;
            }
        }
        Ok(())
    }

    async fn actor(&self, ctl: &Control, user: &User) -> Result<ActorView> {
        let (pool, config) = (self.pool, self.config);
        let mut actor = self.owner.inspect_actor(user, ctl.actor).await.ok();
        let approved: Option<Uuid> = sqlx::query_scalar(
            "SELECT approved_by FROM central_supervisor_actors WHERE actor_id=$1",
        )
        .bind(ctl.actor)
        .fetch_optional(pool)
        .await?;
        if actor.as_ref().is_none_or(|a| !a.policy.enabled) || approved != Some(user.id) {
            self.owner
                .configure_actor(
                    user,
                    &serde_json::to_vec(&Configure {
                        operation_id: Uuid::new_v4(),
                        actor_id: ctl.actor,
                        expected_actor_revision: actor.as_ref().map_or(0, |a| a.revision),
                        policy: LocalPolicy {
                            schema_version: 1,
                            kind: PolicyKind::GlobalSupervisorReservation,
                            actor_id: ctl.actor,
                            conversation_id: config.conversation_id,
                            enabled: true,
                        },
                    })?,
                )
                .await
                .map_err(api_error)?;
            actor = Some(
                self.owner
                    .inspect_actor(user, ctl.actor)
                    .await
                    .map_err(api_error)?,
            );
        }
        actor.context("supervisor policy is unavailable")
    }
    async fn reserve(&self, user: &User, ctl: &Control, batch: Ready<'_>) -> Result<()> {
        let Ready { preview, policy } = batch;
        let pool = self.pool;
        let reservation = self
            .owner
            .reserve_batch(
                user,
                &serde_json::to_vec(&Reserve {
                    operation_id: Uuid::new_v4(),
                    reservation_id: Uuid::new_v4(),
                    actor_id: ctl.actor,
                    expected_actor_revision: preview.actor_revision,
                    expected_policy_revision: preview.policy_revision,
                    scan_after: preview.scan_after,
                    through_cursor: preview.through_cursor,
                    selection_digest: preview.selection_digest,
                })?,
            )
            .await
            .map_err(api_error)?
            .reservation
            .context("supervisor reservation was refused")?;
        let generation = aq_supervisor_control::authority::sessions(pool)
            .inspect(ctl.actor)
            .await?
            .map_or(1, |s| s.run.plan().generation + 1);
        let session = Uuid::new_v4();
        self.start_turn(
            ctl,
            reservation.id,
            plan::Generation {
                policy,
                number: generation,
                session,
            },
        )
        .await?;
        Ok(())
    }
    async fn outstanding(&self, user: &User, ctl: &Control, actor: &ActorView) -> Result<()> {
        let pool = self.pool;
        // A dispatch recorded before plan installation is recoverable only
        // if its generation never entered central authority. Preserve its
        // immutable input and operation/session IDs; never retarget an
        // admitted, handed-off or uncertain generation.
        let prior = aq_supervisor_control::authority::sessions(pool)
            .inspect(ctl.actor)
            .await?;
        let closed = prior.as_ref().is_none_or(|s| {
            matches!(
                s.run.phase(),
                Phase::StoppedReported | Phase::CancelledBeforeStart
            )
        });
        let next = prior.as_ref().map_or(1, |s| s.run.plan().generation + 1);
        let row=sqlx::query("SELECT a.current_reservation,d.generation,d.session_id,d.created_at_ms,(SELECT c.updated_at_ms FROM supervisor_controls c WHERE c.actor_id=a.actor_id) AS control_ms FROM central_supervisor_actors a LEFT JOIN central_supervisor_dispatches d ON d.actor_id=a.actor_id AND d.reservation_id=a.current_reservation WHERE a.actor_id=$1").bind(ctl.actor).fetch_one(pool).await?;
        let generation = row
            .get::<Option<i64>, _>("generation")
            .map_or(next, |g| g as u64);
        if closed && generation == next {
            let reservation: Uuid = row
                .get::<Option<Uuid>, _>("current_reservation")
                .context("retained reservation missing")?;
            let session = row
                .get::<Option<Uuid>, _>("session_id")
                .unwrap_or_else(Uuid::new_v4);
            self.start_turn(
                ctl,
                reservation,
                plan::Generation {
                    policy: &actor.policy_digest,
                    number: generation,
                    session,
                },
            )
            .await?;
        } else if let Some(release) = (DispatchTime {
            created: row.get("created_at_ms"),
            control_updated: row.get::<Option<i64>, _>("control_ms").unwrap_or(0),
        })
        .stopped_turn(closed, generation, next)
        {
            let mut retry = release;
            if !retry && let Some(previous) = &prior {
                retry = retry_unstarted(pool, previous).await?;
            }
            if retry {
                self.release(user, ctl, actor).await?;
            } else {
                self.report(
                        "failed",
                        "The last turn ended without a reply; stop and start the supervisor to retry its messages",
                        None,
                    )
                    .await?;
            }
        } else {
            self.report("uncertain", "An earlier reservation needs inspection", None)
                .await?;
        }
        Ok(())
    }
    async fn release(&self, user: &User, ctl: &Control, actor: &ActorView) -> Result<()> {
        let reservation = actor
            .current_reservation
            .as_ref()
            .context("retained reservation missing")?;
        self.owner
            .cancel_reservation(
                user,
                &serde_json::to_vec(&Cancel {
                    operation_id: Uuid::new_v4(),
                    actor_id: ctl.actor,
                    reservation_id: reservation.id,
                    expected_reservation_revision: reservation.revision,
                })?,
            )
            .await
            .map_err(api_error)?;
        self.report(
            "waiting",
            "Retrying the messages of a turn that ended without a reply",
            None,
        )
        .await?;
        Ok(())
    }
    async fn start_turn(
        &self,
        ctl: &Control,
        reservation: Uuid,
        generation: plan::Generation<'_>,
    ) -> Result<()> {
        let (pool, config) = (self.pool, self.config);
        // The saved level and candidate are resolved against the active role
        // snapshot at each start: a level that is gone or no longer lists the
        // candidate keeps the supervisor waiting with the reason, and it starts
        // by itself once the level lists it again.
        let resolved = aq_supervisor_control::resolve_stored(pool, &ctl.profile).await;
        let digest = match resolved.map_err(|e| anyhow::anyhow!(e.1))? {
            Ok(digest) => digest,
            Err(reason) => {
                let stage = format!("Waiting: {reason}");
                self.report("waiting", &stage, None).await?;
                return Ok(());
            }
        };
        let mut ctl = ctl.clone();
        ctl.profile["snapshot_digest"] = serde_json::json!(digest);
        let ctl = &ctl;
        if let Some(reason) = crate::config::swarm_wait(pool, Some(&ctl.profile)).await? {
            self.report("waiting", swarm_stage(reason), None).await?;
            return Ok(());
        }
        let turn = dispatch::prepare_turn(
            pool,
            ctl.actor,
            reservation,
            generation.number,
            generation.session,
            now(),
        )
        .await
        .map_err(api_error)?;
        let candidate = plan::assemble(config, ctl, generation, turn)?;
        plan::install(config, &candidate)?;
        let (_, host) = installed(config, pool)?.context("installed supervisor turn missing")?;
        let state = host
            .start(
                crate::Startup {
                    central: candidate.central.clone(),
                    host: candidate.host.clone(),
                    input: protected_read(&candidate.startup_file, 1048576)?,
                },
                candidate.central.max_permit_ms,
            )
            .await?;
        host.enable_replies(ctl.actor, now()).await?;
        self.report("working", "Working on the conversation", Some(&state))
            .await
    }
}
