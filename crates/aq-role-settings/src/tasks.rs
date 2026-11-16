//! A task's role and a project's defaults against the active snapshot (spec
//! 0014 §5.4, §5.5). Pure: the caller reads the snapshot and the task.
use aq_profiles::roles::Snapshot;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The task metadata key that holds the role a task names. Absent when the
/// task names none; the project's `default_role` then applies.
pub const ROLE_KEY: &str = "aq:role";

/// The role a task runs as, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRole {
    pub id: String,
    /// The task names it; otherwise it is the project's default.
    pub from_task: bool,
}
impl EffectiveRole {
    /// `{id, source}` as every task read shows it; `source` is `task` or
    /// `project_default`.
    pub fn shown(&self) -> Value {
        let source = if self.from_task {
            "task"
        } else {
            "project_default"
        };
        json!({"id": self.id, "source": source})
    }
}
/// The one rule every reader uses: the task's `aq:role` when it has one,
/// otherwise the project binding's `default_role`.
pub fn effective_role(metadata: &Value, default_role: &str) -> EffectiveRole {
    match metadata[ROLE_KEY].as_str().filter(|role| !role.is_empty()) {
        Some(role) => EffectiveRole {
            id: role.into(),
            from_task: true,
        },
        None => EffectiveRole {
            id: default_role.into(),
            from_task: false,
        },
    }
}

fn listed<'a>(ids: impl Iterator<Item = &'a str>, none: &str) -> String {
    let ids: Vec<&str> = ids.collect();
    if ids.is_empty() {
        none.into()
    } else {
        ids.join(", ")
    }
}
fn roles(snapshot: Option<&Snapshot>) -> String {
    let ids = snapshot.into_iter().flat_map(Snapshot::roles);
    listed(ids.map(|role| role.id.as_str()), "none")
}

/// A role that the active snapshot does not hold, with the roles it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRole {
    pub role: String,
    /// The existing role ids, comma separated; `none` when there are none.
    pub existing: String,
}
impl std::fmt::Display for UnknownRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Role `{}` does not exist. The roles are: {}.",
            self.role, self.existing
        )
    }
}
/// Filing a task with `role`: it must exist in the active snapshot. A
/// disabled role may be named; the task is then held ([`role_hold`]).
pub fn named_role(snapshot: Option<&Snapshot>, role: &str) -> Result<(), UnknownRole> {
    if snapshot.is_some_and(|snapshot| snapshot.role(role).is_some()) {
        return Ok(());
    }
    Err(UnknownRole {
        role: role.into(),
        existing: roles(snapshot),
    })
}

/// Why a task with this effective role gets no offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleHold {
    /// `role_disabled` or `role_missing`.
    pub code: &'static str,
    pub role: String,
}
impl RoleHold {
    pub fn message(&self) -> String {
        let role = &self.role;
        match self.code {
            "role_disabled" => format!(
                "AQ does not offer this task: its role `{role}` is disabled. Enable the role in Settings or change the task's role."
            ),
            _ => format!(
                "AQ does not offer this task: its role `{role}` does not exist. Change the task's role or the project's default role."
            ),
        }
    }
    /// The reason as the work view lists it.
    pub fn shown(&self) -> Value {
        json!({"code": self.code, "role": self.role, "message": self.message()})
    }
}
/// The hold of a task whose effective role is `role`, if any: the role is
/// disabled, or the active snapshot does not hold it.
pub fn role_hold(snapshot: Option<&Snapshot>, role: &str) -> Option<RoleHold> {
    let code = match snapshot.and_then(|snapshot| snapshot.role(role)) {
        Some(found) if found.enabled => return None,
        Some(_) => "role_disabled",
        None => "role_missing",
    };
    Some(RoleHold {
        code,
        role: role.into(),
    })
}

