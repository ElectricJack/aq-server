//! Whether a granted launch closure proves that its harness never started (moved out of the
//! aq-server lib; `granted_closure.rs` itself is a pinned compatibility source).
use crate::granted_closure::{Evidence, StoppedObservation};
use crate::transport::{ProcessOutcome, ResultReport, StopReason};

// Here and not in `transport.rs`, which is a pinned compatibility source.
impl ResultReport {
    /// A typed interruption on the authenticated, exact stopped receipt.
    /// Claims and tool text do not establish this fact.
    pub fn platform_interrupted(&self) -> bool {
        matches!(self.outcome, ProcessOutcome::Failed)
            && matches!(self.stop.reason, StopReason::GuardianFailure)
    }
}

impl Evidence {
    /// Whether the worker's retained closure evidence shows that no harness pane
    /// ever existed. Pure; the closure route already verified its correlation.
    pub fn proves_never_started(&self) -> bool {
        match self {
            Evidence::Suppressed { .. } => true,
            Evidence::HostStopped { proof, .. } => paneless(&proof.observation),
        }
    }
}
/// A host whose stopped proof shows that no process ever ran under it: no
/// pane root status, no reaped process, and a kernel wait that sees no
/// children. This is the "host started, harness never launched" case (the
/// guardian stopped before its gate created a pane). A host that created a
/// pane always reaped at least its tmux processes, so any pane fails this
/// rule and the attempt stays held for an operator.
fn paneless(o: &StoppedObservation) -> bool {
    o.kernel_wait_echild && o.root_status.is_none() && o.reaped_count == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use uuid::Uuid;
    fn observation(pane: bool) -> StoppedObservation {
        let session = json!({"kind":"remote","node_id":Uuid::new_v4(),"boot_id":Uuid::new_v4(),"session_id":Uuid::new_v4(),"attempt_id":Uuid::new_v4(),"assignment_generation":1});
        let guardian = json!({"intent":{"launch_id":Uuid::new_v4(),"session":session,"manifest_digest":"a".repeat(64),"configuration_digest":"b".repeat(64)},"process":{"pid":10,"start_ticks":5},"nonce":Uuid::new_v4(),"machine_boot_id":Uuid::new_v4(),"pid_namespace":"pid:[4026531836]"});
        let p = pane.then(|| json!({"server":{"pid":11,"start_ticks":6},"pane":{"pid":12,"start_ticks":7},"pane_id":"%1","session_name":"aq"}));
        serde_json::from_value(json!({"guardian":guardian,"phase":"stopped","pane":p,"permit_sequence":1,"permit_deadline_boot_ms":1,"observed_boot_ms":1,"reaped_count":u64::from(pane),"kernel_wait_echild":true,"root_status":if pane {json!(0)} else {Value::Null},"stop_reason":null})).unwrap()
    }
    #[test]
    fn only_a_host_stopped_before_any_pane_proves_nonstart() {
        let none = observation(false);
        none.validate().unwrap();
        assert!(paneless(&none));
        let ran = observation(true);
        ran.validate().unwrap();
        assert!(
            !paneless(&ran),
            "a pane that existed may have run the harness"
        );
        let mut reaped = observation(false);
        reaped.reaped_count = 1;
        assert!(!paneless(&reaped));
        // A pane that ran and was reaped, even without a root status, holds.
        let mut paned = observation(true);
        paned.root_status = None;
        assert!(!paneless(&paned));
        // Children still waited on (no ECHILD) is not a stop proof of nothing.
        let mut waiting = observation(false);
        waiting.kernel_wait_echild = false;
        assert!(!paneless(&waiting));
    }
}
