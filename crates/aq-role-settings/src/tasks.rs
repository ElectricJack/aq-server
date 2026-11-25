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

/// Why a task gets no offer although its role can run: the level it would
/// run on is not a document of the active snapshot. A level's id is the
/// router's class (spec 0014 §5.2), so this names the missing level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelHold {
    pub level: String,
    /// The task was routed to this level; otherwise it is the project's default.
    pub routed: bool,
}
impl LevelHold {
    pub const CODE: &'static str = "level_missing";
    pub fn message(&self) -> String {
        let level = &self.level;
        if self.routed {
            format!(
                "AQ does not offer this task: it was routed to `{level}`, and no intelligence profile `{level}` exists. Add that intelligence profile in Settings or route the task again."
            )
        } else {
            format!(
                "AQ does not offer this task: the project's default intelligence profile `{level}` does not exist. Add it in Settings or change the project's default."
            )
        }
    }
    pub fn shown(&self) -> Value {
        json!({"code": Self::CODE, "level": self.level, "message": self.message()})
    }
}
/// The level a task runs on: the class it was routed to, otherwise the
/// project's default intelligence profile; and its hold when the active
/// snapshot has no such document.
pub fn level_hold(
    snapshot: Option<&Snapshot>,
    routed_class: Option<&str>,
    default_intelligence: &str,
) -> Option<LevelHold> {
    let level = routed_class.unwrap_or(default_intelligence);
    let exists = snapshot.is_some_and(|snapshot| snapshot.intelligence(level).is_some());
    (!exists).then(|| LevelHold {
        level: level.into(),
        routed: routed_class.is_some(),
    })
}

/// The ready tasks that get no offer, with the hold code: the effective
/// role is disabled or missing, or the level the task would run on (the class
/// it was routed to, else the project's default) has no document. `nodes` are
/// `(task, node, routed class)`.
pub fn held<'a>(
    snapshot: Option<&Snapshot>,
    default_role: &str,
    default_intelligence: &str,
    nodes: impl Iterator<Item = (uuid::Uuid, &'a Value, Option<String>)>,
) -> BTreeMap<uuid::Uuid, &'static str> {
    nodes
        .filter_map(|(task, node, class)| {
            let role = effective_role(&node["metadata"], default_role);
            let role = role_hold(snapshot, &role.id).map(|hold| hold.code);
            let level = || {
                let hold = level_hold(snapshot, class.as_deref(), default_intelligence)?;
                tracing::info!(%task, level = %hold.level, "{}", hold.message());
                Some(LevelHold::CODE)
            };
            role.or_else(level).map(|code| (task, code))
        })
        .collect()
}
/// Whether an attempt of `task` was ever granted; its role is then fixed.
pub async fn granted(pool: &sqlx::PgPool, task: uuid::Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE e.task_id=$1 AND NOT (COALESCE(h.state,e.state)->>'status' IN ('offered','preparing','declined','never_granted') \
         AND COALESCE(jsonb_typeof(COALESCE(h.state,e.state)->'grant'),'null')='null'))",
    )
    .bind(task)
    .fetch_one(pool)
    .await
}

/// An agent may not hand out more than it holds: the operations of the role
/// a task is filed with that the filing attempt's own role does not list.
/// Empty when `named` is within `filer`.
pub fn operations_beyond(filer: &[String], named: &[String]) -> Vec<String> {
    let mut lacking: Vec<String> = (named.iter())
        .filter(|operation| !filer.contains(operation))
        .cloned()
        .collect();
    lacking.sort();
    lacking.dedup();
    lacking
}
/// The refusal for [`operations_beyond`], naming the operations.
pub fn beyond_message(filer: &str, named: &str, lacking: &[String]) -> String {
    format!(
        "Role `{named}` is not within your own role `{filer}`: it may use {}, which you may not. An agent can file a task only for a role whose operations it holds itself.",
        lacking.join(", ")
    )
}

/// The role the filing attempt itself runs as, read from the snapshot it
/// launched with. `None` when the attempt is not restricted: it has no
/// summary (it was not launched from a role), it was recorded before roles
/// and names no snapshot, or its snapshot or role is no longer stored.
pub async fn filer_role(
    pool: &sqlx::PgPool,
    attempt: uuid::Uuid,
) -> crate::Result<Option<aq_profiles::roles::Role>> {
    let mut conn = pool.acquire().await?;
    let ran_with = crate::attempts::ran_with(&mut conn, attempt).await?;
    drop(conn);
    let Some(ran_with) = ran_with else {
        return Ok(None);
    };
    let Some(digest) = &ran_with.snapshot_digest else {
        return Ok(None);
    };
    let own = crate::snapshot(pool, digest).await?;
    Ok(own.and_then(|snapshot| snapshot.role(&ran_with.role).cloned()))
}
/// [`operations_beyond`] as the refusal of a filing: `target` is the role
/// the task is filed with. A role that does not exist is refused (or held)
/// by its own rule.
pub fn within(
    filer: &aq_profiles::roles::Role,
    target: Option<&aq_profiles::roles::Role>,
) -> Result<(), String> {
    let Some(target) = target else {
        return Ok(());
    };
    let lacking = operations_beyond(&filer.operations, &target.operations);
    match lacking.is_empty() {
        true => Ok(()),
        false => Err(beyond_message(&filer.id, &target.id, &lacking)),
    }
}

pub struct BindingDefaults<'a> {
    pub role: &'a str,
    pub intelligence: &'a str,
}
/// A project binding's `default_role`, `default_intelligence` and
/// `role_notes` against the active snapshot: both defaults exist, every note
/// is for an existing role, and each note is within `max_note_bytes` (the
/// store's document byte limit). The message names what exists.
pub fn check_binding(
    snapshot: Option<&Snapshot>,
    defaults: BindingDefaults<'_>,
    role_notes: &BTreeMap<String, String>,
    max_note_bytes: usize,
) -> Result<(), String> {
    let BindingDefaults {
        role: default_role,
        intelligence: default_intelligence,
    } = defaults;
    named_role(snapshot, default_role).map_err(|unknown| format!("default_role: {unknown}"))?;
    if snapshot.is_none_or(|s| s.intelligence(default_intelligence).is_none()) {
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
            check_binding(
                Some(&s),
                BindingDefaults {
                    role,
                    intelligence: level,
                },
                notes,
                16,
            )
        };
        assert!(check("developer", "standard-high", &notes("reviewer", 16)).is_ok());
        assert_eq!(
            check("tester", "standard-high", &BTreeMap::new()).unwrap_err(),
            "default_role: Role `tester` does not exist. The roles are: developer, reviewer."
        );
        assert_eq!(
            check("developer", "enormous", &BTreeMap::new()).unwrap_err(),
            "default_intelligence: Intelligence profile `enormous` does not exist. The intelligence profiles are: complex-high, complex-low, deep-high, deep-low, simple-high, simple-low, standard-high, standard-low, standard-medium."
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
            check_binding(
                None,
                BindingDefaults {
                    role: "developer",
                    intelligence: "standard-high"
                },
                &BTreeMap::new(),
                16
            )
            .unwrap_err()
            .starts_with("default_role: Role `developer` does not exist")
        );
    }
}
