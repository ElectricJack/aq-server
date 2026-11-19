//! Which preparation contract family an artifact names.
use crate::{ArtifactRef, preparation_refresh_v1 as refresh, preparation_v2 as frozen};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Frozen,
    Refresh,
}

/// `exact` matches the whole ref, as a new document must. Otherwise only id and version
/// match, so a proposal recorded under an earlier digest of its contract stays readable:
/// the long contracts changed digest when their lease ceiling was removed (migration
/// 0149 keys the SQL owners on the id the same way).
pub fn family(contract: &ArtifactRef, exact: bool) -> Option<Family> {
    let same = |known: ArtifactRef| match exact {
        true => known == *contract,
        false => known.id == contract.id && known.version == contract.version,
    };
    if frozen::contract_artifact().is_ok_and(same)
        || frozen::long_contract_artifact().is_ok_and(same)
    {
        Some(Family::Frozen)
    } else if refresh::contract_artifact().is_ok_and(same)
        || refresh::long_contract_artifact().is_ok_and(same)
    {
        Some(Family::Refresh)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_contract_under_an_earlier_digest_is_still_its_family() {
        let mut earlier = refresh::long_contract_artifact().unwrap();
        earlier.digest = crate::Digest::of(b"an earlier long schema");
        assert_eq!(family(&earlier, false), Some(Family::Refresh));
        assert_eq!(
            family(&earlier, true),
            None,
            "a new document names the exact contract"
        );
        earlier.id = crate::Text::new("aq.admission.unknown-contract").unwrap();
        assert_eq!(family(&earlier, false), None);
        let current = frozen::contract_artifact().unwrap();
        assert_eq!(family(&current, true), Some(Family::Frozen));
    }
}
