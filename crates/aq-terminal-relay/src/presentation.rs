//! Bounded presentation of an authorized, disposable terminal replay.
use crate::output::View;
use aq_session_api as api;
use aq_session_protocol::{OutputFrame, OutputGap, TerminalIdentity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub const MAX_FRAMES: usize = 64;
pub const MAX_RAW_BYTES: usize = 65536;
pub const MAX_WIRE_BYTES: usize = 524288;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    ResidentTail,
    NotRetained,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TailReply {
    pub terminal: TerminalIdentity,
    pub subscription_id: Uuid,
    pub session_revision: u64,
    pub server_time_ms: u64,
    pub expires_at_ms: u64,
    pub last_observed_at_ms: Option<u64>,
    pub availability: Availability,
    pub disconnected: bool,
    pub initial_incomplete: bool,
    pub grid: Option<(u16, u16)>,
    pub frames: Vec<OutputFrame>,
    pub gaps: Vec<OutputGap>,
    pub next_sequence: u64,
    pub more: bool,
}
impl std::fmt::Debug for TailReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TailReply(<scoped disposable output>)")
    }
}
impl TailReply {
    pub fn apply(&mut self, replay: View, checked_at: u64) {
        let after = self.next_sequence;
        self.grid = replay.snapshot.as_ref().map(|s| (s.columns, s.rows));
        let observed = checked_at.saturating_sub(replay.age_ms);
        if observed > 0 {
            self.last_observed_at_ms = Some(observed);
            self.availability = Availability::ResidentTail;
        }
        let count = replay.frames.len();
        self.frames = bounded_frames(replay.frames, MAX_FRAMES, MAX_RAW_BYTES);
        self.more = self.frames.len() < count;
        let last_frame = self.frames.last().map_or(after, |f| f.sequence);
        // Do not advance over a retained frame omitted by the response budget.
        let cutoff = if self.more {
            last_frame
        } else {
            replay
                .gaps
                .iter()
                .map(|g| g.last_missing)
                .chain(replay.snapshot.as_ref().map(|s| s.through_sequence))
                .fold(last_frame, u64::max)
        }
        .max(after);
        self.gaps = replay
            .gaps
            .into_iter()
            .filter_map(|mut gap| {
                gap.first_missing = gap.first_missing.max(after.saturating_add(1));
                gap.last_missing = gap.last_missing.min(cutoff);
                (gap.first_missing <= gap.last_missing).then_some(gap)
            })
            .collect();
        self.next_sequence = cutoff;
    }
}

pub struct OutputObservation {
    pub terminal: TerminalIdentity,
    pub revision: u64,
    pub peer_observed_at: u64,
    pub expires_at: u64,
    pub clock: u64,
    pub active: bool,
}
impl OutputObservation {
    pub fn render(
        self,
        query: &crate::api::ViewQuery,
        replay: Option<crate::output::View>,
    ) -> crate::api::OutputReply {
        use crate::api::OutputReply;
        let pending = || OutputReply::Pending {
            terminal: self.terminal.clone(),
            subscription_id: query.subscription_id,
            last_peer_observed_at_ms: self.peer_observed_at,
            disconnected: true,
            resynchronize: true,
        };
        let Some(replay) = replay else {
            return pending();
        };
        let Some(snapshot) = replay.snapshot else {
            return pending();
        };
        let original = replay.frames.len();
        let frames = bounded_frames(replay.frames, 128, 32768);
        let next = frames
            .last()
            .map_or(snapshot.through_sequence, |f| f.sequence)
            .max(query.after_sequence);
        OutputReply::Ready {
            view: Box::new(api::WatchView {
                terminal: self.terminal,
                subscription_id: query.subscription_id,
                session_revision: self.revision,
                expires_at_ms: self.expires_at,
                last_observed_at_ms: self.clock.saturating_sub(replay.age_ms),
                disconnected: !self.active
                    || replay.disconnected
                    || self.clock.saturating_sub(self.peer_observed_at) > 25000,
                coverage: if replay.truncated {
                    api::Coverage::Gapped
                } else {
                    api::Coverage::Complete
                },
                truncated: replay.truncated,
                snapshot_id: snapshot.snapshot_id,
                snapshot_reference: api::Source {
                    identity: snapshot.snapshot_id.to_string(),
                    digest: snapshot.digest.as_str().into(),
                    revision: "1".into(),
                },
                renderer_reference: api::Source {
                    identity: snapshot.renderer.id.as_str().into(),
                    digest: snapshot.renderer.digest.as_str().into(),
                    revision: snapshot.renderer.version.as_str().into(),
                },
                through_sequence: snapshot.through_sequence,
                columns: snapshot.columns,
                rows: snapshot.rows,
                gaps: replay.gaps,
            }),
            snapshot,
            next_sequence: next,
            more: frames.len() < original,
            frames,
        }
    }
}
fn bounded_frames(
    frames: Vec<aq_session_protocol::OutputFrame>,
    limit: usize,
    byte_limit: usize,
) -> Vec<aq_session_protocol::OutputFrame> {
    let mut total = 0;
    frames
        .into_iter()
        .take(limit)
        .take_while(|frame| {
            total += frame.bytes.len();
            total <= byte_limit
        })
        .collect()
}
