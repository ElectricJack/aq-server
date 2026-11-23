//! Spec 0017 §5: which environment an attempt runs in. Pure: the caller
//! reads the task's metadata, the binding, the profile and the definitions.
//! This is the default rule of §5.4; the environment the routing document
//! chose comes in as `kept` when the attempt continues none.
use crate::{ProjectBinding, labels};
use serde_json::Value;

/// The task metadata key that holds what a task needs from its environment.
pub const NEEDS_KEY: &str = "aq:needs";
/// The most capability labels a task may need or an environment provide.
pub const MAX_LABELS: usize = 32;

/// A task's needs as filed: distinct bounded labels. The message names the fault.
pub fn check_needs(needs: &[String]) -> Result<(), String> {
    labels(needs, MAX_LABELS).map_err(|e| format!("needs: {e}"))
}
/// What a task needs: its `aq:needs`, sorted. Anything else there needs nothing.
pub fn needs(metadata: &Value) -> Vec<String> {
    let mut needs: Vec<String> = (metadata[NEEDS_KEY].as_array().into_iter().flatten())
        .filter_map(|n| n.as_str().map(str::to_owned))
        .collect();
    needs.sort();
    needs.dedup();
    needs
}
/// `provides` of an environment definition body: a JSON object's list of
/// labels. Any other body (today's) provides nothing.
pub fn provides(body: &str) -> Vec<String> {
    let value: Value = serde_json::from_str(body).unwrap_or_default();
    let provides = needs(&serde_json::json!({ NEEDS_KEY: value["provides"] }));
    match labels(&provides, MAX_LABELS) {
        Ok(()) => provides,
        Err(_) => vec![],
    }
}
/// What `environment` (`name` or `name@version`) provides, from the
/// definitions `(id, version, body)`: what every matching version provides.
pub fn provided<'a>(
    environment: &str,
    definitions: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
) -> Vec<String> {
    let (name, version) = match environment.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (environment, None),
    };
    (definitions.into_iter())
        .filter(|(id, v, _)| *id == name && version.is_none_or(|version| version == *v))
        .map(|(_, _, body)| provides(body))
        .reduce(|all, next| all.into_iter().filter(|c| next.contains(c)).collect())
        .unwrap_or_default()
}

