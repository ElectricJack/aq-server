//! The consumer of aq-git's bounded `diff.patch` query: the exact binary diff
//! between two typed commit OIDs of one bound repository (a read; nothing here
//! approves, settles, moves a ref or recreates work).
//!
//! Contract (aq-git PR 24, `git_diff_binary_full_index_v1`):
//! - capability: `capabilities` lists the operation `diff.patch` and
//!   `limits.diff_patch_bytes`. A service without both is an older deployment
//!   and the answer is [`PatchError::Unsupported`], never a guess.
//! - request: `params {base, target, max_bytes?}`, `base` and `target` typed
//!   commit OIDs `{"algorithm":"sha1","hex":…}`.
//! - success is complete: `format`, `base`, `target`, `size` (raw bytes),
//!   `sha256` (lowercase hex over the raw bytes) and `patch {encoding:
//!   "base64", value}` (standard padded base64). Raw size is at most
//!   [`MAX_PATCH_BYTES`].
//! - refusals: `limit_exceeded` and `object_unavailable`.
//!
//! Bytes are returned only after the format, both OIDs, the size bound, the
//! decoded length and the digest were checked against what was asked for.
use crate::protocol::{MAX_RESPONSE, exchange_within};
use crate::source_checks::hex40;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use uuid::Uuid;

/// The operation name in aq-git's `capabilities.operations`.
pub const OPERATION: &str = "diff.patch";
pub const FORMAT: &str = "git_diff_binary_full_index_v1";
/// The most raw patch bytes aq-git serves, whatever a limit says.
pub const MAX_PATCH_BYTES: u64 = 4_194_304;
/// How long one aq-git exchange may take.
const WAIT: Duration = Duration::from_secs(20);

/// The repository a patch is read from: aq-git's repository id and the
/// binding generation recorded with the retained work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Repository {
    pub repository_id: Uuid,
    pub binding_generation: u64,
}

/// Commit endpoints for the patch, under one repository binding.
#[derive(Clone, Copy)]
pub struct CommitRange<'a> {
    pub base: &'a str,
    pub target: &'a str,
}
impl CommitRange<'_> {
    fn matches(self, result: &Value) -> bool {
        oid_hex(&result["base"]) == Some(self.base)
            && oid_hex(&result["target"]) == Some(self.target)
    }
    fn is_valid(self) -> bool {
        hex40(self.base) && hex40(self.target)
    }
}

/// A verified patch: every field was checked against the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub base: String,
    pub target: String,
    /// Raw byte count; `bytes.len()`.
    pub size: u64,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// Why no patch bytes were returned. Each is a statement about aq-git or
/// the request, not a judgement about the work.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    /// aq-git does not offer `diff.patch` (an older deployment).
    #[error("aq-git does not offer diff.patch")]
    Unsupported,
    /// The patch is larger than the bound that applied.
    #[error("the patch exceeds {limit} bytes")]
    LimitExceeded { limit: u64 },
    /// aq-git does not hold the base or target commit.
    #[error("aq-git does not hold the base or target commit")]
    ObjectUnavailable,
    /// The repository binding is no longer the one the work was retained under.
    #[error("the repository binding moved")]
    BindingMoved,
    /// aq-git did not answer in time or was unreachable.
    #[error("aq-git did not answer")]
    Unavailable,
    /// The answer broke the contract: wrong format, OIDs, size or digest.
    #[error("aq-git answered outside the diff.patch contract")]
    Protocol,
}

fn typed(hex: &str) -> Value {
    json!({"algorithm": "sha1", "hex": hex})
}
/// The hex of a typed OID or the plain hex string an older shape uses.
fn oid_hex(value: &Value) -> Option<&str> {
    match value {
        Value::String(hex) => Some(hex),
        Value::Object(_) if value["algorithm"] == "sha1" => value["hex"].as_str(),
        _ => None,
    }
}
fn lowercase_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

async fn call(
    socket: &Path,
    repository: Option<Repository>,
    operation: &str,
    params: Value,
) -> Result<Value, PatchError> {
    let id = Uuid::new_v4();
    let mut request = json!({
        "protocol_version": 1, "request_id": id.to_string(), "operation": operation,
        "params": params, "deadline_ms": WAIT.as_millis() as u64,
    });
    if let Some(repository) = repository {
        request["repository_id"] = json!(repository.repository_id);
        request["expected_binding_generation"] = json!(repository.binding_generation);
    }
    let mut bytes = serde_json::to_vec(&request).map_err(|_| PatchError::Protocol)?;
    bytes.push(b'\n');
    let raw = exchange_within(socket, &bytes, WAIT + Duration::from_secs(2))
        .await
        .map_err(|e| match e {
            crate::DeliveryError::Protocol => PatchError::Protocol,
            _ => PatchError::Unavailable,
        })?;
    decode_reply(&raw, id)
}

