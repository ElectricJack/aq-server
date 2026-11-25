//! Owner-facing manual admission requests and reproducible engine request bytes.
use crate::{Error, Result, digest};
use aq_playbook_host::{model::canonical_bytes, runner::KeyedAdmissionRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    pub request_id: Uuid,
    pub grant_id: Uuid,
    pub grant_revision: i64,
    pub binding: String,
    pub expected_activation_revision: u64,
    pub entry: String,
    pub input: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdmissionStatus {
    pub request_id: Uuid,
    pub engine_key: String,
    pub status: String,
    pub receipt: Option<aq_playbook_host::runner::KeyedAdmissionReceipt>,
}

impl PrepareRequest {
    pub fn validate(&self) -> Result<()> {
        if self.request_id.is_nil() || self.grant_id.is_nil() {
            return Err(Error::Invalid);
        }
        let activation = (1..=i64::MAX as u64).contains(&self.expected_activation_revision);
        if self.grant_revision <= 0 || !activation {
            return Err(Error::Invalid);
        }
        let valid_name = |name: &str| (1..=512).contains(&name.len()) && !name.contains('\0');
        if !valid_name(&self.binding) || !valid_name(&self.entry) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
pub fn replay_admission(
    row: &sqlx::postgres::PgRow,
    id: Uuid,
    key: String,
    digest: &str,
) -> Result<AdmissionStatus> {
    if row.get::<String, _>("request_digest") != digest {
        return Err(Error::Conflict);
    }
    let receipt: Option<Value> = row.get("receipt");
    Ok(AdmissionStatus {
        request_id: id,
        engine_key: key,
        status: row.get("status"),
        receipt: receipt.map(serde_json::from_value).transpose()?,
    })
}
pub fn validate_receipt(
    row: &sqlx::postgres::PgRow,
    receipt: &aq_playbook_host::runner::KeyedAdmissionReceipt,
) -> Result<()> {
    let stored_key = row.get::<String, _>("engine_key");
    let stored_digest = row.get::<String, _>("request_digest");
    if (stored_key.as_str(), stored_digest.as_str())
        != (receipt.key.as_str(), receipt.request_digest.as_str())
    {
        return Err(Error::Conflict);
    }
    if receipt.activation_revision != row.get::<i64, _>("expected_activation_revision") as u64 {
        return Err(Error::Conflict);
    }
    Ok(())
}

pub struct EncodedAdmission {
    pub bytes: Vec<u8>,
    pub digest: String,
}
impl EncodedAdmission {
    pub fn new(request: &KeyedAdmissionRequest) -> Result<Self> {
        let bytes = canonical_bytes(request).map_err(|_| Error::Invalid)?;
        if bytes.len() > 65536 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            bytes,
            digest: digest(request)?,
        })
    }
}