/// Why no environment of the project can take the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    pub environment: String,
    pub missing: Vec<String>,
}
/// The environment of an attempt (spec 0017 §5.4): the profile's `pinned`
/// one, else the one the attempt it continues ran in (`kept`), else the
/// binding's `default_environment` if it provides every need, else the
/// allowed environment that provides all of them with the fewest extra
/// capabilities (the first listed on a tie). Whatever is chosen must provide
/// every need; `Missing` names the labels the first candidate lacks.
pub fn place(
    binding: &ProjectBinding,
    pinned: Option<&str>,
    kept: Option<&str>,
    needs: &[String],
    provides: impl Fn(&str) -> Vec<String>,
) -> Result<String, Missing> {
    let lacking = |environment: &str| -> Vec<String> {
        let provided = provides(environment);
        (needs.iter())
            .filter(|n| !provided.contains(n))
            .cloned()
            .collect()
    };
    let fixed = pinned.or(kept);
    let default = binding.default_environment.as_deref();
    let fits = (binding.allowed_environments.iter())
        .filter(|e| lacking(e).is_empty())
        .min_by_key(|e| provides(e).len());
    let chosen = match fixed {
        Some(environment) => Some(environment),
        None => (default.filter(|d| lacking(d).is_empty())).or(fits.map(String::as_str)),
    };
    if let Some(environment) = chosen.filter(|e| lacking(e).is_empty()) {
        return Ok(environment.to_owned());
    }
    let first = fixed
        .or(default)
        .or(binding.allowed_environments.first().map(String::as_str))
        .unwrap_or_default();
    Err(Missing {
        environment: first.to_owned(),
        missing: lacking(first),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding(default: Option<&str>) -> ProjectBinding {
        let mut b = crate::tests::binding();
        b.allowed_environments = vec!["rust".into(), "web".into(), "full".into()];
        b.default_environment = default.map(Into::into);
        b
    }
    fn provides(environment: &str) -> Vec<String> {
        let labels: &[&str] = match environment {
            "rust" => &["cpp", "rust"],
            "web" => &["node"],
            "full" => &["cpp", "node", "playwright", "rust"],
            _ => &[],
        };
        labels.iter().map(|l| l.to_string()).collect()
    }
    fn needs(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|l| l.to_string()).collect()
    }
    fn place(
        default: Option<&str>,
        pinned: Option<&str>,
        kept: Option<&str>,
        n: &[&str],
    ) -> Result<String, Missing> {
        super::place(&binding(default), pinned, kept, &needs(n), provides)
    }

    #[test]
    fn the_default_environment_is_one_of_the_allowed() {
        let config = crate::ProjectConfig::default();
        assert!(binding(Some("web")).validate(&config).is_ok());
        assert!(binding(None).validate(&config).is_ok());
        assert_eq!(
            binding(Some("other")).validate(&config).unwrap_err(),
            "default environment is not an allowed environment"
        );
    }
    #[test]
    fn the_default_environment_takes_a_task_it_can_serve() {
        assert_eq!(place(Some("web"), None, None, &[]).unwrap(), "web");
        assert_eq!(place(Some("rust"), None, None, &["rust"]).unwrap(), "rust");
        // Without a default, a task that needs nothing goes to the smallest.
        assert_eq!(place(None, None, None, &[]).unwrap(), "web");
    }
    #[test]
    fn otherwise_the_best_fit_with_the_fewest_extra_capabilities() {
        assert_eq!(place(Some("web"), None, None, &["rust"]).unwrap(), "rust");
        assert_eq!(
            place(Some("web"), None, None, &["node", "rust"]).unwrap(),
            "full"
        );
        assert_eq!(place(Some("rust"), None, None, &["node"]).unwrap(), "web");
    }
    #[test]
    fn a_pinned_profile_environment_wins_and_a_continuation_keeps_its_own() {
        assert_eq!(place(Some("web"), Some("full"), None, &[]).unwrap(), "full");
        assert_eq!(place(Some("web"), None, Some("rust"), &[]).unwrap(), "rust");
        assert_eq!(
            place(Some("web"), Some("full"), Some("rust"), &[]).unwrap(),
            "full"
        );
    }
    #[test]
    fn a_missing_capability_is_named() {
        assert_eq!(
            place(Some("web"), None, None, &["node", "python"]),
            Err(Missing {
                environment: "web".into(),
                missing: needs(&["python"])
            })
        );
        // A pinned environment is never swapped for one that would fit.
        assert_eq!(
            place(Some("web"), Some("rust"), None, &["node"]),
            Err(Missing {
                environment: "rust".into(),
                missing: needs(&["node"])
            })
        );
    }
    #[test]
    fn needs_and_provides_are_read_from_metadata_and_definition_bodies() {
        assert_eq!(
            super::needs(&json!({NEEDS_KEY: ["rust", "node", "rust", 7]})),
            needs(&["node", "rust"])
        );
        assert!(super::needs(&json!({NEEDS_KEY: "rust"})).is_empty());
        assert!(check_needs(&needs(&["rust", "rust"])).is_err());
        assert!(check_needs(&needs(&["c++"])).is_err());
        assert!(check_needs(&needs(&["rust", "node"])).is_ok());
        let body = json!({"schema_version":1,"template":{},"provides":["rust","cpp"]}).to_string();
        assert_eq!(super::provides(&body), needs(&["cpp", "rust"]));
        assert!(super::provides("confined-worktree").is_empty());
        let old = json!({"provides":["rust"]}).to_string();
        let defs = [
            ("aq-rust", "1", old.as_str()),
            ("aq-rust", "2", body.as_str()),
            ("web", "1", "x"),
        ];
        assert_eq!(provided("aq-rust@2", defs), needs(&["cpp", "rust"]));
        assert_eq!(provided("aq-rust", defs), needs(&["rust"]));
        assert!(provided("absent", defs).is_empty());
    }
}
