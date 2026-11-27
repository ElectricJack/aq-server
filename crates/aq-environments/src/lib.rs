//! Spec 0017 §6.3: environment versions live in the database, beside the
//! boot config's definitions and template allowlist (which seed the AQ base
//! and today's environment). Every reader of environment definitions and
//! templates goes through one [`Catalog`]. Empty without `project_environments`.
#![cfg(feature = "project_environments")]
use aq_admission::{ArtifactRef, Digest};
use aq_environment_templates::Allowlist;
use aq_project_policy::{ProjectBinding, environments as env};
use aq_role_settings::refusals::OfferRefusal;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

mod agent;
mod harnesses;
pub use harnesses::{Running, boot_adapter, listed};
#[cfg(feature = "delivery_playbook")]
pub mod commands;
#[cfg(feature = "delivery_playbook")]
pub mod git;
mod store;
pub mod worker;

/// What the boot config holds: each definition's exact reference and body,
/// and the operator's template allowlist.
pub struct BootEnvironments<'a> {
    pub definitions: Vec<(&'a ArtifactRef, &'a str)>,
    pub templates: Option<&'a Allowlist>,
}
impl<'a> BootEnvironments<'a> {
    pub fn new(
        definitions: impl IntoIterator<Item = (&'a ArtifactRef, &'a str)>,
        templates: Option<&'a Allowlist>,
    ) -> Self {
        let definitions = definitions.into_iter().collect();
        Self {
            definitions,
            templates,
        }
    }
}
/// The template an environment runs: its image digest, and where to pull it
/// (none for a boot allowlist entry, which nodes already list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub environment: String,
    pub version: String,
    pub digest: Digest,
    pub image: Option<String>,
}
/// An admitted version as nodes are told it (`GET /v1/worker/environments`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdmittedEnvironment {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub image: String,
    pub harnesses: Value,
    pub warm: Value,
    /// The definition's reference `{id, version, digest}`: what a node lists
    /// for placement, so the offer path retains exactly that body.
    pub definition: Value,
}

pub struct Catalog<'a> {
    boot: Vec<(&'a ArtifactRef, &'a str)>,
    boot_templates: Vec<Admitted>,
    /// Newest first.
    versions: Vec<store::Version>,
    /// The boot docker-local adapters derived for each version.
    derived: Vec<harnesses::Derived>,
}
impl<'a> Catalog<'a> {
    /// Read on `db`: the fence passes its own transaction.
    pub async fn load<'e>(
        db: impl sqlx::PgExecutor<'e>,
        boot: &BootEnvironments<'a>,
    ) -> sqlx::Result<Self> {
        Ok(Self::new(boot, store::versions(db).await?))
    }
    /// The boot config's alone, for a store without the versions table.
    pub fn boot(boot: &BootEnvironments<'a>) -> Self {
        Self::new(boot, Vec::new())
    }
    fn new(boot: &BootEnvironments<'a>, versions: Vec<store::Version>) -> Self {
        let boot_templates = (boot.templates.into_iter().flat_map(Allowlist::entries))
            .map(|(name, version, digest)| Admitted {
                environment: name.into(),
                version: version.into(),
                digest: digest.clone(),
                image: None,
            })
            .collect();
        Self {
            boot: boot.definitions.clone(),
            boot_templates,
            derived: harnesses::derive_all(&boot.definitions, &versions),
            versions,
        }
    }
    /// Every definition `(id, version, body)`: the admitted versions, newest
    /// first, then the boot config's.
    pub fn definitions(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        let admitted = (self.versions.iter()).map(|v| {
            let a = &v.admitted;
            (a.environment.as_str(), a.version.as_str(), v.body.as_str())
        });
        let boot = (self.boot.iter()).map(|(r, body)| (r.id.as_str(), r.version.as_str(), *body));
        admitted.chain(boot)
    }
    /// The exact body of the definition `reference` names, digest included.
    pub fn definition(&self, reference: &ArtifactRef) -> Option<&str> {
        let admitted = self.versions.iter().find(|v| {
            v.admitted.environment == reference.id.as_str()
                && v.admitted.version == reference.version.as_str()
                && v.body_digest == reference.digest
        });
        let derived = self.derived.iter().find(|d| d.artifact == *reference);
        (admitted.map(|v| v.body.as_str()))
            .or_else(|| derived.map(|d| d.body.as_str()))
            .or_else(|| {
                let boot = self.boot.iter().find(|(r, _)| *r == reference);
                boot.map(|(_, body)| *body)
            })
    }
    fn admitted(&self, environment: &str) -> Option<&store::Version> {
        self.versions
            .iter()
            .find(|v| matches(&v.admitted, environment))
    }
    /// The template `environment` (`name` or `name@version`) runs: its
    /// admitted version (the newest for a bare name), else the one digest the
    /// boot allowlist pins it to; none when that names none or several.
    pub fn template(&self, environment: &str) -> Option<&Admitted> {
        if let Some(version) = self.admitted(environment) {
            return Some(&version.admitted);
        }
        let mut boot = (self.boot_templates.iter()).filter(|a| matches(a, environment));
        let first = boot.next()?;
        boot.all(|a| a.digest == first.digest).then_some(first)
    }
    /// What `environment` provides: an admitted one, what its version (the
    /// newest for a bare name) provides, so older versions do not narrow it;
    /// a boot one, what every definition of it provides.
    pub fn provides(&self, environment: &str) -> Vec<String> {
        match self.admitted(environment) {
            Some(v) => {
                let a = &v.admitted;
                let body = (a.environment.as_str(), a.version.as_str(), v.body.as_str());
                env::provided(environment, [body])
            }
            None => env::provided(environment, self.definitions()),
        }
    }
    /// The platform text the admitted version of `environment` carries; none
    /// for a boot environment, whose text the server ships.
    pub fn platform_text(&self, environment: &str) -> Option<&str> {
        self.admitted(environment)?.platform_text.as_deref()
    }
    /// Every admitted version, newest first, for nodes to pull and report.
    pub fn admitted_for_nodes(&self) -> Vec<AdmittedEnvironment> {
        (self.versions.iter())
            .map(|v| AdmittedEnvironment {
                id: v.admitted.environment.clone(),
                version: v.admitted.version.clone(),
                digest: v.template.clone(),
                image: v.image.clone(),
                harnesses: v.harnesses.clone(),
                warm: v.warm.clone(),
                // The same reference its harness entries carry.
                definition: serde_json::json!(harnesses::reference(v)),
            })
            .collect()
    }
}
fn matches(admitted: &Admitted, environment: &str) -> bool {
    match environment.split_once('@') {
        Some((name, version)) => admitted.environment == name && admitted.version == version,
        None => admitted.environment == environment,
    }
}

