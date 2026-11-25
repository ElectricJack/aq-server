use super::*;

pub struct AttemptOutcome {
    attempt_id: Uuid,
    outcome: TransportOutcome,
}
impl AttemptOutcome {
    pub fn new(attempt_id: Uuid, outcome: TransportOutcome) -> Self {
        Self {
            attempt_id,
            outcome,
        }
    }
}
impl DispatchAuthority<'_> {
    fn permits(&self, route: &Route) -> bool {
        self.recipient_can_view && self.route.enabled && self.route == route
    }
}
/// Dispatch queue fields derived from the delivery's status in the same write.
pub struct Schedule {
    pub status: &'static str,
    pub due_at_ms: u64,
    pub attempt_id: Option<Uuid>,
    pub lease_until_ms: Option<u64>,
}
impl Delivery {
    pub fn schedule(&self, lease_ms: u64) -> Result<Schedule> {
        let (status, due_at_ms, attempt_id, lease_until_ms) = match self.status {
            Status::Queued => ("queued", self.message.created_at_ms, None, None),
            Status::Dispatching {
                attempt_id,
                started_at_ms,
            } => (
                "dispatching",
                started_at_ms,
                Some(attempt_id),
                Some(started_at_ms.checked_add(lease_ms).ok_or(Error::Invalid)?),
            ),
            Status::RetryScheduled { next_attempt_ms } => {
                ("retry_scheduled", next_attempt_ms, None, None)
            }
            Status::AcceptedByTransport { .. } => ("accepted_by_transport", 0, None, None),
            Status::DeliveryUnknown { .. } => ("delivery_unknown", 0, None, None),
            Status::PermanentlyFailed { .. } => ("permanently_failed", 0, None, None),
            Status::Suppressed { .. } => ("suppressed", 0, None, None),
        };
        Ok(Schedule {
            status,
            due_at_ms,
            attempt_id,
            lease_until_ms,
        })
    }
    pub fn queued(
        delivery_id: Uuid,
        message: Message,
        route: Route,
        payload: Payload,
    ) -> Result<Self> {
        message.validate()?;
        route.validate()?;
        payload.validate()?;
        require(!delivery_id.is_nil())?;
        if message.scope != route.scope || message.recipient_user_id != route.recipient_user_id {
            return Err(Error::ScopeChanged);
        }
        let immutable_digest = immutable_digest(delivery_id, &message, &route, &payload)?;
        let result = Self {
            delivery_id,
            last_transition_ms: message.created_at_ms,
            message,
            route,
            payload,
            immutable_digest,
            attempts: 0,
            first_attempt_ms: None,
            duplicate_risk_retries: 0,
            status: Status::Queued,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        self.message.validate()?;
        self.route.validate()?;
        self.payload.validate()?;
        self.validate_binding()?;
        self.validate_timing()?;
        self.validate_status()
    }
    fn validate_binding(&self) -> Result<()> {
        require(
            !self.delivery_id.is_nil()
                && self.message.scope == self.route.scope
                && self.message.recipient_user_id == self.route.recipient_user_id,
        )?;
        require(
            self.immutable_digest
                == immutable_digest(self.delivery_id, &self.message, &self.route, &self.payload)?,
        )
    }
    fn validate_timing(&self) -> Result<()> {
        require(
            self.attempts <= self.route.retry.max_attempts
                && self.duplicate_risk_retries <= self.attempts
                && (self.attempts == 0) == self.first_attempt_ms.is_none(),
        )?;
        require(
            self.last_transition_ms >= self.message.created_at_ms
                && positive(self.last_transition_ms),
        )?;
        require(self.first_attempt_ms.is_none_or(|first| {
            (self.message.created_at_ms..=self.last_transition_ms).contains(&first)
        }))
    }
    fn has_attempt(&self, attempt: Uuid) -> bool {
        self.attempts > 0 && !attempt.is_nil()
    }
    fn validate_status(&self) -> Result<()> {
        match &self.status {
            Status::Dispatching {
                attempt_id,
                started_at_ms,
            } => {
                require(self.has_attempt(*attempt_id) && *started_at_ms == self.last_transition_ms)
            }
            Status::DeliveryUnknown {
                attempt_id,
                since_ms,
            } => require(
                self.has_attempt(*attempt_id)
                    && self
                        .first_attempt_ms
                        .is_some_and(|first| (first..=self.last_transition_ms).contains(since_ms)),
            ),
            Status::AcceptedByTransport {
                attempt_id,
                message_id,
                accepted_at_ms,
            } => require(
                self.has_attempt(*attempt_id)
                    && snowflake(message_id)
                    && *accepted_at_ms == self.last_transition_ms,
            ),
            Status::RetryScheduled { next_attempt_ms } => require(
                self.attempts > 0
                    && positive(*next_attempt_ms)
                    && *next_attempt_ms >= self.last_transition_ms,
            ),
            Status::Queued => require(self.attempts == 0),
            _ => Ok(()),
        }
    }
    fn advance(&self, now: u64) -> Result<Self> {
        self.validate()?;
        require(positive(now) && now >= self.last_transition_ms)?;
        Ok(self.clone())
    }
    fn budget(&self, now: u64) -> Result<()> {
        if self.attempts >= self.route.retry.max_attempts
            || self
                .first_attempt_ms
                .is_some_and(|first| now.saturating_sub(first) >= self.route.retry.max_elapsed_ms)
        {
            return Err(Error::Exhausted);
        }
        Ok(())
    }
    pub fn suppression(
        &self,
        route: Option<&Route>,
        visible: bool,
        now: u64,
    ) -> Option<SuppressionReason> {
        if !visible {
            Some(SuppressionReason::ScopeRevoked)
        } else if route != Some(&self.route) || !self.route.enabled {
            Some(SuppressionReason::RouteChanged)
        } else if now >= self.message.expires_at_ms {
            Some(SuppressionReason::MessageExpired)
        } else {
            None
        }
    }
    /// The dispatcher already checked scope, recipient and destination. A spent
    /// attempt budget produces a terminal record under the same revision fence.
    pub fn dispatch_or_exhaust(
        &self,
        authority: DispatchAuthority,
        attempt_id: Uuid,
        now: u64,
    ) -> Result<Self> {
        match self.begin(authority, attempt_id, now) {
            Err(Error::Exhausted) => Ok(Self {
                status: Status::PermanentlyFailed {
                    reason: FailureReason::RetryExhausted,
                },
                last_transition_ms: now,
                ..self.clone()
            }),
            other => other,
        }
    }
    /// Persist the returned state before sending. The caller must CAS the old
    /// revision and may dispatch only its exact committed attempt ID once.
    pub fn begin(
        &self,
        authority: DispatchAuthority<'_>,
        attempt_id: Uuid,
        now: u64,
    ) -> Result<Self> {
        let mut next = self.advance(now)?;
        authority.route.validate()?;
        require(!attempt_id.is_nil())?;
        if !authority.permits(&self.route) {
            return Err(Error::ScopeChanged);
        }
        match self.status {
            Status::Queued => {}
            Status::RetryScheduled { next_attempt_ms } if now >= next_attempt_ms => {}
            _ => return Err(Error::NotDue),
        }
        if now >= self.message.expires_at_ms {
            return Err(Error::NotDue);
        }
        self.budget(now)?;
        next.attempts += 1;
        next.first_attempt_ms.get_or_insert(now);
        next.last_transition_ms = now;
        next.status = Status::Dispatching {
            attempt_id,
            started_at_ms: now,
        };
        next.validate()?;
        Ok(next)
    }
    pub fn observe(&self, observation: AttemptOutcome, now: u64, jitter: u64) -> Result<Self> {
        let mut next = self.advance(now)?;
        let AttemptOutcome {
            attempt_id,
            outcome,
        } = observation;
        if !matches!(self.status,Status::Dispatching{attempt_id:id,..} if id==attempt_id) {
            return Err(Error::Conflict);
        }
        next.last_transition_ms = now;
        next.status = match outcome {
            TransportOutcome::Accepted { message_id } => {
                require(snowflake(&message_id))?;
                Status::AcceptedByTransport {
                    attempt_id,
                    message_id,
                    accepted_at_ms: now,
                }
            }
            TransportOutcome::Unknown => Status::DeliveryUnknown {
                attempt_id,
                since_ms: now,
            },
            TransportOutcome::Permanent(reason) => Status::PermanentlyFailed { reason },
            TransportOutcome::RateLimited { retry_after_ms, .. } => {
                if retry_after_ms > 86_400_000 {
                    Status::PermanentlyFailed {
                        reason: FailureReason::ProviderDelayTooLong,
                    }
                } else if next.budget(now).is_err() {
                    Status::PermanentlyFailed {
                        reason: FailureReason::RetryExhausted,
                    }
                } else {
                    let ceiling = self
                        .route
                        .retry
                        .initial_delay_ms
                        .saturating_mul(1u64 << self.attempts.min(16))
                        .min(self.route.retry.max_delay_ms);
                    let delay =
                        (ceiling / 2 + jitter % (ceiling - ceiling / 2 + 1)).max(retry_after_ms);
                    let due = now.checked_add(delay).ok_or(Error::Invalid)?;
                    if due >= self.message.expires_at_ms
                        || due.saturating_sub(self.first_attempt_ms.unwrap())
                            >= self.route.retry.max_elapsed_ms
                    {
                        Status::PermanentlyFailed {
                            reason: FailureReason::RetryExhausted,
                        }
                    } else {
                        Status::RetryScheduled {
                            next_attempt_ms: due,
                        }
                    }
                }
            }
        };
        next.validate()?;
        Ok(next)
    }
    /// Recovery never repeats an in-flight HTTP send. Any stale dispatcher is
    /// fenced by the owner transaction before this transition is committed.
    pub fn recover(&self, now: u64) -> Result<Self> {
        if let Status::Dispatching { attempt_id, .. } = self.status {
            self.observe(
                AttemptOutcome::new(attempt_id, TransportOutcome::Unknown),
                now,
                0,
            )
        } else {
            self.advance(now)
        }
    }
    /// Caller durably authorizes a NEW retry intent with its own idempotency key.
    /// This does not assert the previous attempt had no external effect.
    pub fn retry_unknown(&self, now: u64) -> Result<Self> {
        let mut next = self.advance(now)?;
        if !matches!(self.status, Status::DeliveryUnknown { .. })
            || !self.route.retry.allow_duplicate_risk
        {
            return Err(Error::Conflict);
        }
        self.budget(now)?;
        if now >= self.message.expires_at_ms {
            return Err(Error::NotDue);
        }
        next.duplicate_risk_retries += 1;
        next.last_transition_ms = now;
        next.status = Status::RetryScheduled {
            next_attempt_ms: now,
        };
        next.validate()?;
        Ok(next)
    }
}

fn immutable_digest(
    id: Uuid,
    message: &Message,
    route: &Route,
    payload: &Payload,
) -> Result<Digest> {
    let mut value =
        serde_json::to_value(("aq.notification-delivery/1", id, message, route, payload))
            .map_err(|_| Error::Invalid)?;
    value.sort_all_objects();
    Ok(Digest::of(
        &serde_json::to_vec(&value).map_err(|_| Error::Invalid)?,
    ))
}
