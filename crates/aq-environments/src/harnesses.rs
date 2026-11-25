//! Spec 0017 §6.3: the harness entries a node lists for admitted
//! environments, and the result-adapter Definition each one runs with.
//!
//! A node runs an admitted environment with its docker-local result adapter
//! (Claude or Codex) whose Definition is the boot one with only `environment`
//! replaced. That Definition is derived here on demand, never stored, so it
//! goes when its environment version is pruned.
use crate::{Catalog, store::Version};
use aq_admission::{ArtifactRef, DOCKER_LOCAL_PROFILES, Digest, Text};
use aq_selection::InstalledHarness;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

/// A boot docker-local adapter Definition with `environment` replaced by an
/// admitted version's definition.
pub(crate) struct Derived {
    base: ArtifactRef,
    base_environment: ArtifactRef,
    target: ArtifactRef,
    pub(crate) artifact: ArtifactRef,
    pub(crate) body: String,
}
/// The definition reference nodes are told for `version`.
pub(crate) fn reference(version: &Version) -> Option<ArtifactRef> {
    Some(ArtifactRef {
        id: Text::new(version.admitted.environment.as_str()).ok()?,
        version: Text::new(version.admitted.version.as_str()).ok()?,
        digest: version.body_digest.clone(),
    })
}
/// Every derivation of the boot docker-local adapters for `versions`.
pub(crate) fn derive_all(boot: &[(&ArtifactRef, &str)], versions: &[Version]) -> Vec<Derived> {
    let sandbox = |r: &ArtifactRef| {
        DOCKER_LOCAL_PROFILES
            .iter()
            .any(|p| p.adapter == r.id.as_str())
    };
    let mut derived = Vec::new();
    for (base, body) in boot.iter().filter(|(r, _)| sandbox(r)) {
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            continue;
        };
        let Some(Ok(base_environment)) =
            (value.get("environment").cloned()).map(serde_json::from_value::<ArtifactRef>)
        else {
            continue;
        };
        for target in versions.iter().filter_map(reference) {
            let Some(d) = derive(base, &value, &target) else {
                continue;
            };
            derived.push(Derived {
                base: (*base).clone(),
                base_environment: base_environment.clone(),
                target,
                artifact: d.0,
                body: d.1,
            });
        }
    }
    derived
}
/// The canonical body (sorted keys, compact, as the node encodes it) and its
/// reference: the boot `id`, the revision `<boot version>+<id>@<version>`.
fn derive(base: &ArtifactRef, boot: &Value, target: &ArtifactRef) -> Option<(ArtifactRef, String)> {
    let mut value = boot.clone();
    value["environment"] = serde_json::to_value(target).ok()?;
    value.sort_all_objects();
    let body = serde_json::to_string(&value).ok()?;
    let version = format!(
        "{}+{}@{}",
        base.version.as_str(),
        target.id.as_str(),
        target.version.as_str()
    );
    let artifact = ArtifactRef {
        id: base.id.clone(),
        version: Text::new(version).ok()?,
        digest: Digest::of(body.as_bytes()),
    };
    Some((artifact, body))
}
/// The boot adapter a derived adapter reference was derived from, by its
/// revision (`<boot version>+…`); none for a boot adapter. It names the
/// result profile only: a derived reference's digest is checked where its
/// body is retained ([`Catalog::definition`]).
pub fn boot_adapter<'a>(
    adapter: &ArtifactRef,
    boot: impl IntoIterator<Item = &'a ArtifactRef>,
) -> Option<ArtifactRef> {
    let (version, _) = adapter.version.as_str().split_once('+')?;
    let mut boot = boot.into_iter();
    boot.find(|r| r.id == adapter.id && r.version.as_str() == version)
        .cloned()
}

