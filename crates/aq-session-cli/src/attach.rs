use super::*;
use aq_session_protocol::{ControllerLease, OutputFrame, Snapshot};

struct Session<'a> {
    client: &'a Client,
    info: &'a TerminalInfo,
}

#[derive(Default)]
struct OutputCursor {
    subscription: Uuid,
    expires_at_ms: u64,
    after: u64,
}

#[derive(Default)]
struct InputSession {
    stdin: Option<Stdin>,
    lease: Option<ControllerLease>,
    permit: Option<epoch::Permit>,
    sequence: u64,
    pending: Option<Uuid>,
}

pub(super) async fn attach(
    client: &Client,
    id: Uuid,
    seconds: u32,
    control: bool,
) -> Result<(), String> {
    if !(1..=300).contains(&seconds) {
        return Err("attach duration must be1..300 seconds".into());
    }
    let initial = info(client, id).await?;
    let end = tokio::time::Instant::now() + Duration::from_secs(seconds as u64);
    let mut output = OutputCursor::default();
    let mut input = InputSession::start(
        Session {
            client,
            info: &initial,
        },
        control,
    )
    .await?;
    while tokio::time::Instant::now() < end {
        let clock = now()?;
        let current = info(client, id).await?;
        if current.terminal != initial.terminal
            || current.session_revision != initial.session_revision
        {
            return Err("session authority changed; attach again explicitly".into());
        }
        let session = Session {
            client,
            info: &current,
        };
        output.poll(&session, clock).await?;
        input.poll_receipt(&session).await?;
        input.tick(&session, now()?).await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    input
        .finish(&Session {
            client,
            info: &initial,
        })
        .await;
    Ok(())
}

impl OutputCursor {
    async fn poll(&mut self, session: &Session<'_>, clock: u64) -> Result<(), String> {
        if clock.saturating_add(5000) >= self.expires_at_ms {
            self.subscription = Uuid::new_v4();
            self.expires_at_ms = clock.saturating_add(20000);
            print(
                &mutate(
                    session.client,
                    &Mutation::Watch(aq_session_api::WatchRequest {
                        terminal: session.info.terminal.clone(),
                        operation_id: self.subscription,
                        expected_session_revision: session.info.session_revision,
                        expires_at_ms: self.expires_at_ms,
                        after_sequence: Some(self.after),
                    }),
                )
                .await?,
            )?;
        }
        let after_text = self.after.to_string();
        let sub_text = self.subscription.to_string();
        let id = session.info.terminal.terminal_id;
        let output: OutputReply = session
            .client
            .get_json(
                &format!("/api/v1/terminals/{id}/output"),
                &[
                    ("subscription_id", &sub_text),
                    ("after_sequence", &after_text),
                ],
            )
            .await
            .map_err(|e| e.to_string())?;
        self.advance(session.info, &output)?;
        print(&output)
    }

    fn advance(&mut self, current: &TerminalInfo, output: &OutputReply) -> Result<(), String> {
        match output {
            OutputReply::Ready {
                view,
                snapshot,
                frames,
                next_sequence,
                ..
            } => {
                let cursor_matches =
                    view.subscription_id == self.subscription && *next_sequence >= self.after;
                check_snapshot(current, view, snapshot, cursor_matches)?;
                self.after = check_frames(
                    current,
                    frames,
                    self.after.max(snapshot.through_sequence),
                    *next_sequence,
                )?;
            }
            OutputReply::Pending {
                terminal,
                subscription_id,
                ..
            } => {
                if terminal != &current.terminal || *subscription_id != self.subscription {
                    return Err("pending response identity mismatch".into());
                }
            }
        }
        Ok(())
    }
}

fn check_snapshot(
    current: &TerminalInfo,
    view: &aq_session_api::WatchView,
    snapshot: &Snapshot,
    cursor_matches: bool,
) -> Result<(), String> {
    snapshot.validate().map_err(|_| "invalid snapshot")?;
    if snapshot.state.len() > current.terminal_policy.replay_bytes as usize {
        return Err("snapshot exceeds sealed policy".into());
    }

    let terminals = (&view.terminal, &snapshot.terminal);
    let snapshot_ref = (view.snapshot_id, view.through_sequence);
    if terminals != (&current.terminal, &current.terminal)
        || snapshot_ref != (snapshot.snapshot_id, snapshot.through_sequence)
    {
        return Err("output response identity/barrier mismatch".into());
    }
    if !cursor_matches {
        return Err("output response identity/barrier mismatch".into());
    }
    check_references(current, view, snapshot)?;
    validate_watch_view(view)
}
fn check_references(
    current: &TerminalInfo,
    view: &aq_session_api::WatchView,
    snapshot: &Snapshot,
) -> Result<(), String> {
    let geometry = (view.session_revision, view.columns, view.rows)
        == (current.session_revision, snapshot.columns, snapshot.rows);
    if !geometry {
        return Err("output reference binding mismatch".into());
    }
    let state = (
        view.snapshot_reference.identity.as_str(),
        view.snapshot_reference.digest.as_str(),
    ) == (
        snapshot.snapshot_id.to_string().as_str(),
        snapshot.digest.as_str(),
    );
    let renderer = (
        view.renderer_reference.identity.as_str(),
        view.renderer_reference.digest.as_str(),
        view.renderer_reference.revision.as_str(),
    ) == (
        snapshot.renderer.id.as_str(),
        snapshot.renderer.digest.as_str(),
        snapshot.renderer.version.as_str(),
    );
    if !state || !renderer {
        return Err("output reference binding mismatch".into());
    }
    Ok(())
}

fn validate_watch_view(view: &aq_session_api::WatchView) -> Result<(), String> {
    let encoded = serde_json::to_vec(&aq_session_api::ContractDocument::WatchView(Box::new(
        view.clone(),
    )))
    .map_err(|_| "invalid watch view")?;
    aq_session_api::decode(&encoded).map_err(|_| "invalid watch view")?;
    Ok(())
}

fn check_frames(
    current: &TerminalInfo,
    frames: &[OutputFrame],
    mut previous: u64,
    next: u64,
) -> Result<u64, String> {
    for frame in frames {
        frame.validate().map_err(|_| "invalid frame")?;
        if frame.bytes.len() > current.terminal_policy.max_frame_bytes as usize {
            return Err("frame exceeds sealed policy".into());
        }
        if frame.terminal != current.terminal || frame.sequence <= previous {
            return Err("output frame barrier/order mismatch".into());
        }
        previous = frame.sequence;
    }
    if next != previous {
        return Err("output continuation mismatch".into());
    }
    Ok(next)
}

impl InputSession {
    async fn start(session: Session<'_>, control: bool) -> Result<Self, String> {
        if !control {
            return Ok(Self::default());
        }
        let stdin = Some(Stdin::new()?);
        if !session.info.terminal_policy.input_enabled {
            return Err("terminal is output-only".into());
        }
        let reply = mutate(
            session.client,
            &Mutation::Control(aq_session_api::AcquireControlRequest {
                terminal: session.info.terminal.clone(),
                operation_id: Uuid::new_v4(),
                expected_session_revision: session.info.session_revision,
                expected_controller_generation: session.info.control.generation,
                expires_at_ms: now()?.saturating_add(15000),
            }),
        )
        .await?;
        let MutationReply::Control { status, .. } = reply else {
            return Err("unexpected controller reply".into());
        };
        let lease = status.lease.clone();
        print(&status)?;
        Ok(Self {
            stdin,
            lease,
            ..Self::default()
        })
    }

    async fn poll_receipt(&mut self, session: &Session<'_>) -> Result<(), String> {
        let Some(operation) = self.pending else {
            return Ok(());
        };
        let id = session.info.terminal.terminal_id;
        let receipt: aq_session_api::InputReceipt = session
            .client
            .get_json(&format!("/api/v1/terminals/{id}/receipts/{operation}"), &[])
            .await
            .map_err(|e| e.to_string())?;
        if receipt.operation_id != operation || receipt.terminal != session.info.terminal {
            return Err("input receipt identity mismatch".into());
        }
        print(&receipt)?;
        match receipt.disposition {
            InputDisposition::DeliveryUnknown => {
                self.stdin = None;
                self.pending = None;
                eprintln!(
                    "input {operation} is unknown; explicit fresh capture and resolution required"
                );
            }
            InputDisposition::Applied | InputDisposition::Expired | InputDisposition::Rejected => {
                self.pending = None
            }
            _ => {}
        }
        Ok(())
    }

    async fn tick(&mut self, session: &Session<'_>, clock: u64) -> Result<(), String> {
        let Some(old) = self.lease.clone() else {
            return Ok(());
        };
        self.check_controller(session.info, &old)?;
        if self.pending.is_some() {
            return Ok(());
        }
        if self.stdin.is_some() && clock >= renewal_at(&old) {
            return self.renew(session, &old, clock).await;
        }
        if self.can_send(session.info, &old, clock) {
            self.poll_input(session, &old).await?;
        }
        Ok(())
    }

    fn can_send(&self, current: &TerminalInfo, lease: &ControllerLease, clock: u64) -> bool {
        let expires = self.permit.as_ref().map_or(lease.expires_at_ms, |p| {
            p.expires_at_ms.min(lease.expires_at_ms)
        });
        current.control.fence_acknowledged && clock.saturating_add(250) < expires
    }

    async fn poll_input(
        &mut self,
        session: &Session<'_>,
        lease: &ControllerLease,
    ) -> Result<(), String> {
        if session.info.input_mode == draft::InputMode::EpochV1 && self.permit.is_none() {
            self.open_epoch(session, lease).await?;
        }
        self.read_input(session, lease).await
    }

    fn check_controller(
        &mut self,
        current: &TerminalInfo,
        old: &ControllerLease,
    ) -> Result<(), String> {
        if current
            .control
            .lease
            .as_ref()
            .is_none_or(|l| l.lease_id != old.lease_id)
        {
            return Err("controller replaced; input stopped".into());
        }
        if current.control.input_paused {
            self.stdin = None;
        }

        Ok(())
    }

    async fn renew(
        &mut self,
        session: &Session<'_>,
        old: &ControllerLease,
        clock: u64,
    ) -> Result<(), String> {
        let reply = mutate(
            session.client,
            &Mutation::Renew(draft::RenewControl {
                terminal: session.info.terminal.clone(),
                operation_id: Uuid::new_v4(),
                expected_session_revision: session.info.session_revision,
                expected_controller_generation: old.generation,
                lease_id: old.lease_id,
                expires_at_ms: clock.saturating_add(15000),
            }),
        )
        .await?;
        let MutationReply::Control { status, .. } = reply else {
            return Err("unexpected renewal reply".into());
        };
        self.lease = status.lease.clone();
        self.permit = None;
        self.sequence = 0;
        print(&status)
    }

    async fn open_epoch(
        &mut self,
        session: &Session<'_>,
        old: &ControllerLease,
    ) -> Result<(), String> {
        let reply = mutate(
            session.client,
            &Mutation::OpenEpoch(epoch::Open {
                terminal: session.info.terminal.clone(),
                operation_id: Uuid::new_v4(),
                expected_session_revision: session.info.session_revision,
                expected_controller_generation: old.generation,
                lease_id: old.lease_id,
                expires_at_ms: now()?.saturating_add(2000),
            }),
        )
        .await?;
        let MutationReply::Epoch { permit: p, .. } = reply else {
            return Err("unexpected epoch reply".into());
        };
        p.validate().map_err(|_| "invalid input epoch")?;

        if (&p.terminal, p.lease_id, p.controller_generation)
            != (&session.info.terminal, old.lease_id, old.generation)
            || p.expires_at_ms > old.expires_at_ms
        {
            return Err("input epoch binding mismatch".into());
        }
        self.permit = Some(p);
        Ok(())
    }

    async fn read_input(
        &mut self,
        session: &Session<'_>,
        old: &ControllerLease,
    ) -> Result<(), String> {
        let Some(reader) = &self.stdin else {
            return Ok(());
        };
        let Some(bytes) = reader.read(session.info.terminal_policy.max_input_bytes as usize)?
        else {
            return Ok(());
        };
        if bytes.is_empty() {
            self.stdin = None;
            return Ok(());
        }
        self.sequence += 1;
        let input = self.make_input(session.info, old, bytes)?;
        let operation = input.operation_id;
        let request = if let Some(p) = &self.permit {
            Mutation::EpochInput(epoch::Request {
                version: 1,
                capability: aq_session_protocol::Text::new(epoch::CAPABILITY)
                    .map_err(|_| "invalid epoch capability")?,
                epoch_id: p.epoch_id,
                sequence: self.sequence,
                input,
            })
        } else {
            Mutation::Input(input)
        };
        let reply = mutate(session.client, &request).await.map_err(|_| {
            format!("input {operation} outcome unknown; inspect its receipt; do not resend")
        })?;
        print(&reply)?;
        self.pending = Some(operation);
        Ok(())
    }

    fn make_input(
        &self,
        current: &TerminalInfo,
        old: &ControllerLease,
        bytes: Vec<u8>,
    ) -> Result<aq_session_api::InputRequest, String> {
        let operation = if let Some(p) = &self.permit {
            epoch::operation_id(p, self.sequence).map_err(|_| "input ID derivation failed")?
        } else {
            Uuid::new_v4()
        };
        Ok(aq_session_api::InputRequest {
            terminal: current.terminal.clone(),
            operation_id: operation,
            lease_id: old.lease_id,
            expected_controller_generation: old.generation,
            expected_session_revision: current.session_revision,
            expires_at_ms: now()?
                .saturating_add(2000)
                .min(old.expires_at_ms)
                .min(self.permit.as_ref().map_or(u64::MAX, |p| p.expires_at_ms)),
            bytes,
            escape_key: false,
        })
    }

    async fn finish(self, session: &Session<'_>) {
        if self.pending.is_some() {
            eprintln!(
                "input receipt remains pending; inspect its operation ID before further input"
            );
        }
        if let Some(old) = self.lease {
            let _ = mutate(
                session.client,
                &Mutation::Release(draft::ReleaseControl {
                    terminal: session.info.terminal.clone(),
                    operation_id: Uuid::new_v4(),
                    expected_session_revision: session.info.session_revision,
                    expected_controller_generation: old.generation,
                    lease_id: old.lease_id,
                }),
            )
            .await;
        }
    }
}
fn renewal_at(lease: &ControllerLease) -> u64 {
    lease.issued_at_ms.saturating_add(
        lease
            .expires_at_ms
            .saturating_sub(lease.issued_at_ms)
            .saturating_mul(2)
            / 3,
    )
}
