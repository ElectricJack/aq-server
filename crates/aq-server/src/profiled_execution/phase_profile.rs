//! Exact parser/history dispatch. A selected parser never authenticates an owner.
use crate::error::AppError;
use aq_admission::preparation_family::{Family, family};
use aq_admission::{ArtifactRef, preparation_refresh_v1 as refresh, preparation_v2 as frozen};
use serde_json::Value;
use sqlx::{Postgres, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Profile {
    Frozen,
    Refresh,
}

#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("unsupported exact preparation profile".into())
}
fn malformed() -> AppError {
    AppError::BadRequest("invalid phase preparation document".into())
}
impl Profile {
    /// `exact` is false only for a proposal already recorded (see `family`).
    pub(super) fn for_contract(contract: &ArtifactRef, exact: bool) -> Result<Self, AppError> {
        match family(contract, exact) {
            Some(Family::Frozen) => Ok(Self::Frozen),
            Some(Family::Refresh) => Ok(Self::Refresh),
            None => Err(refused()),
        }
    }
    pub(super) fn from_proposal(value: &Value, exact: bool) -> Result<Self, AppError> {
        let contract: ArtifactRef =
            serde_json::from_value(value["negotiation"]["contract"].clone())
                .map_err(|_| malformed())?;
        Self::for_contract(&contract, exact)
    }
    fn from_document(bytes: &[u8]) -> Result<Self, AppError> {
        if bytes.len() > frozen::MAX_BYTES {
            return Err(AppError::PayloadTooLarge("phase document bound".into()));
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| malformed())?;
        Self::from_proposal(&value["body"], true)
    }
}

pub(super) enum ProposalRequest {
    Frozen(super::accepted_preparation::ProposalRequest),
    Refresh(super::accepted_refresh::ProposalRequest),
}
pub(super) fn decode_proposal(bytes: &[u8]) -> Result<ProposalRequest, AppError> {
    if bytes.len() > frozen::MAX_BYTES {
        return Err(AppError::PayloadTooLarge("phase document bound".into()));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| malformed())?;
    // The header only chooses the strict full decoder below. In particular it
    // cannot discard duplicate/unknown fields before authoritative validation.
    match Profile::from_proposal(&value, true)? {
        Profile::Frozen => {
            let request: super::accepted_preparation::ProposalRequest =
                serde_json::from_slice(bytes).map_err(|_| malformed())?;
            request.negotiation.validate().map_err(|_| refused())?;
            Ok(ProposalRequest::Frozen(request))
        }
        Profile::Refresh => {
            let request: super::accepted_refresh::ProposalRequest =
                serde_json::from_slice(bytes).map_err(|_| malformed())?;
            request.negotiation.validate().map_err(|_| refused())?;
            Ok(ProposalRequest::Refresh(request))
        }
    }
}
pub(super) enum AcceptRequest {
    Frozen(frozen::AcceptRequest),
    Refresh(refresh::AcceptRequest),
}
pub(super) fn decode_accept(bytes: &[u8]) -> Result<AcceptRequest, AppError> {
    match Profile::from_document(bytes)? {
        Profile::Frozen => match frozen::decode(bytes).map_err(|_| malformed())? {
            frozen::Document::Accept(value) => Ok(AcceptRequest::Frozen(*value)),
            _ => Err(malformed()),
        },
        Profile::Refresh => match refresh::decode(bytes).map_err(|_| malformed())? {
            refresh::Document::Accept(value) => Ok(AcceptRequest::Refresh(*value)),
            _ => Err(malformed()),
        },
    }
}
pub(super) enum PrepareRequest {
    Frozen(frozen::PrepareRequest),
    Refresh(refresh::PrepareRequest),
}
pub(super) fn decode_prepare(bytes: &[u8]) -> Result<PrepareRequest, AppError> {
    match Profile::from_document(bytes)? {
        Profile::Frozen => match frozen::decode(bytes).map_err(|_| malformed())? {
            frozen::Document::Prepare(value) => Ok(PrepareRequest::Frozen(*value)),
            _ => Err(malformed()),
        },
        Profile::Refresh => match refresh::decode(bytes).map_err(|_| malformed())? {
            refresh::Document::Prepare(value) => Ok(PrepareRequest::Refresh(*value)),
            _ => Err(malformed()),
        },
    }
}