impl Catalog<'_> {
    /// `entry`'s counterpart for `version`: the same harness entry with the
    /// version's definition as `environment` and the derived adapter, when
    /// the version's template carries that harness and version.
    fn counterpart(&self, entry: &InstalledHarness, version: &Version) -> Option<InstalledHarness> {
        let carried = (version.harnesses.as_array()?.iter()).any(|h| {
            h["harness"] == entry.harness.as_str() && h["version"] == entry.cli_version.as_str()
        });
        let derived = self.derived.iter().find(|d| {
            d.base == entry.adapter
                && d.base_environment == entry.environment
                && d.target.id.as_str() == version.admitted.environment
                && d.target.version.as_str() == version.admitted.version
        })?;
        carried.then(|| InstalledHarness {
            adapter: derived.artifact.clone(),
            environment: derived.target.clone(),
            ..entry.clone()
        })
    }
    /// The qualified harnesses: the configured ones, and each one's
    /// counterpart for every admitted version. A counterpart is qualified
    /// because it derives from a configured qualified harness.
    pub fn qualified(&self, configured: &[InstalledHarness]) -> Vec<InstalledHarness> {
        let derived = (self.versions.iter()).flat_map(|v| {
            configured
                .iter()
                .filter_map(move |e| self.counterpart(e, v))
        });
        configured.iter().cloned().chain(derived).collect()
    }
    /// What a node lists: its configured entries, then each counterpart a
    /// live attempt on it runs (`running`, matched exactly), then those for
    /// the newest version of each environment whose template it reported
    /// ready. Within `bound` entries the newest-version ones are left out last
    /// first, and named; an in-use entry is never left out. The third value
    /// counts the in-use entries.
    pub fn listed(
        &self,
        configured: Vec<InstalledHarness>,
        ready: &[String],
        running: &[Running],
        bound: usize,
    ) -> (Vec<InstalledHarness>, Vec<String>, usize) {
        let (mut in_use, mut newest) = (Vec::new(), Vec::new());
        let mut seen = std::collections::BTreeSet::new();
        for v in &self.versions {
            let first = seen.insert(v.admitted.environment.as_str());
            let a = &v.admitted;
            for h in configured.iter().filter_map(|e| self.counterpart(e, v)) {
                if running.iter().any(|r| r.runs(&h)) {
                    in_use.push(h);
                } else if first && ready.contains(&v.template) {
                    newest.push((format!("{}@{}", a.environment, a.version), h));
                }
            }
        }
        let using = in_use.len();
        let room = bound.saturating_sub(configured.len() + using);
        let mut left_out: Vec<String> = newest.iter().skip(room).map(|(e, _)| e.clone()).collect();
        left_out.dedup();
        let mut listed = configured;
        listed.extend(in_use);
        listed.extend(newest.into_iter().take(room).map(|(_, h)| h));
        (listed, left_out, using)
    }
}

