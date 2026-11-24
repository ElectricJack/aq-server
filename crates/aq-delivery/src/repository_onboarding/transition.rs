use super::{Git, Row};
use serde_json::{Value, json};
use std::ops::ControlFlow;

#[derive(serde::Serialize)]
#[serde(rename_all = "kebab-case")]
enum RefusalSource {
    AqGit,
    Precheck,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RefusalClass {
    UnsupportedRepoProfile,
    CredentialProfileUnavailable,
    RepositoryAlreadyRegistered,
    InvalidRepository,
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum UnsupportedReason {
    ObjectFormat,
    DefaultBranchAbsent,
    RequiredSigning,
    UnsupportedFeature,
    Submodules,
    Lfs,
    Gitattributes,
}
impl UnsupportedReason {
    fn source(self) -> RefusalSource {
        match self {
            Self::ObjectFormat | Self::DefaultBranchAbsent | Self::UnsupportedFeature => {
                RefusalSource::AqGit
            }
            Self::RequiredSigning | Self::Submodules | Self::Lfs | Self::Gitattributes => {
                RefusalSource::Precheck
            }
        }
    }
}

/// Result of one onboarding step, ready to persist before the next external operation.
pub enum Transition {
    Registered(u64),
    Synced(Value),
    Checked,
    Bound(Value),
    Refused(Value),
    Retry(Value),
}
impl Transition {
    pub fn apply(self, row: &mut Row) -> ControlFlow<()> {
        let step = match self {
            Self::Registered(generation) => {
                row.state["binding_generation"] = json!(generation);
                "registered"
            }
            Self::Synced(base) => {
                row.state["base"] = base;
                "synced"
            }
            Self::Checked => "checked",
            Self::Bound(project) => {
                row.state["project"] = project;
                "bound"
            }
            Self::Refused(refusal) => {
                row.state["refusal"] = refusal;
                row.step = "refused".into();
                return ControlFlow::Break(());
            }
            Self::Retry(error) => {
                row.state["error"] = error;
                return ControlFlow::Break(());
            }
        };
        row.state["error"] = Value::Null;
        row.step = step.into();
        if step == "bound" {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
    pub fn retry(code: &str, message: &str) -> Self {
        Self::Retry(json!({"code":code,"message":message,"retry":true}))
    }
    fn refused(
        code: RefusalClass,
        reason: impl serde::Serialize,
        source: RefusalSource,
        message: &str,
    ) -> Self {
        Self::Refused(json!({"code":code,"reason":reason,"source":source,"message":message}))
    }
    fn unsupported(reason: UnsupportedReason, message: &str) -> Self {
        Self::refused(
            RefusalClass::UnsupportedRepoProfile,
            reason,
            reason.source(),
            message,
        )
    }
    pub fn signing_required() -> Self {
        Self::unsupported(
            UnsupportedReason::RequiredSigning,
            "the default branch requires signed commits",
        )
    }
    fn unavailable() -> Self {
        Self::retry("aq_git_unavailable", "aq-git is unavailable")
    }
    pub fn registration(reply: Git, has_profile: bool) -> Self {
        match reply {
            Git::Ok(value) => {
                let central = value["object_format"] == "sha1" && value["mode"] == "central";
                match value["binding_generation"].as_u64() {
                    Some(g) if central && g > 0 => Self::Registered(g),
                    _ => Self::unsupported(
                        UnsupportedReason::ObjectFormat,
                        "aq-git binding is not central SHA-1",
                    ),
                }
            }
            Git::Refused { code, message } => {
                Self::registration_refusal(&code, &message, has_profile)
            }
            Git::Unavailable => Self::unavailable(),
        }
    }
    fn registration_refusal(code: &str, message: &str, has_profile: bool) -> Self {
        let (kind, reason) = match code {
            "unsupported_feature" if has_profile && message.contains("credential") => (
                RefusalClass::CredentialProfileUnavailable,
                "credential_profile_unavailable",
            ),
            "unsupported_feature" => {
                return Self::unsupported(UnsupportedReason::UnsupportedFeature, message);
            }
            "binding_conflict" => (
                RefusalClass::RepositoryAlreadyRegistered,
                "binding_conflict",
            ),
            "invalid_request" => (RefusalClass::InvalidRepository, "invalid_request"),
            _ => return Self::retry(code, message),
        };
        Self::refused(kind, reason, RefusalSource::AqGit, message)
    }
    fn read(reply: Git, complete: impl FnOnce(Value) -> Self) -> Self {
        match reply {
            Git::Ok(value) => complete(value),
            Git::Refused { code, message } if code == "unsupported_feature" => {
                Self::unsupported(UnsupportedReason::UnsupportedFeature, &message)
            }
            Git::Refused { code, message } => Self::retry(&code, &message),
            Git::Unavailable => Self::unavailable(),
        }
    }
    pub fn sync(reply: Git, target: &str) -> Self {
        Self::read(reply, |v| match v["refs"][target].as_str() {
            Some(oid) if oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Self::Synced(json!({"ref":target,"oid":oid,"observation_id":v["observation_id"]}))
            }
            _ => Self::unsupported(
                UnsupportedReason::DefaultBranchAbsent,
                "the default branch has no retained head",
            ),
        })
    }
    pub fn tree(reply: Git) -> Self {
        Self::read(reply, |v| {
            let entries = v["entries"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            match tree_refusal(entries) {
                Some((reason, message)) => Self::unsupported(reason, &message),
                None => Self::Checked,
            }
        })
    }
}
/// Pre-check the retained head's top-level tree against the v1 checkout
/// profile (`no_attributes_v1`, no submodules, no LFS).
fn tree_refusal(entries: &[Value]) -> Option<(UnsupportedReason, String)> {
    for e in entries {
        let path = e["path"]["value"].as_str().unwrap_or("");
        let found = if e["mode"] == "160000" || path == ".gitmodules" {
            Some(UnsupportedReason::Submodules)
        } else if path == ".lfsconfig" {
            Some(UnsupportedReason::Lfs)
        } else if path == ".gitattributes" {
            Some(UnsupportedReason::Gitattributes)
        } else {
            None
        };
        if let Some(reason) = found {
            return Some((reason, format!("repository head contains {path}")));
        }
    }
    None
}
