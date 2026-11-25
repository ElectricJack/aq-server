//! Stored phase execution reads, validation and fact construction.
//! Callers supply authenticated records and keep permission and transaction fences.
use crate::hot::HotState;
use aq_admission::{
    ArtifactRef, Digest, LaunchBinding, Text, control::Versions, transport::ArtifactReceipt,
};
use aq_worker_protocol::TransferManifest;
use serde::Serialize;
use sqlx::{Postgres, Row, Transaction};

#[derive(Debug)]
pub enum Error {
    Invalid,
    Encoding,
    Database(sqlx::Error),
}
fn canonical(value: &impl Serialize) -> Result<Vec<u8>, Error> {
    let mut value = serde_json::to_value(value).map_err(|_| Error::Encoding)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| Error::Encoding)
}
fn digest(value: &impl Serialize) -> Result<Digest, Error> {
    Ok(Digest::of(&canonical(value)?))
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool, Error> {
    Ok(canonical(a)? == canonical(b)?)
}
fn decode<T: serde::de::DeserializeOwned + Serialize>(
    bytes: &[u8],
    expected: &str,
) -> Result<T, Error> {
    if bytes.is_empty() || bytes.len() > 131072 || Digest::of(bytes).as_str() != expected {
        return Err(Error::Invalid);
    }
    let v: T = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
    if canonical(&v)? != bytes {
        return Err(Error::Invalid);
    }
    Ok(v)
}

pub async fn history_storage_available(tx: &mut Transaction<'_, Postgres>) -> Result<bool, Error> {
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('profiled_phase_execution_grants') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await
            .map_err(Error::Database)?;
    if present {
        return Ok(true);
    }
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut **tx)
            .await
            .map_err(Error::Database)?;
    if version.parse::<u32>().map_err(|_| Error::Invalid)? >= 32 {
        return Err(Error::Invalid);
    }
    Ok(false)
}

pub async fn load_hot_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
    accepted_versions: &Versions,
) -> Result<(HotState, Versions), Error> {
    let row=sqlx::query("SELECT state,record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE").bind(binding.attempt_id).fetch_one(&mut **tx).await.map_err(Error::Database)?;
    let hot: HotState = serde_json::from_value(row.get("state")).map_err(|_| Error::Invalid)?;
    hot.validate_against(binding).map_err(|_| Error::Invalid)?;
    let versions = Versions {
        record_revision: row.get::<i64, _>("record_revision") as u64,
        authority_revision: row.get::<i64, _>("authority_revision") as u64,
        grant_revision: row.get::<i64, _>("grant_revision") as u64,
    };
    versions
        .validate_after(accepted_versions)
        .map_err(|_| Error::Invalid)?;
    Ok((hot, versions))
}

pub fn source_artifact_receipts(source: &TransferManifest) -> Result<Vec<ArtifactReceipt>, Error> {
    let mut receipts = Vec::with_capacity(2);
    for (name, hash, size) in [
        (
            "manifest",
            source.manifest_digest.as_str(),
            source.manifest_bytes,
        ),
        (
            "bundle",
            source.bundle_digest.as_deref().ok_or(Error::Invalid)?,
            source.bundle_bytes,
        ),
    ] {
        let d: Digest =
            serde_json::from_value(serde_json::json!(hash)).map_err(|_| Error::Invalid)?;
        receipts.push(ArtifactReceipt {
            artifact: ArtifactRef {
                id: Text::new(format!("aq.source-{name}/{}", source.transfer_id))
                    .map_err(|_| Error::Invalid)?,
                version: Text::new("1").map_err(|_| Error::Invalid)?,
                digest: d.clone(),
            },
            bytes: size,
            digest: d,
        });
    }
    Ok(receipts)
}