fn decode_reply(raw: &[u8], id: Uuid) -> Result<Value, PatchError> {
    if raw.len() > MAX_RESPONSE {
        return Err(PatchError::Protocol);
    }
    let v: Value = serde_json::from_slice(raw).map_err(|_| PatchError::Protocol)?;
    if v["protocol_version"] != 1 || v["request_id"] != id.to_string() {
        return Err(PatchError::Protocol);
    }
    match v["ok"].as_bool() {
        Some(true) if v["result"].is_object() && v.get("error").is_none() => {
            Ok(v["result"].clone())
        }
        Some(false) => Err(refusal(&v["error"])),
        _ => Err(PatchError::Protocol),
    }
}

fn refusal(error: &Value) -> PatchError {
    match error["code"].as_str().unwrap_or_default() {
        "limit_exceeded" => PatchError::LimitExceeded {
            limit: error["details"]["limit"]
                .as_u64()
                .filter(|l| (1..=MAX_PATCH_BYTES).contains(l))
                .unwrap_or(MAX_PATCH_BYTES),
        },
        "object_unavailable" => PatchError::ObjectUnavailable,
        "binding_conflict" | "binding_generation_mismatch" | "stale_binding" => {
            PatchError::BindingMoved
        }
        "unsupported_operation" | "unknown_operation" | "operation_unsupported" => {
            PatchError::Unsupported
        }
        "overloaded" | "io_error" | "state_busy" => PatchError::Unavailable,
        _ => PatchError::Protocol,
    }
}

/// aq-git's advertised `limits.diff_patch_bytes`, or [`PatchError::Unsupported`]
/// when it does not list `diff.patch` with a usable limit.
pub async fn capability(socket: &Path) -> Result<u64, PatchError> {
    let result = call(socket, None, "capabilities", json!({})).await?;
    supported_limit(&result)
}
fn supported_limit(capabilities: &Value) -> Result<u64, PatchError> {
    let listed = capabilities["operations"]
        .as_array()
        .is_some_and(|ops| ops.iter().any(|op| op == OPERATION));
    match capabilities["limits"]["diff_patch_bytes"].as_u64() {
        Some(limit) if listed && limit > 0 => Ok(limit.min(MAX_PATCH_BYTES)),
        _ => Err(PatchError::Unsupported),
    }
}

/// The exact patch from `base` to `target` in `repository`, at most
/// `max_bytes` raw bytes (clamped to aq-git's advertised limit). Asks the
/// capability first, so an older aq-git yields [`PatchError::Unsupported`].
pub async fn read(
    socket: &Path,
    repository: Repository,
    range: CommitRange<'_>,
    max_bytes: u64,
) -> Result<Patch, PatchError> {
    if !range.is_valid() || max_bytes == 0 {
        return Err(PatchError::Protocol);
    }
    let limit = capability(socket).await?.min(max_bytes);
    let params =
        json!({"base": typed(range.base), "target": typed(range.target), "max_bytes": limit});
    let result = match call(socket, Some(repository), OPERATION, params).await {
        // The bound that applied is the one this side asked for.
        Err(PatchError::LimitExceeded { .. }) => return Err(PatchError::LimitExceeded { limit }),
        other => other?,
    };
    verify(&result, range, limit)
}

/// Checks one success result against the request before any byte is exposed.
fn verify(result: &Value, range: CommitRange<'_>, limit: u64) -> Result<Patch, PatchError> {
    let size = result["size"].as_u64().ok_or(PatchError::Protocol)?;
    let sha256 = result["sha256"].as_str().ok_or(PatchError::Protocol)?;
    let format_matches = result["format"] == FORMAT && result["patch"]["encoding"] == "base64";
    if ![
        format_matches,
        range.matches(result),
        lowercase_hex(sha256, 64),
    ]
    .into_iter()
    .all(|valid| valid)
    {
        return Err(PatchError::Protocol);
    }
    // An oversized claim is refused before decoding the (larger) text.
    if size > limit {
        return Err(PatchError::LimitExceeded { limit });
    }
    let bytes = decode_patch(&result["patch"], size, sha256)?;
    Ok(Patch {
        base: range.base.into(),
        target: range.target.into(),
        size,
        sha256: sha256.into(),
        bytes,
    })
}

