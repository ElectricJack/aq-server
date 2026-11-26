//! Negotiated worker poll extension, separate from the frozen launch protocols.
use anyhow::{Result, ensure};
use aq_admission::{
    control::{self, ControlReply, NativePollPage},
    startup::{Descriptor, NativeOffer},
    transport::{Cancellation, OfferedLaunch, WorkerIdentity},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const POLL_HEADER: &str = "aq-git-author";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitAuthor {
    pub name: String,
    pub email: String,
}
fn allowed(c: char) -> bool {
    !c.is_control() && !matches!(c, '<' | '>')
}

fn valid_edges(value: &str) -> bool {
    let edge = |c: char| !c.is_ascii_whitespace() && !r#",:;<>"'\"#.contains(c);
    value.chars().next().is_some_and(edge) && value.chars().next_back().is_some_and(edge)
}

fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    email.len() <= 254
        && valid_edges(email)
        && email.chars().all(|c| allowed(c) && !c.is_whitespace())
        && !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
}

impl GitAuthor {
    pub fn validate(&self) -> Result<()> {
        let name = &self.name;
        let valid_name = !name.trim().is_empty()
            && valid_edges(name)
            && name.chars().count() <= 120
            && name.chars().all(allowed);
        ensure!(
            valid_name && valid_email(&self.email),
            "invalid offered Git author"
        );
        Ok(())
    }

    pub fn identity(&self) -> (&str, &str) {
        (&self.name, &self.email)
    }

    /// An ordinary sandbox Git process needs both identities. AQ's own
    /// checkpoint subprocesses set their committer independently.
    pub fn tool_environment(&self) -> impl ExactSizeIterator<Item = (&'static str, &str)> {
        [
            ("GIT_AUTHOR_NAME", self.name.as_str()),
            ("GIT_AUTHOR_EMAIL", self.email.as_str()),
            ("GIT_COMMITTER_NAME", self.name.as_str()),
            ("GIT_COMMITTER_EMAIL", self.email.as_str()),
        ]
        .into_iter()
    }
}

/// The local owner always reserves the original offer together with its author.
pub struct AttemptOffer {
    pub offer: NativeOffer,
    pub git_author: Option<GitAuthor>,
}
impl From<NativeOffer> for AttemptOffer {
    fn from(offer: NativeOffer) -> Self {
        Self {
            offer,
            git_author: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum PollReply {
    Poll(Page),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    schema_version: u32,
    operation_id: Uuid,
    identity: WorkerIdentity,
    server_time_ms: u64,
    offers: Vec<AuthoredOffer>,
    cancellations: Vec<Cancellation>,
    next_after: Option<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredOffer {
    schema_version: u32,
    offered: OfferedLaunch,
    startup: Descriptor,
    git_author: Option<GitAuthor>,
}

/// Strip only the negotiated field, then use the ordinary typed validation
/// for the original offer. Non-poll replies keep the original decoder.
pub fn decode_poll(bytes: &[u8]) -> Result<(ControlReply, Vec<Option<GitAuthor>>)> {
    ensure!(
        bytes.len() <= control::MAX_REPLY_BYTES,
        "control frame byte bound"
    );
    let Ok(PollReply::Poll(page)) = serde_json::from_slice(bytes) else {
        return Ok((control::decode_reply(bytes)?, vec![]));
    };
    let (offers, authors) = page
        .offers
        .into_iter()
        .map(|entry| {
            if let Some(author) = &entry.git_author {
                author.validate()?;
            }
            Ok((
                NativeOffer {
                    schema_version: entry.schema_version,
                    offered: entry.offered,
                    startup: entry.startup,
                },
                entry.git_author,
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();
    let reply = ControlReply::Poll(NativePollPage {
        schema_version: page.schema_version,
        operation_id: page.operation_id,
        identity: page.identity,
        server_time_ms: page.server_time_ms,
        offers,
        cancellations: page.cancellations,
        next_after: page.next_after,
    });
    reply.validate_shape()?;
    Ok((reply, authors))
}
