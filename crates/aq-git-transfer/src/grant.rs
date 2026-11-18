use crate::{Loaded, proxy::send};
use aq_admission::{
    ObjectFormat, ObjectId,
    git_grant::{self, GitGrant, GitToken, OfferGitGrant},
    startup::NativeOffer,
};
use axum::{body::Body, http::StatusCode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use http_body_util::{BodyExt, Limited};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// The server's MAC of `(attempt, repository)` under its credential key.
pub type Mac = Arc<dyn Fn(Uuid, Uuid) -> Option<[u8; 32]> + Send + Sync>;

/// A grant could not be made or delivered; the offer is refused.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// `aqg_<attempt>_<mac>`: the attempt in clear, bound to one repository by the
/// server's key. Derived, so redelivery needs no stored secret.
pub fn token(mac: &Mac, attempt_id: Uuid, repository_id: Uuid) -> Option<GitToken> {
    let mac = mac(attempt_id, repository_id)?;
    GitToken::new(format!(
        "aqg_{}_{}",
        attempt_id.simple(),
        URL_SAFE_NO_PAD.encode(mac)
    ))
    .ok()
}
/// The attempt a token names, if its MAC binds it to `repository_id`.
pub fn verify(mac: &Mac, token: &str, repository_id: Uuid) -> Option<Uuid> {
    let rest = token.strip_prefix("aqg_")?;
    let attempt = Uuid::try_parse(rest.get(..32)?).ok()?;
    if rest.get(32..33)? != "_" {
        return None;
    }
    let presented = URL_SAFE_NO_PAD.decode(rest.get(33..)?).ok()?;
    let expected = mac(attempt, repository_id)?;
    bool::from(presented.ct_eq(&expected)).then_some(attempt)
}
/// The push's aq-git operation (`AQ-Operation`, read back by `push.receipt`):
/// one per attempt, so a retried push with the same lease is the same one.
pub fn push_operation(attempt_id: Uuid) -> Uuid {
    Uuid::new_v5(&attempt_id, b"aq.git-push/v1")
}

/// The grant's server-held facts, kept with the attempt's original offer.
/// The token is not stored: it is derived again on every delivery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredGrant {
    pub repository_id: Uuid,
    pub base: ObjectId,
    pub task_ref: String,
    pub lease: Option<ObjectId>,
    pub main_ref: Option<String>,
    pub operation_id: Uuid,
}
impl StoredGrant {
    /// The wire grant for `attempt_id`, with its derived token.
    pub fn wire(&self, mac: &Mac, attempt_id: Uuid) -> Option<GitGrant> {
        Some(GitGrant {
            schema_version: 1,
            endpoint: git_grant::endpoint(self.repository_id),
            repository_id: self.repository_id,
            base: self.base.clone(),
            task_ref: self.task_ref.clone(),
            lease: self.lease.clone(),
            main_ref: self.main_ref.clone(),
            operation_id: self.operation_id,
            token: token(mac, attempt_id, self.repository_id)?,
        })
    }
}
/// The headers aq-git trusts, all from the server-held grant.
pub fn grant_headers(grant: &StoredGrant, max_push_bytes: u64) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("aq-repository", grant.repository_id.to_string()),
        ("aq-base", grant.base.hex.clone()),
        ("aq-task-ref", grant.task_ref.clone()),
    ];
    if let Some(lease) = &grant.lease {
        headers.push(("aq-task-ref-lease", lease.hex.clone()));
    }
    if let Some(main) = &grant.main_ref {
        headers.push(("aq-main-ref", main.clone()));
    }
    headers.push(("aq-max-push-bytes", max_push_bytes.to_string()));
    headers.push(("aq-operation", grant.operation_id.to_string()));
    headers
}

/// What the server knows about an attempt at offer time.
pub struct OfferFacts {
    pub attempt_id: Uuid,
    pub task_id: Uuid,
    pub repository_id: Uuid,
    pub base: ObjectId,
    /// The default branch: the followed ref or the publication target.
    pub main_ref: Option<String>,
}
/// The offer's grant. Its lease is the task branch's head as aq-git
/// advertises it to this grant now, or absent.
pub async fn offer_grant(
    loaded: &Loaded,
    mac: &Mac,
    facts: OfferFacts,
) -> Result<StoredGrant, Refused> {
    if facts.base.format != ObjectFormat::Sha1 {
        return Err(Refused("git grant base is not SHA-1"));
    }
    let mut grant = StoredGrant {
        repository_id: facts.repository_id,
        base: facts.base,
        task_ref: git_grant::task_ref(facts.task_id),
        lease: None,
        main_ref: facts
            .main_ref
            .filter(|r| r.starts_with("refs/heads/") && !r.starts_with(git_grant::TASK_REF_PREFIX)),
        operation_id: push_operation(facts.attempt_id),
    };
    grant.lease = advertised_lease(loaded, &grant).await?;
    grant
        .wire(mac, facts.attempt_id)
        .ok_or(Refused("git grant token"))?
        .validate_for_task(facts.task_id)
        .map_err(|_| Refused("git grant shape"))?;
    Ok(grant)
}
/// The grants of the offers on a poll page, for the reply's top-level
/// `git_grants` field (`aq_admission::git_grant::POLL_REPLY_FIELD`).
pub async fn page_grants(
    pool: &sqlx::PgPool,
    mac: &Mac,
    offers: &[NativeOffer],
) -> Result<Vec<OfferGitGrant>, Refused> {
    let mut grants = Vec::new();
    for offer in offers {
        let attempt_id = offer.offered.offer.binding.attempt_id;
        let stored: Option<Option<serde_json::Value>> = sqlx::query_scalar(
            "SELECT graph_task_observation->'git' FROM profiled_launches WHERE attempt_id=$1",
        )
        .bind(attempt_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| Refused("git grant read"))?;
        let Some(stored) = stored.flatten().filter(|v| !v.is_null()) else {
            continue;
        };
        let stored: StoredGrant =
            serde_json::from_value(stored).map_err(|_| Refused("stored git grant"))?;
        let grant = stored
            .wire(mac, attempt_id)
            .ok_or(Refused("git grant token"))?;
        grant
            .validate_for_task(offer.offered.offer.manifest.bindings.task_id)
            .map_err(|_| Refused("git grant shape"))?;
        grants.push(OfferGitGrant { attempt_id, grant });
    }
    Ok(grants)
}

