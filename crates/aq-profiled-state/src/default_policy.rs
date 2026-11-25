//! Pure default completion policy (moved out of the aq-server lib). Every
//! fact is server-owned evidence; the worker's report outcome or conclusion
//! is never sufficient on its own.
use serde::{Deserialize, Serialize};

pub const KIND: &str = "aq.default-completion.v1";
pub const MAX_BODY: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Required,
    NotRequired,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    schema_version: u32,
    kind: String,
    delivery: Delivery,
}
/// The retained policy body, or None when this policy does not apply.
pub fn parse(body: &str) -> Option<Delivery> {
    if body.len() > MAX_BODY {
        return None;
    }
    let b: Body = serde_json::from_str(body).ok()?;
    (b.schema_version == 1 && b.kind == KIND).then_some(b.delivery)
}

/// Facts the gate reads, gathered by the server.
pub use crate::result_facts::{CheckVerdict, DeliveryState};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acceptance {
    Accepted,
    /// Superseded, cancelled, revoked or not reported as a success.
    Refused,
    /// The stop proof has not been recorded yet.
    NotStopped,
}
pub struct Gate<'a> {
    pub acceptance: Acceptance,
    pub source_retained: bool,
    /// The retained source is unchanged, yet the worker's report claims changes.
    pub claimed_changes_missing: bool,
    /// A refused (Failed) report whose agent said `Outcome: blocked` (R1-M2).
    pub agent_blocked: bool,
    /// The lineage's base is past the handoff bound (R1-M1, R2-M1).
    pub lineage_unknown: bool,
    pub checks: &'a [CheckVerdict],
    pub delivery: Delivery,
    pub delivery_state: DeliveryState,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Complete,
    Rework(&'static str),
    Hold(&'static str),
}
/// Ordered gate: acceptance, retained source, required checks, delivery.
/// Source precedes checks because check evidence binds the retained source.
pub fn evaluate(g: &Gate<'_>) -> Verdict {
    match g.acceptance {
        Acceptance::Refused if g.agent_blocked => return Verdict::Rework("agent_blocked"),
        Acceptance::Refused => return Verdict::Rework("not_accepted"),
        Acceptance::NotStopped => return Verdict::Hold("not_stopped"),
        Acceptance::Accepted => {}
    }
    if !g.source_retained {
        return Verdict::Hold("source_not_retained");
    }
    if g.claimed_changes_missing {
        return Verdict::Rework("claimed_changes_missing");
    }
    if g.checks.contains(&CheckVerdict::Failed) {
        return Verdict::Rework("required_check_failed");
    }
    if g.checks.contains(&CheckVerdict::Missing) {
        return Verdict::Hold("required_check_evidence_missing");
    }
    if g.lineage_unknown {
        return Verdict::Hold("lineage_history_unknown");
    }
    delivery_verdict(g.delivery, g.delivery_state)
}
fn delivery_verdict(delivery: Delivery, state: DeliveryState) -> Verdict {
    if delivery == Delivery::NotRequired {
        return Verdict::Complete;
    }
    match state {
        DeliveryState::Published | DeliveryState::NoChange => Verdict::Complete,
        DeliveryState::Indeterminate => Verdict::Hold("delivery_indeterminate"),
        DeliveryState::Missing => Verdict::Hold("delivery_evidence_missing"),
        DeliveryState::Unpublished => Verdict::Rework("delivery_unpublished"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gate(checks: &[CheckVerdict]) -> Gate<'_> {
        Gate {
            acceptance: Acceptance::Accepted,
            source_retained: true,
            claimed_changes_missing: false,
            agent_blocked: false,
            lineage_unknown: false,
            checks,
            delivery: Delivery::Required,
            delivery_state: DeliveryState::Published,
        }
    }
    #[test]
    fn default_policy_gate_order() {
        use CheckVerdict::*;
        assert_eq!(evaluate(&gate(&[Passed])), Verdict::Complete);
        assert_eq!(evaluate(&gate(&[])), Verdict::Complete);
        assert_eq!(
            evaluate(&gate(&[Missing, Failed])),
            Verdict::Rework("required_check_failed")
        );
        assert_eq!(
            evaluate(&gate(&[Passed, Missing])),
            Verdict::Hold("required_check_evidence_missing")
        );
        let mut g = gate(&[Failed]);
        g.source_retained = false;
        assert_eq!(evaluate(&g), Verdict::Hold("source_not_retained"));
        g.acceptance = Acceptance::Refused;
        assert_eq!(evaluate(&g), Verdict::Rework("not_accepted"));
        let mut g = gate(&[Passed]);
        for (state, verdict) in [
            (
                DeliveryState::Missing,
                Verdict::Hold("delivery_evidence_missing"),
            ),
            (
                DeliveryState::Unpublished,
                Verdict::Rework("delivery_unpublished"),
            ),
            (
                DeliveryState::Indeterminate,
                Verdict::Hold("delivery_indeterminate"),
            ),
        ] {
            g.delivery_state = state;
            assert_eq!(evaluate(&g), verdict);
        }
        g.delivery = Delivery::NotRequired;
        assert_eq!(evaluate(&g), Verdict::Complete);
        // A retained source equal to the admitted base has nothing to deliver.
        g.delivery = Delivery::Required;
        g.delivery_state = DeliveryState::NoChange;
        assert_eq!(evaluate(&g), Verdict::Complete);
        g.checks = &[Failed];
        assert_eq!(evaluate(&g), Verdict::Rework("required_check_failed"));
        g.claimed_changes_missing = true;
        assert_eq!(evaluate(&g), Verdict::Rework("claimed_changes_missing"));
        g.acceptance = Acceptance::Refused;
        g.agent_blocked = true;
        assert_eq!(evaluate(&g), Verdict::Rework("agent_blocked"));
    }
    /// R2-M1: an unknown lineage holds only a report that would otherwise settle
    /// as success; a refused report or a failed check is reworked as normal.
    #[test]
    fn an_unknown_lineage_holds_only_what_would_otherwise_succeed() {
        use CheckVerdict::*;
        let mut g = gate(&[Passed]);
        g.lineage_unknown = true;
        assert_eq!(evaluate(&g), Verdict::Hold("lineage_history_unknown"));
        g.checks = &[Failed];
        assert_eq!(evaluate(&g), Verdict::Rework("required_check_failed"));
        g.acceptance = Acceptance::Refused;
        assert_eq!(evaluate(&g), Verdict::Rework("not_accepted"));
    }
    #[test]
    fn default_policy_body_is_exact() {
        assert_eq!(
            parse(
                r#"{"schema_version":1,"kind":"aq.default-completion.v1","delivery":"required"}"#
            ),
            Some(Delivery::Required)
        );
        for body in [
            r#"{"schema_version":2,"kind":"aq.default-completion.v1","delivery":"required"}"#,
            r#"{"schema_version":1,"kind":"other","delivery":"required"}"#,
            r#"{"schema_version":1,"kind":"aq.default-completion.v1","delivery":"required","x":1}"#,
            r#"{"schema_version":1,"kind":"aq.default-completion.v1"}"#,
        ] {
            assert_eq!(parse(body), None, "{body}");
        }
    }
}