/// The harness a live attempt runs, from its launch manifest.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Running {
    pub harness: String,
    pub adapter: ArtifactRef,
    pub environment: ArtifactRef,
}
impl Running {
    /// Whether `h` is exactly this harness, adapter and environment (digests too).
    fn runs(&self, h: &InstalledHarness) -> bool {
        self.harness == h.harness && self.adapter == h.adapter && self.environment == h.environment
    }
}
/// [`Catalog::listed`] for `node`, from its last ready report and the
/// environments its live attempts run in, within the snapshot's harness bound
/// ([`aq_selection::MAX_NODE_HARNESSES`]). What the bound leaves out is logged.
pub async fn listed(
    conn: &mut PgConnection,
    catalog: &Catalog<'_>,
    node: Uuid,
    configured: Vec<InstalledHarness>,
) -> sqlx::Result<Vec<InstalledHarness>> {
    if catalog.versions.is_empty() {
        return Ok(configured);
    }
    let ready: Option<Value> =
        sqlx::query_scalar("SELECT digests FROM environment_ready WHERE node_id=$1")
            .bind(node)
            .fetch_optional(&mut *conn)
            .await?;
    let ready: Vec<String> = ready
        .and_then(|r| serde_json::from_value(r).ok())
        .unwrap_or_default();
    let running: Vec<Value> = sqlx::query_scalar(
        "SELECT l.launch_manifest->'harness' FROM execution_attempts a \
         JOIN profiled_launches l USING(attempt_id) WHERE a.node_id=$1 AND a.holds_task",
    )
    .bind(node)
    .fetch_all(&mut *conn)
    .await?;
    let running: Vec<Running> = (running.into_iter())
        .filter_map(|h| serde_json::from_value(h).ok())
        .collect();
    let (bound, configured_count) = (aq_selection::MAX_NODE_HARNESSES, configured.len());
    let (listed, left_out, in_use) = catalog.listed(configured, &ready, &running, bound);
    if configured_count + in_use >= bound {
        tracing::warn!(%node, configured = configured_count, in_use, bound, "node's configured and in-use harness entries reach its snapshot bound of {bound}");
    }
    if !left_out.is_empty() {
        tracing::warn!(%node, environments = ?left_out, bound, "admitted environments not listed on node: its snapshot holds at most {bound} harnesses");
    }
    Ok(listed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BootEnvironments;
    use serde_json::json;

    fn r(id: &str, body: &str) -> ArtifactRef {
        serde_json::from_value(
            json!({"id": id, "version": "1", "digest": Digest::of(body.as_bytes())}),
        )
        .unwrap()
    }
    fn version(id: &str, n: &str, c: char) -> Version {
        let template = format!("sha256:{}", c.to_string().repeat(64));
        crate::store::version((
            id.into(),
            n.into(),
            template.clone(),
            format!("registry.example/x@{template}"),
            json!([]),
            None,
            json!([]),
            json!([{"harness": "claude", "version": "2.1.318"}, {"harness": "codex", "version": "0.166.0"}]),
            json!([]),
        ))
        .unwrap()
    }
    fn entry(adapter: &ArtifactRef, environment: &ArtifactRef) -> InstalledHarness {
        serde_json::from_value(json!({"harness": "claude", "cli_version": "2.1.318", "adapter": adapter,
            "environment": environment, "qualification": r("q", "q"), "failure_classifier": null, "capabilities": [],
            "limits": {"context_bytes": 1, "cpu_millis": 1, "hard_cost_enforcer": null, "max_output_tokens": 1,
                "memory_bytes": 1, "process_limit": 1, "wall_time_ms": 1}}))
        .unwrap()
    }

    /// Spec 0017 §6.3 rulings: configured entries first, then the versions a
    /// live attempt uses, then each environment's newest ready version; the
    /// bound leaves out newest-version entries only, last first.
    #[test]
    fn a_node_lists_in_use_then_newest_ready_versions_within_the_bound() {
        let boot_env = r("confined-worktree", "env");
        let body =
            json!({"environment": boot_env, "profile": "p", "schema_version": 1}).to_string();
        let adapter = r("aq.claude-result-local-adapter", &body);
        let plain = r("plain-adapter", "plain");
        let definitions = vec![(&adapter, body.as_str()), (&plain, "plain")];
        let boot = BootEnvironments::new(definitions, None);
        let versions = vec![
            version("a", "2", 'a'),
            version("a", "1", 'b'),
            version("b", "1", 'c'),
        ];
        let catalog = Catalog::new(&boot, versions);
        let configured = vec![entry(&adapter, &boot_env), entry(&plain, &boot_env)];
        let ready = |d: &[char]| {
            d.iter()
                .map(|c| format!("sha256:{}", c.to_string().repeat(64)))
                .collect::<Vec<_>>()
        };
        let envs = |l: &[InstalledHarness]| {
            l.iter()
                .map(|h| {
                    format!(
                        "{}@{}",
                        h.environment.id.as_str(),
                        h.environment.version.as_str()
                    )
                })
                .collect::<Vec<_>>()
        };
        // Nothing ready: only the configured entries.
        let (listed, left, _) = catalog.listed(configured.clone(), &[], &[], 16);
        assert_eq!(
            envs(&listed),
            ["confined-worktree@1", "confined-worktree@1"]
        );
        assert!(left.is_empty());
        // Ready a@1 is not the newest a: not listed; a@2 and b@1 are, for the
        // docker-local entry only, with the derived adapter.
        let (listed, _, _) = catalog.listed(configured.clone(), &ready(&['a', 'b', 'c']), &[], 16);
        assert_eq!(envs(&listed[2..]), ["a@2", "b@1"]);
        assert!(listed[2..].iter().all(|h| h.adapter.id == adapter.id && h.adapter.version.as_str().starts_with("1+")));
        assert_eq!(listed[2].adapter.version.as_str(), "1+a@2");
        // An attempt on a@1 keeps it listed, ahead of the newest, ready or not.
        let on = |h: &InstalledHarness| Running {
            harness: h.harness.clone(),
            adapter: h.adapter.clone(),
            environment: h.environment.clone(),
        };
        let a1 = catalog.qualified(&configured)[3].clone();
        assert_eq!(envs(std::slice::from_ref(&a1)), ["a@1"]);
        let used = [on(&a1)];
        let (listed, _, _) = catalog.listed(configured.clone(), &ready(&['a', 'c']), &used, 16);
        assert_eq!(envs(&listed[2..]), ["a@1", "a@2", "b@1"]);
        // A bound of 4 leaves out the last newest-version entry and names it;
        // a bound of 2 never leaves out the in-use one.
        let (listed, left, _) = catalog.listed(configured.clone(), &ready(&['a', 'c']), &used, 4);
        assert_eq!(
            (envs(&listed[2..]), left),
            (
                vec!["a@1".to_string(), "a@2".into()],
                vec!["b@1".to_string()]
            )
        );
        let (listed, left, using) =
            catalog.listed(configured.clone(), &ready(&['a', 'c']), &used, 2);
        assert_eq!(
            (envs(&listed[2..]), left),
            (
                vec!["a@1".to_string()],
                vec!["a@2".to_string(), "b@1".into()]
            )
        );
        assert_eq!(using, 1);
        // An attempt on another digest under the same id and version (for
        // example a boot environment's) pulls in no admitted counterpart.
        let mut other = on(&a1);
        other.environment.digest = boot_env.digest.clone();
        let (listed, _, using) = catalog.listed(configured.clone(), &[], &[other], 16);
        assert_eq!((listed.len(), using), (2, 0));
        // Qualified: every version's counterpart; each derived body resolves.
        let qualified = catalog.qualified(&configured);
        assert_eq!(envs(&qualified[2..]), ["a@2", "a@1", "b@1"]);
        for h in &qualified[2..] {
            let body = catalog.definition(&h.adapter).unwrap();
            assert_eq!(Digest::of(body.as_bytes()), h.adapter.digest);
            assert_eq!(
                boot_adapter(&h.adapter, [&adapter, &plain]),
                Some(adapter.clone())
            );
        }
        assert_eq!(boot_adapter(&adapter, [&adapter]), None);
    }

    /// Fix round 1: only the entry each attempt runs is listed in use, so a
    /// node with two configured harnesses and attempts in 8 environments
    /// lists 2 + 8 entries, within the bound of 16.
    #[test]
    fn only_the_entry_an_attempt_runs_is_listed_in_use() {
        let boot_env = r("confined-worktree", "env");
        let body = |p: &str| {
            json!({"environment": boot_env, "profile": p, "schema_version": 1}).to_string()
        };
        let (claude_body, codex_body) = (body("claude"), body("codex"));
        let claude = r("aq.claude-result-local-adapter", &claude_body);
        let codex = r("aq.codex-docker-result-local-adapter", &codex_body);
        let definitions = vec![
            (&claude, claude_body.as_str()),
            (&codex, codex_body.as_str()),
        ];
        let boot = BootEnvironments::new(definitions, None);
        let ids: Vec<String> = (0..8).map(|i| format!("e{i}")).collect();
        let versions = (ids.iter()).map(|id| version(id, "1", 'a')).collect();
        let catalog = Catalog::new(&boot, versions);
        let mut codex_entry = entry(&codex, &boot_env);
        (codex_entry.harness, codex_entry.cli_version) = ("codex".into(), "0.166.0".into());
        let configured = vec![entry(&claude, &boot_env), codex_entry];
        let qualified = catalog.qualified(&configured);
        assert_eq!(
            qualified.len(),
            2 + 16,
            "both harnesses in every environment"
        );
        let running: Vec<Running> = (qualified[2..].iter())
            .filter(|h| h.harness == "claude")
            .map(|h| Running {
                harness: h.harness.clone(),
                adapter: h.adapter.clone(),
                environment: h.environment.clone(),
            })
            .collect();
        assert_eq!(running.len(), 8);
        let (listed, left, using) = catalog.listed(configured, &[], &running, 16);
        assert_eq!((listed.len(), using), (2 + 8, 8));
        assert!(listed[2..].iter().all(|h| h.harness == "claude"));
        assert!(left.is_empty());
    }
}