fn decode_patch(patch: &Value, size: u64, sha256: &str) -> Result<Vec<u8>, PatchError> {
    let text = patch["value"].as_str().ok_or(PatchError::Protocol)?;
    // Padded base64 of `size` bytes has exactly this many characters.
    if text.len() as u64 != size.div_ceil(3) * 4 {
        return Err(PatchError::Protocol);
    }
    let bytes = STANDARD.decode(text).map_err(|_| PatchError::Protocol)?;
    if bytes.len() as u64 != size || hex::encode(Sha256::digest(&bytes)) != sha256 {
        return Err(PatchError::Protocol);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: &str = "957a001e392c4405b00ea1f11a1e621b19842447";
    const HEAD: &str = "105c94ad4a258ab7b943f14bbd31db935ec9af5e";
    const PATCH: &[u8] = b"diff --git a/f b/f\nindex 1111111..2222222 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n";
    const RANGE: CommitRange<'_> = CommitRange {
        base: BASE,
        target: HEAD,
    };
    fn answer(bytes: &[u8]) -> Value {
        json!({
            "format": FORMAT, "base": typed(BASE), "target": typed(HEAD),
            "size": bytes.len(), "sha256": hex::encode(Sha256::digest(bytes)),
            "patch": {"encoding": "base64", "value": STANDARD.encode(bytes)},
        })
    }
    #[test]
    fn a_complete_answer_yields_the_exact_bytes() {
        let patch = verify(&answer(PATCH), RANGE, 4096).unwrap();
        assert_eq!(patch.bytes, PATCH);
        assert_eq!(patch.size, PATCH.len() as u64);
        assert_eq!(patch.sha256, hex::encode(Sha256::digest(PATCH)));
        // Binary content survives, including a zero length patch.
        let binary: Vec<u8> = (0..=255).collect();
        assert_eq!(verify(&answer(&binary), RANGE, 4096).unwrap().bytes, binary);
        assert!(verify(&answer(b""), RANGE, 4096).unwrap().bytes.is_empty());
    }
    #[test]
    fn every_deviation_from_the_request_or_the_digest_refuses_the_bytes() {
        let ok = answer(PATCH);
        let with = |f: &dyn Fn(&mut Value)| {
            let mut v = ok.clone();
            f(&mut v);
            verify(&v, RANGE, 4096)
        };
        assert_eq!(
            with(&|v| v["format"] = json!("other")),
            Err(PatchError::Protocol)
        );
        assert_eq!(
            with(&|v| v["target"] = typed(BASE)),
            Err(PatchError::Protocol)
        );
        assert_eq!(
            with(&|v| v["base"] = typed(HEAD)),
            Err(PatchError::Protocol)
        );
        assert_eq!(
            with(&|v| v["sha256"] = json!("0".repeat(64))),
            Err(PatchError::Protocol)
        );
        assert_eq!(
            with(&|v| v["sha256"] = json!(v["sha256"].as_str().unwrap().to_uppercase())),
            Err(PatchError::Protocol)
        );
        assert_eq!(
            with(&|v| v["size"] = json!(PATCH.len() + 1)),
            Err(PatchError::Protocol)
        );
        assert_eq!(
            with(&|v| v["patch"]["encoding"] = json!("hex")),
            Err(PatchError::Protocol)
        );
        // Altered bytes with the original digest and size.
        let mut altered = PATCH.to_vec();
        altered[0] ^= 1;
        assert_eq!(
            with(&|v| v["patch"]["value"] = json!(STANDARD.encode(&altered))),
            Err(PatchError::Protocol)
        );
        // Unpadded base64 is not the contract's encoding.
        assert_eq!(
            with(&|v| v["patch"]["value"] = json!(STANDARD.encode(PATCH).trim_end_matches('='))),
            Err(PatchError::Protocol)
        );
        assert_eq!(
            with(&|v| {
                v.as_object_mut().unwrap().remove("patch");
            }),
            Err(PatchError::Protocol)
        );
    }
    #[test]
    fn a_patch_over_the_bound_is_refused_before_it_is_decoded() {
        assert_eq!(
            verify(&answer(PATCH), RANGE, PATCH.len() as u64 - 1),
            Err(PatchError::LimitExceeded {
                limit: PATCH.len() as u64 - 1
            })
        );
        assert!(verify(&answer(PATCH), RANGE, PATCH.len() as u64).is_ok());
    }
    #[test]
    fn the_capability_needs_the_operation_and_a_limit() {
        let caps = |ops: Value, limits: Value| json!({"operations": ops, "limits": limits});
        assert_eq!(
            supported_limit(&caps(
                json!(["diff", OPERATION]),
                json!({"diff_patch_bytes": 4194304})
            )),
            Ok(MAX_PATCH_BYTES)
        );
        // A larger advertised limit never raises the contract maximum.
        assert_eq!(
            supported_limit(&caps(
                json!([OPERATION]),
                json!({"diff_patch_bytes": 1u64 << 40})
            )),
            Ok(MAX_PATCH_BYTES)
        );
        assert_eq!(
            supported_limit(&caps(json!([OPERATION]), json!({"diff_patch_bytes": 1000}))),
            Ok(1000)
        );
        for older in [
            caps(
                json!(["diff", "contains"]),
                json!({"query_response_bytes": 65536}),
            ),
            caps(json!(["diff", OPERATION]), json!({})),
            caps(json!(["diff"]), json!({"diff_patch_bytes": 4096})),
            caps(json!([OPERATION]), json!({"diff_patch_bytes": 0})),
            json!({}),
        ] {
            assert_eq!(supported_limit(&older), Err(PatchError::Unsupported));
        }
    }
    #[test]
    fn refusals_map_to_the_contract_reasons() {
        let code = |c: &str| refusal(&json!({"code": c, "message": "m"}));
        assert_eq!(
            code("limit_exceeded"),
            PatchError::LimitExceeded {
                limit: MAX_PATCH_BYTES
            }
        );
        assert_eq!(code("object_unavailable"), PatchError::ObjectUnavailable);
        assert_eq!(code("unknown_operation"), PatchError::Unsupported);
        assert_eq!(code("overloaded"), PatchError::Unavailable);
        assert_eq!(code("something_else"), PatchError::Protocol);
    }
}