/// A project binding's `default_role`, `default_intelligence` and
/// `role_notes` against the active snapshot: both defaults exist, every note
/// is for an existing role, and each note is within `max_note_bytes` (the
/// store's document byte limit). The message names what exists.
pub fn check_binding(
    snapshot: Option<&Snapshot>,
    default_role: &str,
    default_intelligence: &str,
    role_notes: &BTreeMap<String, String>,
    max_note_bytes: usize,
) -> Result<(), String> {
    named_role(snapshot, default_role).map_err(|unknown| format!("default_role: {unknown}"))?;
    if !snapshot.is_some_and(|s| s.intelligence(default_intelligence).is_some()) {
        let profiles = snapshot
            .into_iter()
            .flat_map(Snapshot::intelligence_profiles);
        return Err(format!(
            "default_intelligence: Intelligence profile `{default_intelligence}` does not exist. The intelligence profiles are: {}.",
            listed(profiles.map(|profile| profile.id.as_str()), "none")
        ));
    }
    for (role, note) in role_notes {
        named_role(snapshot, role).map_err(|unknown| format!("role_notes: {unknown}"))?;
        if note.len() > max_note_bytes {
            return Err(format!(
                "role_notes: the note for role `{role}` is {} bytes, over the limit of {max_note_bytes} bytes",
                note.len()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_profiles::roles::{self, SelectionDefaults};

    fn snapshot(disable_reviewer: bool) -> Snapshot {
        let mut documents = roles::default_documents();
        if disable_reviewer {
            for document in &mut documents {
                if document.label == "roles/reviewer.md" {
                    document.text = document.text.replace("enabled: true", "enabled: false");
                }
            }
        }
        roles::compile(
            &documents,
            &SelectionDefaults::default(),
            &Default::default(),
        )
        .unwrap()
    }

    #[test]
    fn the_effective_role_is_the_tasks_or_the_project_default() {
        let named = effective_role(&json!({"aq:role": "reviewer"}), "developer");
        assert_eq!(named.shown(), json!({"id": "reviewer", "source": "task"}));
        for metadata in [json!({}), json!({"aq:role": null}), json!({"aq:role": ""})] {
            let defaulted = effective_role(&metadata, "developer");
            assert_eq!(
                defaulted.shown(),
                json!({"id": "developer", "source": "project_default"})
            );
        }
    }
    #[test]
    fn a_named_role_must_exist_and_the_refusal_lists_the_roles() {
        let s = snapshot(true);
        assert!(named_role(Some(&s), "developer").is_ok());
        // A disabled role may be named.
        assert!(named_role(Some(&s), "reviewer").is_ok());
        assert_eq!(
            named_role(Some(&s), "tester").unwrap_err().to_string(),
            "Role `tester` does not exist. The roles are: developer, reviewer."
        );
        assert_eq!(
            named_role(None, "developer").unwrap_err().to_string(),
            "Role `developer` does not exist. The roles are: none."
        );
    }
    #[test]
    fn a_disabled_or_missing_role_holds_the_task() {
        let s = snapshot(true);
        assert_eq!(role_hold(Some(&s), "developer"), None);
        let disabled = role_hold(Some(&s), "reviewer").unwrap();
        assert_eq!(disabled.code, "role_disabled");
        assert!(
            disabled
                .message()
                .contains("its role `reviewer` is disabled")
        );
        let missing = role_hold(Some(&s), "tester").unwrap();
        assert_eq!(
            missing.shown(),
            json!({"code": "role_missing", "role": "tester", "message": missing.message()})
        );
        assert!(
            missing
                .message()
                .contains("its role `tester` does not exist")
        );
        assert_eq!(role_hold(None, "developer").unwrap().code, "role_missing");
    }
    #[test]
    fn binding_defaults_and_notes_are_checked_against_the_snapshot() {
        let s = snapshot(false);
        let notes = |role: &str, bytes: usize| BTreeMap::from([(role.into(), "n".repeat(bytes))]);
        let check = |role: &str, level: &str, notes: &BTreeMap<String, String>| {
            check_binding(Some(&s), role, level, notes, 16)
        };
        assert!(check("developer", "standard-high", &notes("reviewer", 16)).is_ok());
        assert_eq!(
            check("tester", "standard-high", &BTreeMap::new()).unwrap_err(),
            "default_role: Role `tester` does not exist. The roles are: developer, reviewer."
        );
        assert_eq!(
            check("developer", "enormous", &BTreeMap::new()).unwrap_err(),
            "default_intelligence: Intelligence profile `enormous` does not exist. The intelligence profiles are: complex-high, complex-low, deep-high, deep-low, fast-high, fast-low, standard-high, standard-low, standard-medium."
        );
        assert_eq!(
            check("developer", "standard-high", &notes("tester", 1)).unwrap_err(),
            "role_notes: Role `tester` does not exist. The roles are: developer, reviewer."
        );
        assert_eq!(
            check("developer", "standard-high", &notes("reviewer", 17)).unwrap_err(),
            "role_notes: the note for role `reviewer` is 17 bytes, over the limit of 16 bytes"
        );
        assert!(
            check_binding(None, "developer", "standard-high", &BTreeMap::new(), 16)
                .unwrap_err()
                .starts_with("default_role: Role `developer` does not exist")
        );
    }
}