pub(super) async fn for_attempt_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: uuid::Uuid,
) -> Result<Option<Profile>, AppError> {
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('profiled_v2_proposals') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await
            .map_err(|_| AppError::Internal)?;
    if !present {
        return Ok(None);
    }
    let value: Option<Value> = sqlx::query_scalar(
        "SELECT proposal FROM profiled_v2_proposals WHERE attempt_id=$1 FOR SHARE",
    )
    .bind(attempt)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?;
    value
        .as_ref()
        .map(|v| Profile::from_proposal(v, false))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture(refresh: bool) -> Value {
        if refresh {
            serde_json::from_str::<Value>(include_str!(
                "../../../aq-admission/artifacts/preparation-refresh-v1.json"
            ))
            .unwrap()["fixture"]
                .clone()
        } else {
            serde_json::from_str(include_str!(
                "../../../aq-admission/artifacts/preparation-v2.fixture.json"
            ))
            .unwrap()
        }
    }
    #[test]
    fn exact_profile_dispatch_retains_the_nominal_family_and_rejects_cross_profile_features() {
        for refresh in [false, true] {
            let fixture = fixture(refresh);
            let proposal = &fixture["proposal"];
            let request = json!({"identity":fixture["accept"]["original"]["identity"],"binding":proposal["original"]["offered"]["offer"]["binding"],"negotiation":proposal["negotiation"]});
            let bytes = serde_json::to_vec(&request).unwrap();
            assert!(matches!(
                (refresh, decode_proposal(&bytes).unwrap()),
                (false, ProposalRequest::Frozen(_)) | (true, ProposalRequest::Refresh(_))
            ));
            let accept =
                serde_json::to_vec(&json!({"kind":"accept","body":fixture["accept"]})).unwrap();
            assert!(matches!(
                (refresh, decode_accept(&accept).unwrap()),
                (false, AcceptRequest::Frozen(_)) | (true, AcceptRequest::Refresh(_))
            ));
            let mut mixed = request.clone();
            mixed["negotiation"]["contract"] = if refresh {
                json!(frozen::contract_artifact().unwrap())
            } else {
                json!(refresh::contract_artifact().unwrap())
            };
            assert!(decode_proposal(&serde_json::to_vec(&mixed).unwrap()).is_err());
            let mut unknown = request.clone();
            unknown["negotiation"]["contract"]["digest"] =
                json!(aq_admission::Digest::of(b"unknown profile"));
            assert!(decode_proposal(&serde_json::to_vec(&unknown).unwrap()).is_err());
        }
    }
    #[test]
    fn header_probe_never_erases_duplicate_or_unknown_fields_before_full_decode() {
        let f = fixture(true);
        let request = json!({"identity":f["accept"]["original"]["identity"],"binding":f["proposal"]["original"]["offered"]["offer"]["binding"],"negotiation":f["proposal"]["negotiation"]});
        let raw = serde_json::to_string(&request).unwrap();
        let duplicate = raw.replacen(
            '{',
            &format!("{{\"negotiation\":{},", request["negotiation"]),
            1,
        );
        assert!(decode_proposal(duplicate.as_bytes()).is_err());
        let mut unknown = request;
        unknown["unqualified_profile_hint"] = json!(true);
        assert!(decode_proposal(&serde_json::to_vec(&unknown).unwrap()).is_err());
        let mut oversized = raw.into_bytes();
        oversized.resize(frozen::MAX_BYTES + 1, b' ');
        assert!(decode_proposal(&oversized).is_err());
    }
}