/// Spec 0017 §5.4: the default rule of [`env::place`], fed the profile's pin,
/// the environment the `predecessor` attempt ran in or else the one the
/// router chose (`routed`), and what the catalog's definitions provide. `Err`
/// is the hold the task's work read shows.
#[allow(clippy::too_many_arguments)]
pub async fn place(
    pool: &PgPool,
    catalog: &Catalog<'_>,
    binding: &ProjectBinding,
    pinned: Option<&str>,
    routed: Option<&str>,
    metadata: &Value,
    predecessor: Option<Uuid>,
    services: &[String],
) -> Result<(String, bool), OfferRefusal> {
    let kept = match predecessor {
        Some(attempt) => aq_role_settings::environments::ran_in(pool, attempt).await,
        None => None,
    };
    let provides = |e: &str| catalog.provides(e);
    let needs = env::needs(metadata);
    let fixed = env::FixedEnvironments {
        pinned,
        kept: kept.as_deref().or(routed),
    };
    env::place_with_services(binding, fixed, &needs, services, provides).map_err(
        |env::Missing {
             environment,
             missing,
         }| OfferRefusal::EnvironmentMissingCapability {
            environment,
            missing,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }
    fn version(id: &str, version: &str, c: char) -> store::Version {
        let template = format!("sha256:{}", digest(c));
        store::version((
            id.into(),
            version.into(),
            template.clone(),
            format!("registry.example/x@{template}"),
            json!(["rust"]),
            Some(format!("{id} {version}")),
            json!([]),
            json!([]),
            json!([]),
        ))
        .unwrap()
    }

    #[test]
    fn admitted_versions_shadow_the_boot_allowlist_and_a_bare_name_is_the_newest() {
        let allowlist = json!({"schema_version":1,"entries":[
            {"name":"ci","version":"1","image_digest":format!("sha256:{}", digest('a'))},
            {"name":"ci","version":"2","image_digest":format!("sha256:{}", digest('b'))},
            {"name":"dev","version":"1","image_digest":format!("sha256:{}", digest('c'))},
            {"name":"p.web","version":"0","image_digest":format!("sha256:{}", digest('c'))},
        ]});
        let allowlist = Allowlist::parse(allowlist.to_string().as_bytes()).unwrap();
        let boot = BootEnvironments {
            definitions: vec![],
            templates: Some(&allowlist),
        };
        let catalog = Catalog::new(
            &boot,
            vec![version("p.web", "2", 'e'), version("p.web", "1", 'd')],
        );
        let digest_of = |e| catalog.template(e).map(|a| a.digest.as_str().to_owned());
        assert_eq!(digest_of("p.web"), Some(digest('e')));
        assert_eq!(digest_of("p.web@1"), Some(digest('d')));
        assert_eq!(digest_of("dev"), Some(digest('c')));
        assert_eq!(digest_of("ci@2"), Some(digest('b')));
        assert_eq!(digest_of("ci"), None, "two boot digests are ambiguous");
        assert_eq!(digest_of("ci@3"), None);
        assert_eq!(catalog.platform_text("p.web"), Some("p.web 2"));
        assert_eq!(catalog.platform_text("dev"), None);
        assert_eq!(env::provided("p.web", catalog.definitions()), ["rust"]);
    }

    #[test]
    fn a_bare_name_provides_what_its_newest_version_provides() {
        let boot = BootEnvironments::new([], None);
        let mut newest = version("p.web", "2", 'e');
        newest.body = newest.body.replace(r#"["rust"]"#, r#"["node","rust"]"#);
        let catalog = Catalog::new(&boot, vec![newest, version("p.web", "1", 'd')]);
        assert_eq!(catalog.provides("p.web"), ["node", "rust"]);
        assert_eq!(catalog.provides("p.web@1"), ["rust"]);
    }
}
