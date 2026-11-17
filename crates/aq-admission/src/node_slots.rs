//! Spec 0015 (a worker node runs several agents): the slot a native request is for, carried
//! beside the pinned transport documents (`transport.rs` is a pinned compatibility source, so
//! `Poll` and `Register` keep their bytes). Both values travel as HTTP request headers, like
//! the delivery nonce.
use crate::{Invalid, Result};

/// On `POST /v1/worker/profiled/poll`: the 1-based slot of the node this poll is for. An offer
/// in the reply is for this slot only, and `after_offer` pages this slot's undelivered offers.
/// Absent = slot 1 (a worker that runs one attempt per node).
pub const SLOT_HEADER: &str = "aq-node-slot";
/// On `POST /v1/worker/profiled/register`: the number of slots this node runs under this boot,
/// as the server's control reply told it. Present = a slot-aware worker: one registration and
/// boot per service lifetime, every poll names its slot. Absent = a worker that registers per
/// attempt and runs one attempt per node.
pub const SLOTS_HEADER: &str = "aq-node-slots";
/// A format bound on a slot count or index, not a limit: the limit is the server's Setting
/// for the node.
pub const MAX_NODE_SLOTS: u32 = 1024;

/// One header value: decimal `1..=MAX_NODE_SLOTS`, no sign, space or leading zero.
pub fn parse(value: &str) -> Result<u32> {
    let canonical = !value.is_empty()
        && value.len() <= 4
        && value.bytes().all(|b| b.is_ascii_digit())
        && !value.starts_with('0');
    match value.parse::<u32>() {
        Ok(n) if canonical && n <= MAX_NODE_SLOTS => Ok(n),
        _ => Err(Invalid("node slot")),
    }
}
/// The slot a poll is for: the header's value, or slot 1 when the worker names none.
pub fn poll_slot(header: Option<&str>) -> Result<u32> {
    header.map_or(Ok(1), parse)
}
/// The slot count a registration lists; `None` for a worker that is not slot-aware.
pub fn registered_slots(header: Option<&str>) -> Result<Option<u32>> {
    header.map(parse).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_absent_slot_is_slot_one_and_values_are_canonical_and_bounded() {
        assert_eq!(poll_slot(None).unwrap(), 1);
        assert_eq!(poll_slot(Some("3")).unwrap(), 3);
        assert_eq!(registered_slots(None).unwrap(), None);
        assert_eq!(registered_slots(Some("1024")).unwrap(), Some(1024));
        for bad in [
            "", "0", "01", "+1", " 1", "1 ", "1025", "-1", "1.0", "99999",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
            assert!(poll_slot(Some(bad)).is_err());
            assert!(registered_slots(Some(bad)).is_err());
        }
    }
}