pub fn require_artifact_receipts(
    expected: &[ArtifactReceipt],
    actual: &[ArtifactReceipt],
) -> Result<(), Error> {
    if expected.len() != actual.len()
        || expected
            .iter()
            .any(|e| !actual.iter().any(|a| same(e, a).unwrap_or(false)))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

pub fn require_runtime_capabilities(
    offer: &aq_admission::startup::NativeOffer,
    capabilities: [&[aq_session_protocol::Capability]; 2],
) -> Result<(), Error> {
    for caps in capabilities {
        aq_session_protocol::require_capabilities(&offer.offered.offer.required_capabilities, caps)
            .map_err(|_| Error::Invalid)?;
        aq_session_protocol::require_capabilities(
            &offer.offered.offer.manifest.harness.required_capabilities,
            caps,
        )
        .map_err(|_| Error::Invalid)?;
    }
    Ok(())
}

pub fn renewed_expiry(
    time: u64,
    previous_expiry: u64,
    maximum_lease_ms: u64,
    hard_deadline_ms: u64,
) -> Result<u64, Error> {
    let expiry = time
        .checked_add(maximum_lease_ms.min(120_000))
        .ok_or(Error::Invalid)?
        .min(hard_deadline_ms);
    if time >= previous_expiry || expiry <= previous_expiry {
        return Err(Error::Invalid);
    }
    Ok(expiry)
}

// Each profile keeps its own nominal request, receipt and fact types.
pub mod frozen {
    #[cfg(test)]
    const FIXTURE: &str =
        include_str!("../../../aq-phase-terminal-protocol/tests/data/preparation-v2.fixture.json");
    use aq_admission::preparation_v2 as phase;
    include!("profile.rs");
}
pub mod refresh {
    #[cfg(test)]
    const FIXTURE: &str = include_str!(
        "../../../aq-phase-terminal-protocol/tests/data/preparation-refresh-v1.fixture.json"
    );
    use aq_admission::preparation_refresh_v1 as phase;
    include!("profile.rs");
}
#[cfg(test)]
mod document_tests {
    use super::*;

    #[test]
    fn retained_documents_require_matching_digest_and_canonical_bytes() {
        let value = serde_json::json!({"b": 2, "a": 1});
        let bytes = canonical(&value).unwrap();
        assert_eq!(
            decode::<serde_json::Value>(&bytes, Digest::of(&bytes).as_str()).unwrap(),
            value
        );
        assert!(decode::<serde_json::Value>(&bytes, Digest::of(b"changed").as_str()).is_err());
        let pretty = serde_json::to_vec_pretty(&value).unwrap();
        assert!(decode::<serde_json::Value>(&pretty, Digest::of(&pretty).as_str()).is_err());
    }

    #[test]
    fn retained_document_byte_bound_is_inclusive() {
        let bytes = serde_json::to_vec(&"x".repeat(131070)).unwrap();
        assert_eq!(bytes.len(), 131072);
        assert!(decode::<String>(&bytes, Digest::of(&bytes).as_str()).is_ok());
        let oversized = serde_json::to_vec(&"x".repeat(131071)).unwrap();
        assert!(decode::<String>(&oversized, Digest::of(&oversized).as_str()).is_err());
        assert!(decode::<String>(&[], Digest::of(&[]).as_str()).is_err());
    }
}
#[cfg(test)]
mod renewal_tests {
    use super::*;

    #[test]
    fn renewal_respects_policy_lease_cap_and_original_hard_deadline() {
        assert_eq!(renewed_expiry(10, 20, 50, 1_000_000).unwrap(), 60);
        assert_eq!(renewed_expiry(10, 20, 200_000, 1_000_000).unwrap(), 120_010);
        assert_eq!(renewed_expiry(10, 20, 50, 40).unwrap(), 40);
    }

    #[test]
    fn renewal_cannot_revive_expired_grants_or_fail_to_extend_them() {
        assert!(renewed_expiry(20, 20, 50, 100).is_err());
        assert!(renewed_expiry(21, 20, 50, 100).is_err());
        assert!(renewed_expiry(10, 20, 10, 100).is_err());
        assert!(renewed_expiry(10, 20, 50, 20).is_err());
        assert!(renewed_expiry(u64::MAX - 10, u64::MAX, 50, u64::MAX).is_err());
    }
}