async fn advertised_lease(
    loaded: &Loaded,
    grant: &StoredGrant,
) -> Result<Option<ObjectId>, Refused> {
    let mut request = axum::http::Request::get(format!(
        "/{}/info/refs?service=git-upload-pack",
        grant.repository_id
    ))
    .header(axum::http::header::HOST, "aq-git")
    .body(Body::empty())
    .map_err(|_| Refused("git advertisement request"))?;
    crate::proxy::set_grant(request.headers_mut(), grant, loaded.config.max_push_bytes)
        .map_err(|_| Refused("git grant header"))?;
    let response = send(loaded, request)
        .await
        .map_err(|_| Refused("aq-git git socket unavailable"))?;
    if response.status() != StatusCode::OK {
        return Err(Refused("aq-git refused the git advertisement"));
    }
    // An advertisement of the main and task refs is small; bound it anyway.
    let bytes = Limited::new(response.into_body(), 1_048_576)
        .collect()
        .await
        .map_err(|_| Refused("git advertisement read"))?
        .to_bytes();
    let refs = advertisement(&bytes).ok_or(Refused("git advertisement shape"))?;
    Ok(refs
        .into_iter()
        .find(|(name, _)| *name == grant.task_ref)
        .map(|(_, hex)| ObjectId {
            format: ObjectFormat::Sha1,
            hex,
        }))
}
/// `(ref, sha1)` pairs of a protocol v0 smart-HTTP ref advertisement.
pub fn advertisement(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let mut refs = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let len = usize::from_str_radix(std::str::from_utf8(rest.get(..4)?).ok()?, 16).ok()?;
        if len == 0 {
            rest = &rest[4..];
            continue;
        }
        let line = rest.get(4..len)?;
        rest = &rest[len..];
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if line.starts_with(b"# service=") {
            continue;
        }
        let line = std::str::from_utf8(line.split(|b| *b == 0).next()?).ok()?;
        let (oid, name) = line.split_once(' ')?;
        if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        if oid.bytes().any(|b| b != b'0') && name != "capabilities^{}" {
            refs.push((name.to_owned(), oid.to_ascii_lowercase()));
        }
    }
    Some(refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) fn mac(key: u8) -> Mac {
        Arc::new(move |a: Uuid, r: Uuid| {
            let mut out = [key; 32];
            for (i, b) in a.as_bytes().iter().chain(r.as_bytes()).enumerate() {
                out[i % 32] ^= b;
            }
            Some(out)
        })
    }
    #[test]
    fn token_names_one_attempt_and_one_repository() {
        let key = mac(7);
        let (attempt, repository) = (Uuid::new_v4(), Uuid::new_v4());
        let t = token(&key, attempt, repository).unwrap();
        assert_eq!(verify(&key, t.expose(), repository), Some(attempt));
        assert_eq!(verify(&key, t.expose(), Uuid::new_v4()), None);
        assert_eq!(verify(&mac(8), t.expose(), repository), None);
        // Another attempt's id with this attempt's MAC is refused.
        let other = Uuid::new_v4().simple().to_string();
        let forged = t
            .expose()
            .replacen(&attempt.simple().to_string(), &other, 1);
        assert_eq!(verify(&key, &forged, repository), None);
        assert_eq!(verify(&key, "aqg_", repository), None);
    }
    #[test]
    fn advertisement_lists_refs_and_skips_service_lines() {
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let pkt = |s: &str| format!("{:04x}{s}", s.len() + 4);
        let bytes = format!(
            "{}0000{}{}0000",
            pkt("# service=git-upload-pack\n"),
            pkt(&format!("{a} refs/heads/main\0side-band-64k\n")),
            pkt(&format!("{b} refs/heads/aq/x\n"))
        );
        assert_eq!(
            advertisement(bytes.as_bytes()).unwrap(),
            [
                ("refs/heads/main".to_owned(), a),
                ("refs/heads/aq/x".to_owned(), b)
            ]
        );
        let empty = format!(
            "{}0000{}0000",
            pkt("# service=git-upload-pack\n"),
            pkt(&format!(
                "{} capabilities^{{}}\0ofs-delta\n",
                "0".repeat(40)
            ))
        );
        assert!(advertisement(empty.as_bytes()).unwrap().is_empty());
        assert!(advertisement(b"zzzz").is_none());
    }
    #[test]
    fn grant_headers_come_from_the_grant() {
        let grant = StoredGrant {
            repository_id: Uuid::from_u128(1),
            base: ObjectId {
                format: ObjectFormat::Sha1,
                hex: "a".repeat(40),
            },
            task_ref: git_grant::task_ref(Uuid::from_u128(2)),
            lease: None,
            main_ref: Some("refs/heads/main".into()),
            operation_id: push_operation(Uuid::from_u128(3)),
        };
        let names: Vec<_> = grant_headers(&grant, 9).into_iter().map(|h| h.0).collect();
        assert_eq!(
            names,
            [
                "aq-repository",
                "aq-base",
                "aq-task-ref",
                "aq-main-ref",
                "aq-max-push-bytes",
                "aq-operation"
            ]
        );
    }
}
