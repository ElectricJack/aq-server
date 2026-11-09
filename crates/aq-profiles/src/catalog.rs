use crate::*;
use aq_session_api::Permission;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

pub const MAX_CATALOG_BYTES: usize = 1_048_576;
pub const MAX_INSTRUCTION_BYTES: usize = 65_536;
pub const MAX_MODULES: usize = 128;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema_version: u32,
    pub modules: Vec<Module>,
    pub agents: Vec<Agent>,
    pub intelligence: Vec<Intelligence>,
    pub execution: Vec<Execution>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModuleKind {
    Behavior,
    Capability,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Startup,
    OnDemand,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub permission: Permission,
    pub required: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub id: String,
    pub version: u32,
    pub kind: ModuleKind,
    pub purpose: String,
    pub instruction: String,
    pub delivery: Delivery,
    pub required: bool,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    #[serde(default)]
    pub required_adapters: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub id: String,
    pub version: u32,
    pub role: String,
    pub behaviors: Vec<String>,
    pub capabilities: Vec<String>,
    pub intelligence: String,
    pub execution: String,
    #[serde(default)]
    pub allowed_overrides: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCandidate {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_configuration_reference: String,
    pub harnesses: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intelligence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub id: String,
    pub version: u32,
    pub candidates: Vec<ModelCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_policy: Option<SelectionPolicy>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub id: String,
    pub version: u32,
    pub harnesses: Vec<String>,
    pub environments: Vec<String>,
    pub context_limit_bytes: u64,
    pub permissions: Vec<Permission>,
    pub credential_profile_references: Vec<String>,
    /// Harness-confinement permission, delivered to the worker inside the
    /// retained execution-profile definition. Omitted when default (deny
    /// container), so existing catalog identities are unchanged.
    #[serde(default, skip_serializing_if = "ExecutionConfinement::is_default")]
    pub confinement: ExecutionConfinement,
}
/// Mirrors aq-worker `runner::sandbox::confinement::ExecutionConfinement`.
/// Default: native confinement only, container confinement denied.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfinement {
    #[serde(default)]
    pub permit_container: bool,
    #[serde(default)]
    pub require_native: bool,
    /// MS1: the profile's harness always runs inside a node-owned sandbox
    /// container (the docker-local Claude driver), whatever the node's B1
    /// placement facts say. Implies `permit_container`. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_container: bool,
}
impl ExecutionConfinement {
    /// Container permission and native requirement exclude each other;
    /// requiring container confinement needs its permission.
    pub fn consistent(&self) -> bool {
        !matches!(
            (
                self.permit_container,
                self.require_native,
                self.require_container
            ),
            (true, true, _) | (false, _, true)
        )
    }
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogArtifact {
    pub schema_version: u32,
    pub catalog: Catalog,
    /// Exact UTF-8 Markdown bytes indexed by versioned module identity.
    #[serde(deserialize_with = "unique_string_map")]
    pub instructions: BTreeMap<String, String>,
    /// Canonical metadata + exact Markdown identity hashes, namespace separated.
    #[serde(deserialize_with = "unique_string_map")]
    pub identities: BTreeMap<String, String>,
}
#[derive(Debug, Clone)]
pub struct ValidatedCatalog {
    artifact: CatalogArtifact,
    digest: String,
}
impl ValidatedCatalog {
    pub fn artifact(&self) -> &CatalogArtifact {
        &self.artifact
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let mut v =
            serde_json::to_value(&self.artifact).map_err(|e| Error::Invalid(e.to_string()))?;
        v.sort_all_objects();
        serde_json::to_vec(&v).map_err(|e| Error::Invalid(e.to_string()))
    }
    pub fn from_artifact(bytes: &[u8]) -> Result<Self> {
        ensure(
            bytes.len() <= 2 * MAX_CATALOG_BYTES,
            "artifact exceeds bound",
        )?;
        let artifact: CatalogArtifact =
            serde_json::from_slice(bytes).map_err(|e| Error::Invalid(e.to_string()))?;
        let result = validate(artifact.catalog, artifact.instructions)?;
        ensure(
            result.artifact.identities == artifact.identities && artifact.schema_version == 1,
            "artifact identity hash mismatch",
        )?;
        Ok(result)
    }
    pub fn source(&self, namespace: &str, key: &str) -> Result<aq_session_api::Source> {
        let identity = format!("{namespace}:{key}");
        Ok(aq_session_api::Source {
            identity: identity.clone(),
            digest: self
                .artifact
                .identities
                .get(&identity)
                .ok_or_else(|| Error::Invalid("missing identity".into()))?
                .clone(),
            revision: key.rsplit_once('@').unwrap().1.into(),
        })
    }
}
pub fn key(id: &str, version: u32) -> String {
    format!("{id}@{version}")
}
fn identity(id: &str, version: u32) -> Result<()> {
    ensure(
        version > 0
            && !id.is_empty()
            && id.len() <= 96
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "identity must be bounded ASCII id and positive version",
    )
}
fn reference(s: &str) -> Result<()> {
    let (id, version) = s
        .rsplit_once('@')
        .ok_or_else(|| Error::Invalid("reference must be id@version".into()))?;
    identity(
        id,
        version
            .parse()
            .map_err(|_| Error::Invalid("bad reference version".into()))?,
    )
}
fn bounded<T>(v: &[T]) -> Result<()> {
    ensure(v.len() <= MAX_MODULES, "definition list bound")
}
fn distinct(v: &[String]) -> Result<()> {
    bounded(v)?;
    let mut seen = BTreeSet::new();
    for s in v {
        text(s)?;
        ensure(seen.insert(s), "duplicate reference")?;
    }
    Ok(())
}
fn permissions(v: &[Permission]) -> Result<()> {
    bounded(v)?;
    let mut set = BTreeSet::new();
    for p in v {
        text(&p.operation)?;
        text(&p.scope_reference)?;
        ensure(
            p.scope_reference.contains(':'),
            "scope must be logical kind:identity reference",
        )?;
        ensure(
            set.insert((&p.operation, &p.scope_reference)),
            "duplicate permission",
        )?;
    }
    Ok(())
}
/// Load a single explicitly named catalog file, never provider homes or task prose.
pub fn load(directory: &Path) -> Result<ValidatedCatalog> {
    use std::os::fd::FromRawFd;
    let descriptor = nix::fcntl::open(
        directory,
        nix::fcntl::OFlag::O_RDONLY
            | nix::fcntl::OFlag::O_DIRECTORY
            | nix::fcntl::OFlag::O_NOFOLLOW
            | nix::fcntl::OFlag::O_NONBLOCK
            | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    // SAFETY: open returned a fresh directory descriptor, transferred exactly once.
    let root = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let data = read_beneath(&root, Path::new("catalog.toml"), MAX_CATALOG_BYTES)?;
    let catalog: Catalog = toml::from_str(&data).map_err(|e| Error::Invalid(e.to_string()))?;
    bounded(&catalog.modules)?;
    let mut instructions = BTreeMap::new();
    let mut total = data.len();
    for m in &catalog.modules {
        let path = Path::new(&m.instruction);
        ensure(
            path.extension().is_some_and(|s| s == "md")
                && path.components().all(|c| matches!(c, Component::Normal(_))),
            "instruction must be relative Markdown path",
        )?;
        let body = read_beneath(&root, path, MAX_INSTRUCTION_BYTES)?;
        total = total
            .checked_add(body.len())
            .ok_or_else(|| Error::Invalid("catalog overflow".into()))?;
        ensure(total <= MAX_CATALOG_BYTES, "aggregate catalog byte bound")?;
        ensure(
            instructions.insert(key(&m.id, m.version), body).is_none(),
            "duplicate module",
        )?;
    }
    validate(catalog, instructions)
}
pub fn validate(
    catalog: Catalog,
    instructions: BTreeMap<String, String>,
) -> Result<ValidatedCatalog> {
    ensure(catalog.schema_version == 1, "catalog version")?;
    bounded(&catalog.modules)?;
    bounded(&catalog.agents)?;
    bounded(&catalog.intelligence)?;
    bounded(&catalog.execution)?;
    ensure(!catalog.agents.is_empty(), "catalog requires agent recipe")?;
    let mut identities = BTreeMap::new();
    let mut insert =
        |namespace: &str, id: &str, version: u32, value: serde_json::Value| -> Result<()> {
            identity(id, version)?;
            ensure(
                identities
                    .insert(format!("{namespace}:{}", key(id, version)), digest(&value)?)
                    .is_none(),
                "duplicate definition identity",
            )
        };
    let modules: BTreeMap<_, _> = catalog
        .modules
        .iter()
        .map(|m| (key(&m.id, m.version), m))
        .collect();
    ensure(
        modules.len() == instructions.len() && instructions.len() == catalog.modules.len(),
        "instruction identity coverage",
    )?;
    let mut total = 0;
    for m in &catalog.modules {
        text(&m.purpose)?;
        text(&m.instruction)?;
        let path = Path::new(&m.instruction);
        ensure(
            path.extension().is_some_and(|s| s == "md")
                && path.components().all(|c| matches!(c, Component::Normal(_))),
            "instruction must be relative Markdown path",
        )?;
        distinct(&m.dependencies)?;
        distinct(&m.required_adapters)?;
        for d in &m.dependencies {
            reference(d)?;
            ensure(modules.contains_key(d), "missing module dependency")?;
            ensure(
                m.kind != ModuleKind::Behavior || modules[d].kind == ModuleKind::Behavior,
                "behavior cannot depend on capability",
            )?;
        }
        ensure(
            !m.required || m.delivery == Delivery::Startup,
            "required module cannot be deferred",
        )?;
        ensure(
            m.kind == ModuleKind::Capability || m.requirements.is_empty(),
            "behavior cannot request grants",
        )?;
        permissions(
            &m.requirements
                .iter()
                .map(|r| r.permission.clone())
                .collect::<Vec<_>>(),
        )?;
        let body = instructions
            .get(&key(&m.id, m.version))
            .ok_or_else(|| Error::Invalid("missing Markdown".into()))?;
        ensure(
            !body.trim().is_empty() && body.len() <= MAX_INSTRUCTION_BYTES,
            "Markdown bounds",
        )?;
        total += body.len();
        ensure(total <= MAX_CATALOG_BYTES, "aggregate instruction bounds")?;
        insert(
            "module",
            &m.id,
            m.version,
            serde_json::json!({"metadata":m,"markdown":body}),
        )?;
    }
    for a in &catalog.agents {
        text(&a.role)?;
        distinct(&a.behaviors)?;
        distinct(&a.capabilities)?;
        distinct(&a.allowed_overrides)?;
        ensure(
            a.allowed_overrides
                .iter()
                .all(|f| f == "context_limit_bytes"),
            "protected override field",
        )?;
        for (refs, kind) in [
            (&a.behaviors, ModuleKind::Behavior),
            (&a.capabilities, ModuleKind::Capability),
        ] {
            for r in refs {
                reference(r)?;
                ensure(
                    modules.get(r).is_some_and(|m| m.kind == kind),
                    "missing or wrong-kind module",
                )?;
            }
        }
        reference(&a.intelligence)?;
        reference(&a.execution)?;
        ensure(
            catalog
                .intelligence
                .iter()
                .any(|i| key(&i.id, i.version) == a.intelligence),
            "missing intelligence",
        )?;
        ensure(
            catalog
                .execution
                .iter()
                .any(|e| key(&e.id, e.version) == a.execution),
            "missing execution",
        )?;
        insert("agent", &a.id, a.version, serde_json::to_value(a).unwrap())?;
    }
    for i in &catalog.intelligence {
        if let Some(name) = &i.name {
            text(name)?;
            ensure(name.len() <= 128, "profile name bound")?;
        }
        if let Some(description) = &i.description {
            ensure(
                description.len() <= 2000 && !description.contains('\0'),
                "profile description bound",
            )?;
        }
        if let Some(policy) = &i.selection_policy {
            policy.validate()?;
        }
        bounded(&i.candidates)?;
        ensure(
            !i.candidates.is_empty(),
            "intelligence requires explicit candidates",
        )?;
        let mut names = BTreeSet::new();
        for c in &i.candidates {
            text(&c.id)?;
            text(&c.provider)?;
            text(&c.model)?;
            text(&c.reasoning_configuration_reference)?;
            distinct(&c.harnesses)?;
            ensure(
                !c.harnesses.is_empty() && names.insert(&c.id),
                "candidate harness/identity",
            )?;
        }
        insert(
            "intelligence",
            &i.id,
            i.version,
            serde_json::to_value(i).unwrap(),
        )?;
    }
    for e in &catalog.execution {
        distinct(&e.harnesses)?;
        distinct(&e.environments)?;
        distinct(&e.credential_profile_references)?;
        permissions(&e.permissions)?;
        ensure(
            !e.harnesses.is_empty()
                && !e.environments.is_empty()
                && e.context_limit_bytes > 0
                && e.context_limit_bytes <= 1_048_576,
            "execution bounds",
        )?;
        ensure(
            e.confinement.consistent(),
            "execution confinement contradictory",
        )?;
        insert(
            "execution",
            &e.id,
            e.version,
            serde_json::to_value(e).unwrap(),
        )?;
    }
    let artifact = CatalogArtifact {
        schema_version: 1,
        catalog,
        instructions,
        identities,
    };
    // Validate *every* DAG, even definitions not selected by a baseline recipe.
    for m in &artifact.catalog.modules {
        expand(&artifact, &[key(&m.id, m.version)])?;
    }
    for agent in &artifact.catalog.agents {
        let mut roots = agent.behaviors.clone();
        roots.extend(agent.capabilities.clone());
        expand(&artifact, &roots)?;
        let required_roots: Vec<_> = roots
            .into_iter()
            .filter(|name| {
                artifact
                    .catalog
                    .modules
                    .iter()
                    .any(|m| key(&m.id, m.version) == *name && m.required)
            })
            .collect();
        for dependency in expand(&artifact, &required_roots)? {
            ensure(
                artifact
                    .catalog
                    .modules
                    .iter()
                    .find(|m| key(&m.id, m.version) == dependency)
                    .unwrap()
                    .delivery
                    == Delivery::Startup,
                "required dependency cannot be deferred",
            )?;
        }
    }
    ensure(
        serde_json::to_vec(&artifact).unwrap().len() <= 2 * MAX_CATALOG_BYTES,
        "artifact byte bound",
    )?;
    let hash = digest(&artifact)?;
    Ok(ValidatedCatalog {
        artifact,
        digest: hash,
    })
}
/// Stable dependency-first DFS; conflicting versions of one logical module refuse.
pub fn expand(artifact: &CatalogArtifact, roots: &[String]) -> Result<Vec<String>> {
    fn visit(
        key: &str,
        a: &CatalogArtifact,
        visiting: &mut BTreeSet<String>,
        seen: &mut BTreeMap<String, String>,
        out: &mut Vec<String>,
    ) -> Result<()> {
        let m = a
            .catalog
            .modules
            .iter()
            .find(|m| crate::key(&m.id, m.version) == key)
            .ok_or_else(|| Error::Invalid("missing module".into()))?;
        if let Some(old) = seen.get(&m.id) {
            return ensure(old == key, "incompatible module versions");
        }
        ensure(visiting.insert(key.into()), "module dependency cycle")?;
        ensure(visiting.len() <= 32, "module dependency depth")?;
        for d in &m.dependencies {
            visit(d, a, visiting, seen, out)?;
        }
        visiting.remove(key);
        seen.insert(m.id.clone(), key.into());
        out.push(key.into());
        ensure(out.len() <= 128, "expanded modules")
    }
    let mut out = Vec::new();
    let mut seen = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    for root in roots {
        visit(root, artifact, &mut visiting, &mut seen, &mut out)?;
    }
    Ok(out)
}

fn unique_string_map<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, String>, D::Error> {
    struct Unique;
    impl<'de> serde::de::Visitor<'de> for Unique {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("unique bounded string map")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if result.len() >= 512 || result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom(
                        "duplicate or excessive artifact map entries",
                    ));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Unique)
}

/// Descriptor-relative no-follow opens prevent component replacement from escaping
/// the explicitly opened root. Nonblocking open rejects FIFO/device inputs without
/// waiting, and a cap+1 read remains bounded if a regular file grows after stat.
fn read_beneath(root: &std::fs::File, path: &Path, limit: usize) -> Result<String> {
    use nix::{
        fcntl::{OFlag, openat},
        sys::stat::Mode,
    };
    use std::os::fd::{AsRawFd, FromRawFd};
    let parts: Vec<_> = path.components().collect();
    ensure(
        !parts.is_empty() && parts.iter().all(|p| matches!(p, Component::Normal(_))),
        "relative normal path required",
    )?;
    let mut current = root.try_clone()?;
    for (index, part) in parts.iter().enumerate() {
        let mut flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
        if index + 1 < parts.len() {
            flags |= OFlag::O_DIRECTORY;
        }
        let fd = openat(
            Some(current.as_raw_fd()),
            part.as_os_str(),
            flags,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        // SAFETY: openat returned a new owned descriptor; File is its only owner.
        current = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    read_handle(current, limit)
}
fn read_handle(current: std::fs::File, limit: usize) -> Result<String> {
    use std::io::Read;
    ensure(
        limit > 0 && limit <= 2 * MAX_CATALOG_BYTES,
        "file read limit",
    )?;
    let metadata = current.metadata()?;
    ensure(
        metadata.is_file() && metadata.len() <= limit as u64,
        "input must be bounded regular file",
    )?;
    let mut bytes = Vec::new();
    current.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure(bytes.len() <= limit, "input grew beyond byte bound")?;
    String::from_utf8(bytes).map_err(|_| Error::Invalid("UTF-8 text required".into()))
}

/// Bounded named local input, with no-follow/nonblocking regular-file checks.
/// Unlike catalog instruction paths, this explicitly selected file is not rooted
/// in a catalog directory. Its contents never select another filesystem path.
pub fn read_bounded_regular(path: &Path, limit: usize) -> Result<String> {
    use nix::{
        fcntl::{OFlag, open},
        sys::stat::Mode,
    };
    use std::os::fd::FromRawFd;
    let descriptor = open(
        path,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    // SAFETY: open returned a new owned descriptor, transferred exactly once.
    read_handle(unsafe { std::fs::File::from_raw_fd(descriptor) }, limit)
}
