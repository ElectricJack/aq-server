//! Opt-in task attribution on native poll replies. The original startup and
//! control contracts remain valid for clients that send no opt-in header.
//! `POST /v1/worker/profiled/poll` accepts a single `aq-git-author: 1`.
//! The response adds `body.offers[n].git_author` only when the published
//! assignment carries an author. The metadata does not grant launch authority.
//! Attributed tasks require the opt-in for a new offer. Reoffers keep the
//! presence and exact value recorded in the attempt's original offer.
use aq_admission::{
    Invalid, Result,
    control::{MAX_REPLY_BYTES, NativePollPage},
    startup::{Descriptor, NativeOffer},
    transport::{Cancellation, OfferedLaunch, Poll, WorkerIdentity},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

fn require(ok: bool, problem: &'static str) -> Result<()> {
    if ok { Ok(()) } else { Err(Invalid(problem)) }
}

pub const HEADER: &str = "aq-git-author";
pub const VERSION: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitAuthor {
    pub name: String,
    pub email: String,
}

impl GitAuthor {
    pub fn validate(&self) -> Result<()> {
        require(
            valid_name(&self.name) && valid_email(&self.email),
            "invalid task git author",
        )
    }
}
fn signature_char(c: char) -> bool {
    !c.is_control() && !matches!(c, '<' | '>')
}
fn preserves_edges(s: &str) -> bool {
    // Git strips these characters at both ends of a signature field.
    let stripped = |c: char| {
        c.is_ascii_whitespace() || matches!(c, ',' | ':' | ';' | '<' | '>' | '"' | '\'' | '\\')
    };
    !s.starts_with(stripped) && !s.ends_with(stripped)
}
fn valid_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.chars().count() <= 120
        && name.chars().all(signature_char)
        && preserves_edges(name)
}
fn valid_email(email: &str) -> bool {
    let address = email.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
    });
    address
        && email.len() <= 254
        && email
            .chars()
            .all(|c| signature_char(c) && !c.is_whitespace())
        && preserves_edges(email)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, from = "OfferWire", into = "OfferWire")]
pub struct OfferDelivery {
    pub native: NativeOffer,
    pub git_author: Option<GitAuthor>,
}

impl OfferDelivery {
    pub fn validate(&self) -> Result<()> {
        if let Some(author) = &self.git_author {
            author.validate()?;
        }
        self.native.validate()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OfferWire {
    schema_version: u32,
    offered: OfferedLaunch,
    startup: Descriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_author: Option<GitAuthor>,
}

impl From<OfferDelivery> for OfferWire {
    fn from(value: OfferDelivery) -> Self {
        let NativeOffer {
            schema_version,
            offered,
            startup,
        } = value.native;
        Self {
            schema_version,
            offered,
            startup,
            git_author: value.git_author,
        }
    }
}

impl From<OfferWire> for OfferDelivery {
    fn from(value: OfferWire) -> Self {
        let OfferWire {
            schema_version,
            offered,
            startup,
            git_author,
        } = value;
        Self {
            native: NativeOffer {
                schema_version,
                offered,
                startup,
            },
            git_author,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollDelivery {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub server_time_ms: u64,
    pub offers: Vec<OfferDelivery>,
    pub cancellations: Vec<Cancellation>,
    pub next_after: Option<Uuid>,
}

impl PollDelivery {
    pub fn into_native(self) -> NativePollPage {
        NativePollPage {
            schema_version: self.schema_version,
            operation_id: self.operation_id,
            identity: self.identity,
            server_time_ms: self.server_time_ms,
            offers: self.offers.into_iter().map(|offer| offer.native).collect(),
            cancellations: self.cancellations,
            next_after: self.next_after,
        }
    }

    pub fn validate_for(&self, request: &Poll) -> Result<()> {
        for author in self
            .offers
            .iter()
            .filter_map(|offer| offer.git_author.as_ref())
        {
            author.validate()?;
        }
        self.clone().into_native().validate_for(request)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PollReply {
    Poll(PollDelivery),
}

pub fn decode_poll_reply(bytes: &[u8], request: &Poll) -> Result<PollDelivery> {
    require(bytes.len() <= MAX_REPLY_BYTES, "control frame byte bound")?;
    let PollReply::Poll(page) =
        serde_json::from_slice(bytes).map_err(|_| Invalid("native poll reply JSON shape"))?;
    page.validate_for(request)?;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_admission::control::decode_reply;
    use serde_json::{Value, json};

    fn fixture() -> (Value, Poll) {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../aq-admission/artifacts/control.fixture.json"
        ))
        .unwrap();
        (
            fixture["replies"][0].clone(),
            serde_json::from_value(fixture["requests"]["poll"].clone()).unwrap(),
        )
    }

    #[test]
    fn legacy_poll_reply_round_trips_without_wire_changes() {
        let (value, request) = fixture();
        let json = serde_json::to_vec(&value).unwrap();
        let page = decode_poll_reply(&json, &request).unwrap();
        assert!(page.offers.iter().all(|offer| offer.git_author.is_none()));
        assert_eq!(serde_json::to_value(PollReply::Poll(page)).unwrap(), value);
        decode_reply(&json).unwrap();
    }

    #[test]
    fn negotiated_poll_reply_accepts_exact_identity_and_rejects_injection() {
        let (mut value, request) = fixture();
        let name = "Zoë+Work";
        let email = "Zoë+Work@Example.test";
        value["body"]["offers"][0]["git_author"] = json!({"name": name, "email": email});
        let decode =
            |value: &Value| decode_poll_reply(&serde_json::to_vec(value).unwrap(), &request);
        let page = decode(&value).unwrap();
        let author = page.offers[0].git_author.as_ref().unwrap();
        assert_eq!((&*author.name, &*author.email), (name, email));
        assert!(decode_reply(&serde_json::to_vec(&value).unwrap()).is_err());

        for bad in [
            json!({"name": "a\nAuthor", "email": email}),
            json!({"name": "  ", "email": email}),
            json!({"name": "λ".repeat(121), "email": email}),
            json!({"name": "Author,", "email": email}),
            json!({"name": " Author", "email": email}),
            json!({"name": name, "email": "a b@example.test"}),
            json!({"name": name, "email": ":a@example.test"}),
            json!({"name": name, "email": "<a@example.test>"}),
            json!({"name": name, "email": "a@b@example.test"}),
            json!({"name": name, "email": format!("{}@a", "b".repeat(253))}),
            json!({"name": name, "email": email, "other": true}),
            json!({"name": name}),
        ] {
            value["body"]["offers"][0]["git_author"] = bad;
            assert!(decode(&value).is_err(), "{value}");
        }
    }
}
