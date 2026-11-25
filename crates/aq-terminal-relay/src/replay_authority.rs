//! Fresh authorization for replayable outbox effects.
use super::*;

fn current(matches: bool) -> Result<()> {
    matches.then_some(()).ok_or(Error::Stale)
}

impl Access {
    pub(super) fn control_deadline(&self) -> u64 {
        self.execution_expires_at_ms.min(self.valid_until_ms)
    }

    pub(super) fn covers_deadline(&self, now: u64, deadline: u64) -> bool {
        now < deadline && deadline <= self.control_deadline()
    }
}

impl Ledger {
    /// A replay queue is never an input authority, regardless of plaintext queue
    /// flags. Fresh input effects come exclusively from a successful one-shot CAS.
    pub fn authorize_replayable_effect(
        &self,
        access: Option<&Access>,
        effect: &draft::Effect,
        now: u64,
    ) -> Result<()> {
        use draft::Effect::*;
        effect.validate().map_err(|_| Error::Invalid)?;
        current(effect.terminal() == self.terminal())?;
        let access = || access.ok_or(Error::Forbidden);
        match effect {
            Input(_) | EpochInput(_) => Err(Error::Transition),
            Fence(fence) if fence.controller.is_none() => {
                current(self.last_fence.as_ref() == Some(fence))
            }
            Fence(fence) => self.replay_fence(access()?, fence, now),
            Watch(watch) => self.replay_watch(access()?, watch, now),
            Resize(resize) => self.replay_resize(access()?, resize, now),
            Capture(capture) => self.replay_capture(access()?, capture, now),
        }
    }

    fn replay_fence(&self, a: &Access, fence: &draft::Fence, now: u64) -> Result<()> {
        self.auth(a, self.terminal(), self.session_revision, true, now)?;
        let controller = self.controller.as_ref().ok_or(Error::Stale)?;
        let lease = &controller.lease;
        current(self.last_fence.as_ref() == Some(fence))?;
        current(fence.controller.as_ref() == Some(lease) && controller.fence_id == fence.fence_id)?;
        current(self.execution_active(a, now))?;
        current(now >= lease.issued_at_ms && a.covers_deadline(now, lease.expires_at_ms))?;
        current(lease.principal_id == a.principal)
    }

    fn replay_watch(&self, a: &Access, watch: &WatchSubscription, now: u64) -> Result<()> {
        self.authorize_watch(a, watch.subscription_id, now)?;
        current(
            self.viewers.get(&watch.subscription_id) == Some(watch)
                && watch.expires_at_ms <= a.valid_until_ms,
        )
    }

    fn replay_resize(&self, a: &Access, resize: &ResizeCommand, now: u64) -> Result<()> {
        let lease = self.active(a, resize.lease.lease_id, resize.lease.generation, now)?;
        self.auth(a, self.terminal(), self.session_revision, true, now)?;
        let command_digest = digest(resize)?;
        current(lease == &resize.lease && resize.sequence == self.resize_sequence)?;
        current(a.covers_deadline(now, resize.deadline_ms))?;
        current(
            self.resize_digests.get(&resize.operation_id)
                == Some(&(command_digest, resize.sequence, None)),
        )
    }

    fn replay_capture(&self, a: &Access, capture: &draft::Capture, now: u64) -> Result<()> {
        self.auth(a, self.terminal(), self.session_revision, true, now)?;
        if !a.watch {
            return Err(Error::Forbidden);
        }
        self.active(a, capture.lease_id, capture.controller_generation, now)?;
        let state = self
            .captures
            .get(&capture.capture_request_id)
            .ok_or(Error::Stale)?;
        current(!state.invalidated && state.snapshot_id.is_none())?;
        current(
            state.request == *capture && self.unknown.contains(&capture.uncertain_operation_id),
        )?;
        current(a.covers_deadline(now, capture.expires_at_ms))
    }
}
