//! Spec 0016 §5.2: an attempt's grant to fetch from and push to central aq-git
//! through the server's `/v1/git/<repository>` proxy. The server derives every
//! value here; validation is structural and grants nothing by itself.
use crate::*;

/// The only task-ref namespace a grant names (spec 0016 §3).
pub const TASK_REF_PREFIX: &str = "refs/heads/aq/";

/// The worker API path of a repository's git endpoint.
pub fn endpoint(repository_id: Uuid) -> String {
    format!("/v1/git/{repository_id}")
}

/// The task branch every attempt of `task_id` writes.
pub fn task_ref(task_id: Uuid) -> String {
    format!("{TASK_REF_PREFIX}{task_id}")
}

/// Bearer credential for one attempt. Never printed by `Debug`.
#[derive(Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct GitToken(#[schemars(length(min = 32, max = 256))] String);
impl GitToken {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let token = Self(value.into());
        token.validate()?;
        Ok(token)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    fn validate(&self) -> Result<()> {
        require(
            self.0.starts_with("aqg_")
                && (32..=256).contains(&self.0.len())
                && self.0.bytes().all(|b| b.is_ascii_graphic()),
            "git grant token shape",
        )
    }
}
impl std::fmt::Debug for GitToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GitToken([REDACTED])")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitGrant {
    pub schema_version: u32,
    /// `/v1/git/<repository_id>` on the worker API.
    pub endpoint: String,
    pub repository_id: Uuid,
    /// The commit the attempt starts from; every pushed commit descends from it.
    pub base: ObjectId,
    /// `refs/heads/aq/<task-id>`, the only ref a push may write.
    pub task_ref: String,
    /// The task branch's head in central aq-git when the offer was made;
    /// absent when the branch does not exist. A push replaces exactly this.
    pub lease: Option<ObjectId>,
    /// The project's default branch, when the server knows it.
    pub main_ref: Option<String>,
    /// The push's aq-git operation; `push.receipt` reads it back.
    pub operation_id: Uuid,
    pub token: GitToken,
}
fn sha1(object: &ObjectId) -> Result<()> {
    object.validate()?;
    require(
        object.format == ObjectFormat::Sha1,
        "git grant object format",
    )
}
fn branch(name: &str) -> bool {
    name.starts_with("refs/heads/")
        && name.len() > "refs/heads/".len()
        && name.len() <= 1024
        && name.bytes().all(|b| b.is_ascii_graphic())
        && !name.contains("..")
        && !name.ends_with('/')
}
impl GitGrant {
    /// Shape, and that the grant names `task_id`'s own branch.
    pub fn validate_for_task(&self, task_id: Uuid) -> Result<()> {
        require(self.schema_version == 1, "git grant version")?;
        id(self.repository_id)?;
        id(self.operation_id)?;
        require(
            self.endpoint == endpoint(self.repository_id),
            "git grant endpoint",
        )?;
        require(self.task_ref == task_ref(task_id), "git grant task ref")?;
        sha1(&self.base)?;
        if let Some(lease) = &self.lease {
            sha1(lease)?;
        }
        if let Some(main) = &self.main_ref {
            require(
                branch(main) && !main.starts_with(TASK_REF_PREFIX),
                "git grant main ref",
            )?;
        }
        self.token.validate()
    }
}

/// One offer's grant on a poll reply.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OfferGitGrant {
    pub attempt_id: Uuid,
    pub grant: GitGrant,
}
/// The poll reply's top-level field beside `kind` and `body`. The frozen
/// control reply (`ControlReply`, `NativePollPage`, `NativeOffer`) is not
/// changed: its bytes stay what `decode_reply` accepts once this is removed.
pub const POLL_REPLY_FIELD: &str = "git_grants";
/// Each grant names an offer on the page, at most once, and that offer's task.
pub fn validate_page(
    offers: &[crate::startup::NativeOffer],
    grants: &[OfferGitGrant],
) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for g in grants {
        let offer = offers
            .iter()
            .find(|o| o.offered.offer.binding.attempt_id == g.attempt_id)
            .ok_or(Invalid("git grant without its offer"))?;
        require(seen.insert(g.attempt_id), "duplicate git grant")?;
        g.grant
            .validate_for_task(offer.offered.offer.manifest.bindings.task_id)?;
    }
    Ok(())
}
/// Splits a poll reply into the frozen control reply and the offers' grants.
pub fn decode_poll_reply(
    bytes: &[u8],
) -> Result<(crate::control::ControlReply, Vec<OfferGitGrant>)> {
    require(
        bytes.len() <= crate::control::MAX_REPLY_BYTES,
        "control frame byte bound",
    )?;
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| Invalid("control reply JSON shape"))?;
    let grants: Vec<OfferGitGrant> = match value
        .as_object_mut()
        .and_then(|o| o.remove(POLL_REPLY_FIELD))
    {
        Some(v) => serde_json::from_value(v).map_err(|_| Invalid("git grant JSON shape"))?,
        None => vec![],
    };
    let frozen = serde_json::to_vec(&value).map_err(|_| Invalid("control encoding"))?;
    let reply = crate::control::decode_reply(&frozen)?;
    match &reply {
        crate::control::ControlReply::Poll(page) => validate_page(&page.offers, &grants)?,
        _ => require(grants.is_empty(), "git grant outside a poll reply")?,
    }
    Ok((reply, grants))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn object(c: char) -> ObjectId {
        ObjectId {
            format: ObjectFormat::Sha1,
            hex: c.to_string().repeat(40),
        }
    }
    fn grant(task: Uuid) -> GitGrant {
        let repository = Uuid::from_u128(7);
        GitGrant {
            schema_version: 1,
            endpoint: endpoint(repository),
            repository_id: repository,
            base: object('a'),
            task_ref: task_ref(task),
            lease: Some(object('b')),
            main_ref: Some("refs/heads/main".into()),
            operation_id: Uuid::from_u128(9),
            token: GitToken::new(format!("aqg_{}", "0".repeat(40))).unwrap(),
        }
    }
    #[test]
    fn grant_names_its_own_task_branch_and_endpoint() {
        let task = Uuid::from_u128(3);
        grant(task).validate_for_task(task).unwrap();
        assert!(grant(task).validate_for_task(Uuid::from_u128(4)).is_err());
        let mut g = grant(task);
        g.endpoint = endpoint(Uuid::from_u128(8));
        assert!(g.validate_for_task(task).is_err());
        let mut g = grant(task);
        g.main_ref = Some(task_ref(task));
        assert!(g.validate_for_task(task).is_err());
        let mut g = grant(task);
        g.lease = None;
        g.main_ref = None;
        g.validate_for_task(task).unwrap();
        assert!(GitToken::new("aqn_short").is_err());
    }
    #[test]
    fn token_is_never_printed() {
        let g = grant(Uuid::from_u128(3));
        let printed = format!("{g:?}");
        assert!(printed.contains("REDACTED") && !printed.contains(g.token.expose()));
    }
}
