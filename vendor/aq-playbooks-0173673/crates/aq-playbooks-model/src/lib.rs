//! Versioned, bounded portable workflow format. See FORMAT.md for canonical rules.
pub mod schedules;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const FORMAT_VERSION: u32 = 1;
pub const MAX_ARTIFACT_BYTES: usize = 16_777_216;
pub type Json = Value;
pub type Hash = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schema {
    Null,
    Boolean,
    Integer {
        minimum: i64,
        maximum: i64,
    },
    String {
        max_length: usize,
        #[serde(default)]
        values: BTreeSet<String>,
    },
    Array {
        items: Box<Schema>,
        max_items: usize,
    },
    Object {
        properties: BTreeMap<String, Schema>,
        #[serde(default)]
        required: BTreeSet<String>,
    },
    Nullable {
        inner: Box<Schema>,
    },
    /// A bounded explicit union; a value must satisfy at least one alternative.
    Union {
        variants: Vec<Schema>,
    },
}
impl Schema {
    pub fn string(max_length: usize) -> Self {
        Self::String {
            max_length,
            values: BTreeSet::new(),
        }
    }
    pub fn object(properties: BTreeMap<String, Schema>) -> Self {
        let required = properties.keys().cloned().collect();
        Self::Object {
            properties,
            required,
        }
    }
    pub fn validate(&self, value: &Value) -> Result<(), ModelError> {
        self.check_value(value, "$", 0)
    }
    fn check_value(&self, v: &Value, path: &str, depth: usize) -> Result<(), ModelError> {
        if depth > 32 {
            return Err(ModelError::Invalid(format!(
                "{path}: schema nesting exceeds 32"
            )));
        }
        let valid = match self {
            Self::Null => v.is_null(),
            Self::Boolean => v.is_boolean(),
            Self::Integer { minimum, maximum } => {
                v.as_i64().is_some_and(|n| n >= *minimum && n <= *maximum)
            }
            Self::String { max_length, values } => v.as_str().is_some_and(|s| {
                s.len() <= *max_length && (values.is_empty() || values.contains(s))
            }),
            Self::Array { items, max_items } => {
                if let Some(a) = v.as_array() {
                    if a.len() > *max_items {
                        false
                    } else {
                        for (i, x) in a.iter().enumerate() {
                            items.check_value(x, &format!("{path}/{i}"), depth + 1)?;
                        }
                        true
                    }
                } else {
                    false
                }
            }
            Self::Object {
                properties,
                required,
            } => {
                if let Some(o) = v.as_object() {
                    if required.iter().any(|k| !o.contains_key(k))
                        || o.keys().any(|k| !properties.contains_key(k))
                    {
                        false
                    } else {
                        for (k, x) in o {
                            properties[k].check_value(x, &format!("{path}/{k}"), depth + 1)?;
                        }
                        true
                    }
                } else {
                    false
                }
            }
            Self::Union { variants } => variants
                .iter()
                .any(|schema| schema.check_value(v, path, depth + 1).is_ok()),
            Self::Nullable { inner } => {
                if v.is_null() {
                    true
                } else {
                    inner.check_value(v, path, depth + 1)?;
                    true
                }
            }
        };
        if valid {
            Ok(())
        } else {
            Err(ModelError::Invalid(format!(
                "{path}: value violates bounded schema"
            )))
        }
    }
    pub fn validate_definition(&self) -> Result<(), ModelError> {
        self.check_definition(0)
    }
    fn check_definition(&self, depth: usize) -> Result<(), ModelError> {
        if depth > 32 {
            return Err(ModelError::Invalid("schema nesting exceeds 32".into()));
        }
        match self {
            Self::Integer { minimum, maximum } if minimum > maximum => {
                return Err(ModelError::Invalid("inverted integer bounds".into()));
            }
            Self::String { max_length, values } => {
                if *max_length == 0
                    || *max_length > 1_048_576
                    || values.len() > 1024
                    || values.iter().any(|s| s.len() > *max_length)
                {
                    return Err(ModelError::Invalid("invalid string bound".into()));
                }
            }
            Self::Array { items, max_items } => {
                if *max_items == 0 || *max_items > 100_000 {
                    return Err(ModelError::Invalid("invalid array bound".into()));
                }
                items.check_definition(depth + 1)?;
            }
            Self::Object {
                properties,
                required,
            } => {
                if properties.len() > 1024 || required.iter().any(|k| !properties.contains_key(k)) {
                    return Err(ModelError::Invalid(
                        "invalid object properties/required".into(),
                    ));
                }
                for s in properties.values() {
                    s.check_definition(depth + 1)?;
                }
            }
            Self::Union { variants } => {
                if variants.len() < 2 || variants.len() > 16 {
                    return Err(ModelError::Invalid(
                        "union requires 2..16 alternatives".into(),
                    ));
                }
                for schema in variants {
                    schema.check_definition(depth + 1)?;
                }
            }
            Self::Nullable { inner } => inner.check_definition(depth + 1)?,
            _ => {}
        }
        Ok(())
    }
    /// Every value admitted by `self` must be admitted by `target`.
    pub fn assignable_to(&self, target: &Schema) -> bool {
        match (self, target) {
            (Self::Nullable { inner }, target @ Self::Union { .. }) => {
                Self::Null.assignable_to(target) && inner.assignable_to(target)
            }
            (Self::Union { variants }, target) => {
                variants.iter().all(|schema| schema.assignable_to(target))
            }
            (source, Self::Union { variants }) => {
                variants.iter().any(|schema| source.assignable_to(schema))
            }
            (Self::Null, Self::Nullable { .. }) => true,
            (Self::Nullable { inner: a }, Self::Nullable { inner: b }) => a.assignable_to(b),
            (a, Self::Nullable { inner: b }) => a.assignable_to(b),
            (
                Self::Integer {
                    minimum: a,
                    maximum: b,
                },
                Self::Integer {
                    minimum: c,
                    maximum: d,
                },
            ) => a >= c && b <= d,
            (
                Self::String {
                    max_length: a,
                    values: x,
                },
                Self::String {
                    max_length: b,
                    values: y,
                },
            ) => a <= b && (y.is_empty() || (!x.is_empty() && x.is_subset(y))),
            (
                Self::Array {
                    items: a,
                    max_items: x,
                },
                Self::Array {
                    items: b,
                    max_items: y,
                },
            ) => x <= y && a.assignable_to(b),
            (
                Self::Object {
                    properties: a,
                    required: ar,
                },
                Self::Object {
                    properties: b,
                    required: br,
                },
            ) => {
                br.is_subset(ar)
                    && a.iter()
                        .all(|(k, v)| b.get(k).is_some_and(|s| v.assignable_to(s)))
            }
            _ => self == target,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expr {
    Literal {
        value: Value,
    },
    Ref {
        root: String,
        #[serde(default)]
        path: Vec<String>,
    },
    Object {
        fields: BTreeMap<String, Expr>,
    },
    Array {
        items: Vec<Expr>,
    },
    Exists {
        value: Box<Expr>,
    },
    Not {
        value: Box<Expr>,
    },
    And {
        values: Vec<Expr>,
    },
    Or {
        values: Vec<Expr>,
    },
    Eq {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Lt {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Contains {
        collection: Box<Expr>,
        value: Box<Expr>,
    },
    Map {
        collection: Box<Expr>,
        item: String,
        value: Box<Expr>,
        max_items: usize,
    },
    Filter {
        collection: Box<Expr>,
        item: String,
        predicate: Box<Expr>,
        max_items: usize,
    },
    Difference {
        left: Box<Expr>,
        right: Box<Expr>,
        identity: Vec<String>,
        max_items: usize,
    },
}
impl Expr {
    pub fn literal(value: impl Into<Value>) -> Self {
        Self::Literal {
            value: value.into(),
        }
    }
    pub fn reference(root: impl Into<String>) -> Self {
        Self::Ref {
            root: root.into(),
            path: vec![],
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum EvalValue {
    Known(Value),
    Missing,
    Unknown(String),
}
pub type EvalContext = BTreeMap<String, EvalValue>;
#[derive(Debug, Clone, Copy)]
pub struct EvalLimits {
    pub max_depth: usize,
    pub max_elements: usize,
    pub max_bytes: usize,
}
impl Default for EvalLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_elements: 10_000,
            max_bytes: 1_048_576,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Observation,
    Mutation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    Observation,
    NaturallyIdempotent,
    KeyedIdempotent,
    Reconcilable,
    Unsafe,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandContract {
    pub name: String,
    pub version: String,
    pub input: Schema,
    pub outcomes: BTreeMap<String, Schema>,
    pub capabilities: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
    pub effect: Effect,
    pub recovery: Recovery,
    pub asynchronous: Option<AsyncContract>,
    pub cancellation: bool,
    pub public_result_fields: BTreeSet<String>,
    pub sensitive_input_fields: BTreeSet<String>,
    #[serde(default)]
    pub sensitive_output_fields: BTreeSet<String>,
    pub max_payload_bytes: usize,
    pub deadline_ms: u64,
}
impl CommandContract {
    pub fn fingerprint(&self) -> Result<Hash, ModelError> {
        digest(self)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsyncContract {
    pub completion_event: String,
    pub completion: Schema,
    pub correlation_field: String,
    pub reconciliation_command: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventContract {
    #[serde(default)]
    pub sensitive_payload_fields: BTreeSet<String>,
    pub name: String,
    pub version: String,
    pub payload: Schema,
    pub trusted_producers: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
    pub correlation_fields: BTreeSet<String>,
    pub subject_field: Option<String>,
    pub revision_field: Option<String>,
    pub max_payload_bytes: usize,
    pub retention_ms: u64,
}
impl EventContract {
    pub fn fingerprint(&self) -> Result<Hash, ModelError> {
        digest(self)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub commands: BTreeMap<String, CommandContract>,
    pub events: BTreeMap<String, EventContract>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractRef {
    pub name: String,
    pub fingerprint: Hash,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub producer: String,
    pub event_id: String,
    pub contract: ContractRef,
    pub scope: String,
    pub correlation: BTreeMap<String, String>,
    pub subject: Option<String>,
    pub revision: Option<u64>,
    pub causation: Option<String>,
    pub occurred_at_ms: Option<i64>,
    pub observed_at_ms: Option<i64>,
    pub payload: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    pub input: Schema,
    pub outcomes: BTreeMap<String, Schema>,
    pub start: String,
}
/// A catalog selection resolves the named binding exactly once at root admission.
/// An empty digest allowlist permits any currently active compatible version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DependencySelection {
    #[default]
    Exact,
    Catalog {
        allowed_artifacts: BTreeSet<Hash>,
    },
}
/// Actual implementations may accept a wider input domain, return a narrower
/// output domain, and omit expected outcomes; they may not add unhandled outcomes.
pub fn interface_compatible(actual: &EntryPoint, expected: &EntryPoint) -> bool {
    expected.input.assignable_to(&actual.input)
        && actual.outcomes.iter().all(|(name, schema)| {
            expected
                .outcomes
                .get(name)
                .is_some_and(|target| schema.assignable_to(target))
        })
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallableDependency {
    pub binding: String,
    #[serde(default)]
    pub selection: DependencySelection,
    pub artifact: Hash,
    pub interfaces: BTreeMap<String, EntryPoint>,
    pub capabilities: BTreeSet<String>,
    pub scope: String,
    pub dependencies: BTreeSet<Hash>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    pub id: String,
    pub event: ContractRef,
    pub entry: String,
    pub input: Expr,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_step_visits: u64,
    pub max_children: u64,
    pub max_depth: u32,
    pub max_concurrent_effects: u32,
    pub max_concurrent_children: u32,
    pub max_payload_bytes: usize,
    pub max_history_bytes: usize,
    pub max_duration_ms: u64,
    pub max_iterations: u64,
    pub max_iteration_depth: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_step_visits: 10_000,
            max_children: 100,
            max_depth: 8,
            max_concurrent_effects: 16,
            max_concurrent_children: 16,
            max_payload_bytes: 1_048_576,
            max_history_bytes: 16_777_216,
            max_duration_ms: 86_400_000,
            max_iterations: 1000,
            max_iteration_depth: 4,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Playbook {
    pub id: String,
    pub purpose: String,
    pub scope: String,
    pub entries: BTreeMap<String, EntryPoint>,
    pub steps: BTreeMap<String, Step>,
    pub triggers: Vec<Trigger>,
    pub limits: Limits,
    pub required_capabilities: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub op: Operation,
    pub edges: BTreeMap<String, String>,
    pub on_error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Invoke {
        command: ContractRef,
        input: Expr,
        bind: String,
        retry: Option<RetryPolicy>,
    },
    Decide {
        predicate: Expr,
    },
    Wait {
        event: Option<ContractRef>,
        correlation: BTreeMap<String, Expr>,
        minimum_revision: Option<Expr>,
        deadline_ms: u64,
        bind: String,
    },
    ForEach {
        collection: Expr,
        item: String,
        body: String,
        next: String,
        max_items: usize,
        concurrency: u32,
        on_failure: IterationFailure,
    },
    Call {
        dependency: String,
        entry: String,
        input: Expr,
        mode: CallMode,
        bind: String,
    },
    Join {
        members: Vec<Expr>,
        #[serde(default)]
        member_keys: Vec<String>,
        mode: JoinMode,
        accepted_outcomes: BTreeSet<String>,
        deadline_ms: u64,
        remaining: RemainingPolicy,
        bind: String,
    },
    Return {
        outcome: String,
        output: Expr,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallMode {
    Wait,
    OwnedAsync,
    Independent,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinMode {
    All,
    AnyCompletion,
    AnyAccepted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemainingPolicy {
    Track,
    Cancel,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IterationFailure {
    Stop,
    Continue,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub outcomes: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceHeader {
    pub format_version: u32,
    pub id: String,
    pub scope: String,
    pub interfaces: BTreeSet<String>,
    pub definitions: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub digest: Hash,
    pub header: SourceHeader,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub line: u32,
    pub column: u32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDefinition {
    pub version: String,
    pub digest: Hash,
    pub value: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactContent {
    pub format_version: u32,
    pub compiler: String,
    pub source: SourceBinding,
    pub playbook: Playbook,
    pub registry: Registry,
    pub dependencies: BTreeMap<String, CallableDependency>,
    pub definitions: BTreeMap<String, CapturedDefinition>,
    pub source_map: BTreeMap<String, SourceLocation>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub digest: Hash,
    pub executable_fingerprint: Hash,
    pub content: ArtifactContent,
}
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("invalid model: {0}")]
    Invalid(String),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("integrity mismatch: {0}")]
    Integrity(String),
    #[error("expression: {0}")]
    Expression(String),
}

/// Canonical JSON: UTF-8, recursively sorted keys, no whitespace, integral i64/u64 numbers
/// only, serde_json string escaping, no omitted fields beyond the serialized model.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, ModelError> {
    let value = serde_json::to_value(value)?;
    let normalized = canonical_value(value)?;
    Ok(serde_json::to_vec(&normalized)?)
}
fn canonical_value(value: Value) -> Result<Value, ModelError> {
    match value {
        Value::Number(n) if !(n.is_i64() || n.is_u64()) => Err(ModelError::Invalid(
            "fractional/exponential numbers unsupported; use schema-defined decimal strings".into(),
        )),
        Value::Array(a) => Ok(Value::Array(
            a.into_iter()
                .map(canonical_value)
                .collect::<Result<_, _>>()?,
        )),
        Value::Object(o) => {
            let sorted: BTreeMap<_, _> = o.into_iter().collect();
            let mut m = serde_json::Map::new();
            for (k, v) in sorted {
                m.insert(k, canonical_value(v)?);
            }
            Ok(Value::Object(m))
        }
        v => Ok(v),
    }
}
pub fn digest<T: Serialize>(value: &T) -> Result<Hash, ModelError> {
    Ok(hash_bytes(&canonical_bytes(value)?))
}
pub fn hash_bytes(bytes: &[u8]) -> Hash {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
impl ArtifactContent {
    pub fn executable_fingerprint(&self) -> Result<Hash, ModelError> {
        // Source prose, mappings, and human-facing purpose are presentation metadata.
        let mut p = self.playbook.clone();
        p.purpose.clear();
        digest(&(
            self.format_version,
            &p,
            &self.registry,
            &self.dependencies,
            &self.definitions,
        ))
    }
}
impl Artifact {
    pub fn seal(content: ArtifactContent) -> Result<Self, ModelError> {
        Ok(Self {
            digest: digest(&content)?,
            executable_fingerprint: content.executable_fingerprint()?,
            content,
        })
    }
    pub fn verify(&self) -> Result<(), ModelError> {
        let issues = self.content.validate_structure();
        if !issues.is_empty() {
            return Err(ModelError::Invalid(
                issues
                    .iter()
                    .map(|i| format!("{}: {}", i.path, i.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        if self.content.format_version != FORMAT_VERSION {
            return Err(ModelError::Invalid("unsupported artifact format".into()));
        }
        if digest(&self.content)? != self.digest {
            return Err(ModelError::Integrity("content digest".into()));
        }
        if self.content.executable_fingerprint()? != self.executable_fingerprint {
            return Err(ModelError::Integrity("executable fingerprint".into()));
        }
        for d in self.content.definitions.values() {
            if digest(&d.value)? != d.digest {
                return Err(ModelError::Integrity("captured definition".into()));
            }
        }
        Ok(())
    }
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, ModelError> {
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(ModelError::Invalid("artifact exceeds 16 MiB".into()));
        }
        let a: Self = serde_json::from_slice(bytes)?;
        if canonical_bytes(&a)? != bytes {
            return Err(ModelError::Integrity(
                "noncanonical artifact encoding".into(),
            ));
        }
        a.verify()?;
        Ok(a)
    }
}

pub fn eval(
    expr: &Expr,
    context: &EvalContext,
    limits: &EvalLimits,
) -> Result<EvalValue, ModelError> {
    let mut budget = limits.max_elements;
    let result = eval_inner(expr, context, limits, 0, &mut budget)?;
    if let EvalValue::Known(v) = &result
        && canonical_bytes(v)?.len() > limits.max_bytes
    {
        return Err(ModelError::Expression("expression byte limit".into()));
    }
    Ok(result)
}
impl Expr {
    pub fn eval(
        &self,
        context: &EvalContext,
        limits: &EvalLimits,
    ) -> Result<EvalValue, ModelError> {
        eval(self, context, limits)
    }
}
fn unresolved(values: &[EvalValue]) -> Option<EvalValue> {
    values
        .iter()
        .find(|x| matches!(x, EvalValue::Unknown(_)))
        .or_else(|| values.iter().find(|x| matches!(x, EvalValue::Missing)))
        .cloned()
}
fn path_value(value: &Value, path: &[String]) -> EvalValue {
    let mut v = value;
    for k in path {
        let n = match v {
            Value::Object(o) => o.get(k),
            Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
            _ => None,
        };
        let Some(n) = n else {
            return EvalValue::Missing;
        };
        v = n;
    }
    EvalValue::Known(v.clone())
}
fn eval_inner(
    expr: &Expr,
    context: &EvalContext,
    limits: &EvalLimits,
    depth: usize,
    budget: &mut usize,
) -> Result<EvalValue, ModelError> {
    if depth > limits.max_depth || *budget == 0 {
        return Err(ModelError::Expression(
            "expression depth/element limit".into(),
        ));
    }
    *budget -= 1;
    let mut sub = |e: &Expr| eval_inner(e, context, limits, depth + 1, budget);
    let known = |v: Value| Ok(EvalValue::Known(v));
    match expr {
        Expr::Literal { value } => {
            canonical_bytes(value)?;
            known(value.clone())
        }
        Expr::Ref { root, path } => Ok(match context.get(root) {
            Some(EvalValue::Known(v)) => path_value(v, path),
            Some(v) => v.clone(),
            None => EvalValue::Missing,
        }),
        Expr::Object { fields } => {
            let mut o = serde_json::Map::new();
            let mut pending = vec![];
            for (k, e) in fields {
                match sub(e)? {
                    EvalValue::Known(v) => {
                        o.insert(k.clone(), v);
                    }
                    v => pending.push(v),
                }
            }
            if let Some(v) = unresolved(&pending) {
                Ok(v)
            } else {
                known(Value::Object(o))
            }
        }
        Expr::Array { items } => {
            let values = items.iter().map(&mut sub).collect::<Result<Vec<_>, _>>()?;
            if let Some(v) = unresolved(&values) {
                Ok(v)
            } else {
                known(Value::Array(
                    values
                        .into_iter()
                        .map(|x| match x {
                            EvalValue::Known(v) => v,
                            _ => unreachable!(),
                        })
                        .collect(),
                ))
            }
        }
        Expr::Exists { value } => match sub(value)? {
            EvalValue::Missing => known(Value::Bool(false)),
            EvalValue::Known(_) => known(Value::Bool(true)),
            v => Ok(v),
        },
        Expr::Not { value } => match sub(value)? {
            EvalValue::Known(Value::Bool(b)) => known(Value::Bool(!b)),
            EvalValue::Known(_) => Err(ModelError::Expression("not requires boolean".into())),
            v => Ok(v),
        },
        Expr::And { values } | Expr::Or { values } => {
            let and = matches!(expr, Expr::And { .. });
            let values = values.iter().map(&mut sub).collect::<Result<Vec<_>, _>>()?;
            // Conservative evidence semantics: unresolved required evidence never disappears
            // through short circuiting (false && unknown remains unknown).
            if let Some(v) = unresolved(&values) {
                return Ok(v);
            }
            let mut result = and;
            for v in values {
                let EvalValue::Known(Value::Bool(b)) = v else {
                    return Err(ModelError::Expression(
                        "boolean expression requires boolean".into(),
                    ));
                };
                if and { result &= b } else { result |= b }
            }
            known(Value::Bool(result))
        }
        Expr::Eq { left, right }
        | Expr::Lt { left, right }
        | Expr::Contains {
            collection: left,
            value: right,
        } => {
            let values = vec![sub(left)?, sub(right)?];
            if let Some(v) = unresolved(&values) {
                return Ok(v);
            }
            let (EvalValue::Known(a), EvalValue::Known(b)) = (&values[0], &values[1]) else {
                unreachable!()
            };
            let yes = match expr {
                Expr::Eq { .. } => a == b,
                Expr::Lt { .. } => match (a, b) {
                    (Value::Number(a), Value::Number(b)) => {
                        let a = number_i128(a)?;
                        let b = number_i128(b)?;
                        a < b
                    }
                    (Value::String(a), Value::String(b)) => a < b,
                    _ => {
                        return Err(ModelError::Expression(
                            "lt requires matching integers or strings".into(),
                        ));
                    }
                },
                _ => a
                    .as_array()
                    .ok_or_else(|| ModelError::Expression("contains requires array".into()))?
                    .contains(b),
            };
            known(Value::Bool(yes))
        }
        Expr::Map {
            collection,
            item,
            value,
            max_items,
        }
        | Expr::Filter {
            collection,
            item,
            predicate: value,
            max_items,
        } => {
            let source = sub(collection)?;
            let EvalValue::Known(source) = source else {
                return Ok(source);
            };
            let source = source.as_array().ok_or_else(|| {
                ModelError::Expression("collection operation requires array".into())
            })?;
            if *max_items == 0 || source.len() > *max_items || source.len() > limits.max_elements {
                return Err(ModelError::Expression("collection bound exceeded".into()));
            }
            if context.contains_key(item) {
                return Err(ModelError::Expression(
                    "lexical binding shadows existing root".into(),
                ));
            }
            let mut result = vec![];
            for v in source {
                let mut local = context.clone();
                local.insert(item.clone(), EvalValue::Known(v.clone()));
                let mapped = eval_inner(value, &local, limits, depth + 1, budget)?;
                match mapped {
                    EvalValue::Known(m) => {
                        if matches!(expr, Expr::Filter { .. }) {
                            let b = m.as_bool().ok_or_else(|| {
                                ModelError::Expression("filter requires boolean predicate".into())
                            })?;
                            if b {
                                result.push(v.clone());
                            }
                        } else {
                            result.push(m);
                        }
                    }
                    unknown => return Ok(unknown),
                }
            }
            known(Value::Array(result))
        }
        Expr::Difference {
            left,
            right,
            identity,
            max_items,
        } => {
            let values = vec![sub(left)?, sub(right)?];
            if let Some(v) = unresolved(&values) {
                return Ok(v);
            }
            let (EvalValue::Known(a), EvalValue::Known(b)) = (&values[0], &values[1]) else {
                unreachable!()
            };
            let (a, b) = (
                a.as_array()
                    .ok_or_else(|| ModelError::Expression("difference requires arrays".into()))?,
                b.as_array()
                    .ok_or_else(|| ModelError::Expression("difference requires arrays".into()))?,
            );
            if identity.is_empty()
                || *max_items == 0
                || a.len() > *max_items
                || b.len() > *max_items
                || a.len() + b.len() > *budget
            {
                return Err(ModelError::Expression(
                    "difference identity/element bound".into(),
                ));
            }
            *budget -= a.len() + b.len();
            let mut seen_a = BTreeSet::new();
            let mut seen_b = BTreeSet::new();
            for v in b {
                let EvalValue::Known(key) = path_value(v, identity) else {
                    return Err(ModelError::Expression("missing difference identity".into()));
                };
                let key = canonical_bytes(&key)?;
                if !seen_b.insert(key) {
                    return Err(ModelError::Expression(
                        "duplicate difference identity".into(),
                    ));
                }
            }
            let mut result = vec![];
            for v in a {
                let EvalValue::Known(key) = path_value(v, identity) else {
                    return Err(ModelError::Expression("missing difference identity".into()));
                };
                let key = canonical_bytes(&key)?;
                if !seen_a.insert(key.clone()) {
                    return Err(ModelError::Expression(
                        "duplicate difference identity".into(),
                    ));
                }
                if !seen_b.contains(&key) {
                    result.push(v.clone());
                }
            }
            known(Value::Array(result))
        }
    }
}
fn number_i128(n: &serde_json::Number) -> Result<i128, ModelError> {
    n.as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from))
        .ok_or_else(|| ModelError::Expression("noninteger number".into()))
}

/// Validates a bounded event against its captured executable registration.
pub fn validate_event(event: &EventEnvelope, registry: &Registry) -> Result<(), ModelError> {
    let c = registry
        .events
        .get(&event.contract.name)
        .ok_or_else(|| ModelError::Invalid("unknown event".into()))?;
    if c.fingerprint()? != event.contract.fingerprint {
        return Err(ModelError::Integrity("event contract fingerprint".into()));
    }
    if event.event_id.is_empty()
        || !c.trusted_producers.contains(&event.producer)
        || !c.scopes.contains(&event.scope)
    {
        return Err(ModelError::Invalid(
            "event identity/producer/scope refused".into(),
        ));
    }
    if c.correlation_fields
        .iter()
        .any(|k| !event.correlation.contains_key(k))
        || event
            .correlation
            .keys()
            .any(|k| !c.correlation_fields.contains(k))
    {
        return Err(ModelError::Invalid("event correlation fields".into()));
    }
    if c.subject_field.is_some() != event.subject.is_some()
        || c.revision_field.is_some() != event.revision.is_some()
    {
        return Err(ModelError::Invalid(
            "event subject/revision required by contract".into(),
        ));
    }
    if canonical_bytes(&event.payload)?.len() > c.max_payload_bytes {
        return Err(ModelError::Invalid("event payload byte bound".into()));
    }
    c.payload.validate(&event.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn bounded_union_and_callable_interface_variance() {
        let union = Schema::Union {
            variants: vec![Schema::Boolean, Schema::string(8)],
        };
        assert!(union.validate(&json!(true)).is_ok());
        assert!(union.validate(&json!("subject")).is_ok());
        assert!(union.validate(&json!(42)).is_err());
        assert!(
            Schema::Union {
                variants: vec![Schema::Null]
            }
            .validate_definition()
            .is_err()
        );
        let expected = EntryPoint {
            input: Schema::string(8),
            outcomes: BTreeMap::from([("ok".into(), Schema::string(16))]),
            start: "old".into(),
        };
        let actual = EntryPoint {
            input: Schema::string(16),
            outcomes: BTreeMap::from([("ok".into(), Schema::string(8))]),
            start: "new".into(),
        };
        assert!(interface_compatible(&actual, &expected));
        assert!(!interface_compatible(&expected, &actual));
        let mut expanded = actual.clone();
        expanded.outcomes.insert("new_outcome".into(), Schema::Null);
        assert!(!interface_compatible(&expanded, &expected));
    }

    #[test]
    fn canonical_golden_vectors() {
        assert_eq!(
            canonical_bytes(&json!({"z":null,"a":"line\n\u{0000}","n":-3})).unwrap(),
            b"{\"a\":\"line\\n\\u0000\",\"n\":-3,\"z\":null}"
        );
        assert!(canonical_bytes(&json!(1.5)).is_err());
        assert_ne!(
            digest(&json!({})).unwrap(),
            digest(&json!({"x":null})).unwrap()
        );
    }
    #[test]
    fn missing_null_and_unknown_are_distinct() {
        let context = BTreeMap::from([
            ("null".into(), EvalValue::Known(Value::Null)),
            ("stale".into(), EvalValue::Unknown("revision".into())),
        ]);
        let l = EvalLimits::default();
        assert_eq!(
            eval(
                &Expr::Exists {
                    value: Box::new(Expr::reference("null"))
                },
                &context,
                &l
            )
            .unwrap(),
            EvalValue::Known(json!(true))
        );
        assert_eq!(
            eval(
                &Expr::Exists {
                    value: Box::new(Expr::reference("absent"))
                },
                &context,
                &l
            )
            .unwrap(),
            EvalValue::Known(json!(false))
        );
        assert!(matches!(
            eval(
                &Expr::Not {
                    value: Box::new(Expr::reference("stale"))
                },
                &context,
                &l
            )
            .unwrap(),
            EvalValue::Unknown(_)
        ));
        assert!(matches!(
            eval(
                &Expr::And {
                    values: vec![Expr::literal(false), Expr::reference("stale")]
                },
                &context,
                &l
            )
            .unwrap(),
            EvalValue::Unknown(_)
        ));
    }
    #[test]
    fn collection_order_duplicates_and_budget() {
        let d = Expr::Difference {
            left: Box::new(Expr::literal(json!([{"id":"b"},{"id":"a"},{"id":"c"}]))),
            right: Box::new(Expr::literal(json!([{"id":"a"}]))),
            identity: vec!["id".into()],
            max_items: 4,
        };
        assert_eq!(
            eval(&d, &BTreeMap::new(), &EvalLimits::default()).unwrap(),
            EvalValue::Known(json!([{"id":"b"},{"id":"c"}]))
        );
        let duplicate = Expr::Difference {
            left: Box::new(Expr::literal(json!([{"id":1},{"id":1}]))),
            right: Box::new(Expr::literal(json!([]))),
            identity: vec!["id".into()],
            max_items: 4,
        };
        assert!(eval(&duplicate, &BTreeMap::new(), &EvalLimits::default()).is_err());
        assert!(
            eval(
                &d,
                &BTreeMap::new(),
                &EvalLimits {
                    max_elements: 2,
                    ..EvalLimits::default()
                }
            )
            .is_err()
        );
    }
    #[test]
    fn schemas_are_closed_and_bounded() {
        let schema = Schema::object(BTreeMap::from([("s".into(), Schema::string(3))]));
        assert!(schema.validate(&json!({"s":"abc"})).is_ok());
        assert!(schema.validate(&json!({"s":"abcd"})).is_err());
        assert!(schema.validate(&json!({"s":"ab","extra":true})).is_err());
        assert!(
            serde_json::from_value::<Schema>(
                json!({"type":"string","max_length":4,"pattern":".*"})
            )
            .is_err()
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub code: String,
    pub path: String,
    pub message: String,
}
fn issue(issues: &mut Vec<ValidationIssue>, code: &str, path: &str, message: impl Into<String>) {
    issues.push(ValidationIssue {
        code: code.into(),
        path: path.into(),
        message: message.into(),
    });
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
fn valid_hash(hash: &str) -> bool {
    hash.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn schema_has_field(schema: &Schema, field: &str) -> bool {
    match schema {
        Schema::Object { properties, .. } => properties.contains_key(field),
        Schema::Nullable { inner } => schema_has_field(inner, field),
        Schema::Union { variants } => variants
            .iter()
            .any(|schema| schema_has_field(schema, field)),
        _ => false,
    }
}
fn schema_issue(schema: &Schema, path: &str, issues: &mut Vec<ValidationIssue>) {
    if let Err(e) = schema.validate_definition() {
        issue(issues, "schema", path, e.to_string());
    }
}
fn required_edges(
    step: &Step,
    names: impl IntoIterator<Item = String>,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    for name in names {
        if !step.edges.contains_key(&name) {
            issue(
                issues,
                "unhandled_outcome",
                path,
                format!("outcome {name} has no edge"),
            );
        }
    }
}
fn children(step: &Step) -> Vec<&str> {
    let mut result: Vec<_> = step.edges.values().map(String::as_str).collect();
    if let Some(e) = &step.on_error {
        result.push(e);
    }
    if let Operation::ForEach { body, next, .. } = &step.op {
        result.push(body);
        result.push(next);
    }
    result
}
impl ArtifactContent {
    /// Structural and typed validation shared by compiler and runner installation.
    /// Exact dependency hashes are format-1's supported callable selection policy.
    pub fn validate_structure(&self) -> Vec<ValidationIssue> {
        let mut issues = vec![];
        let p = &self.playbook;
        if self.format_version != FORMAT_VERSION
            || self.source.header.format_version != FORMAT_VERSION
        {
            issue(
                &mut issues,
                "unsupported_format",
                "format_version",
                "unsupported artifact/source format",
            );
        }
        if self.compiler.is_empty() || self.compiler.len() > 256 {
            issue(
                &mut issues,
                "compiler_build",
                "compiler",
                "bounded compiler build identifier required",
            );
        }
        if !valid_hash(&self.source.digest) {
            issue(
                &mut issues,
                "source_digest",
                "source.digest",
                "invalid source SHA-256 address",
            );
        }
        if !valid_name(&p.id)
            || !valid_name(&p.scope)
            || p.id != self.source.header.id
            || p.scope != self.source.header.scope
        {
            issue(
                &mut issues,
                "source_identity",
                "playbook",
                "source and proposal identity/scope must match",
            );
        }
        if self.source.header.interfaces != p.entries.keys().cloned().collect() {
            issue(
                &mut issues,
                "source_interfaces",
                "source.header.interfaces",
                "header must enumerate exact callable entry points",
            );
        }
        if self.source.header.definitions != self.definitions.keys().cloned().collect() {
            issue(
                &mut issues,
                "source_definitions",
                "source.header.definitions",
                "header must enumerate exact captured definitions",
            );
        }
        if p.entries.is_empty()
            || p.entries.len() > 128
            || p.steps.is_empty()
            || p.steps.len() > 4096
            || p.triggers.len() > 128
            || self.dependencies.len() > 128
            || self.registry.commands.len() > 256
            || self.registry.events.len() > 256
            || self.definitions.len() > 256
        {
            issue(
                &mut issues,
                "model_limits",
                "playbook",
                "entry/step/trigger/registry/dependency/definition count bound exceeded",
            );
        }
        let l = &p.limits;
        if l.max_step_visits == 0
            || l.max_step_visits > 1_000_000
            || l.max_children > 10_000
            || l.max_depth == 0
            || l.max_depth > 32
            || l.max_concurrent_effects == 0
            || l.max_concurrent_effects > 1024
            || l.max_concurrent_children == 0
            || l.max_concurrent_children > 1024
            || l.max_payload_bytes == 0
            || l.max_payload_bytes > 16_777_216
            || l.max_history_bytes < l.max_payload_bytes
            || l.max_history_bytes > 1_073_741_824
            || l.max_duration_ms == 0
            || l.max_duration_ms > 31_536_000_000
            || l.max_iterations == 0
            || l.max_iterations > 1_000_000
            || l.max_iteration_depth == 0
            || l.max_iteration_depth > 16
        {
            issue(
                &mut issues,
                "limits",
                "playbook.limits",
                "invalid or unsupported execution limits",
            );
        }
        for (name, entry) in &p.entries {
            let path = format!("entries.{name}");
            if !valid_name(name) {
                issue(&mut issues, "identifier", &path, "invalid entry name");
            }
            schema_issue(&entry.input, &path, &mut issues);
            if entry.outcomes.is_empty() || entry.outcomes.len() > 64 {
                issue(
                    &mut issues,
                    "outcomes",
                    &path,
                    "one to 64 business outcomes required",
                );
            }
            for s in entry.outcomes.values() {
                schema_issue(s, &path, &mut issues);
            }
            if !p.steps.contains_key(&entry.start) {
                issue(
                    &mut issues,
                    "step_reference",
                    &path,
                    "entry start does not exist",
                );
            }
        }
        for (name, c) in &self.registry.commands {
            let path = format!("commands.{name}");
            if name != &c.name || !valid_name(name) || c.version.is_empty() || c.version.len() > 64
            {
                issue(
                    &mut issues,
                    "registry_identity",
                    &path,
                    "invalid command name/version",
                );
            }
            schema_issue(&c.input, &path, &mut issues);
            if c.outcomes.is_empty() || c.outcomes.len() > 64 {
                issue(
                    &mut issues,
                    "outcomes",
                    &path,
                    "one to 64 command outcomes required",
                );
            }
            for s in c.outcomes.values() {
                schema_issue(s, &path, &mut issues);
            }
            if c.scopes.is_empty()
                || c.max_payload_bytes == 0
                || c.max_payload_bytes > 16_777_216
                || c.deadline_ms == 0
                || c.deadline_ms > 31_536_000_000
            {
                issue(
                    &mut issues,
                    "contract_limits",
                    &path,
                    "command requires bounded scope, payload, deadline",
                );
            }
            if c.effect == Effect::Mutation && c.recovery == Recovery::Observation {
                issue(
                    &mut issues,
                    "recovery",
                    &path,
                    "mutation cannot use observation recovery",
                );
            }
            if let Some(a) = &c.asynchronous {
                schema_issue(&a.completion, &path, &mut issues);
                let completion_valid = match &a.completion {
                    Schema::Object {
                        properties,
                        required,
                    } => {
                        required.contains("outcome")
                            && required.contains("output")
                            && properties.contains_key("output")
                            && matches!(properties.get("outcome"),Some(Schema::String{values,..}) if !values.is_empty() && values.iter().all(|v|v!="accepted"&&c.outcomes.contains_key(v)))
                    }
                    _ => false,
                };
                let admission_valid = matches!(c.outcomes.get("accepted"),Some(Schema::Object {properties,required}) if required == &BTreeSet::from(["handle".into()]) && matches!(properties.get("handle"),Some(Schema::String{..})));
                if !completion_valid || !admission_valid {
                    issue(
                        &mut issues,
                        "async_contract",
                        &path,
                        "async contract requires accepted {handle:string} and completion {outcome:declared business enum,output}",
                    );
                }
                if !self.registry.events.contains_key(&a.completion_event)
                    || !self
                        .registry
                        .commands
                        .contains_key(&a.reconciliation_command)
                    || a.correlation_field.is_empty()
                {
                    issue(
                        &mut issues,
                        "async_contract",
                        &path,
                        "async completion/reconciliation registrations required",
                    );
                } else {
                    let ev = &self.registry.events[&a.completion_event];
                    if !ev.correlation_fields.contains(&a.correlation_field)
                        || !a.completion.assignable_to(&ev.payload)
                    {
                        issue(
                            &mut issues,
                            "async_contract",
                            &path,
                            "async completion schema/correlation incompatible with event",
                        );
                    }
                }
            }
            for field in &c.public_result_fields {
                if !c
                    .outcomes
                    .values()
                    .any(|schema| schema_has_field(schema, field))
                {
                    issue(
                        &mut issues,
                        "public_projection",
                        &path,
                        "public fields must exist in at least one declared outcome object",
                    );
                }
            }
            for field in &c.sensitive_output_fields {
                if c.public_result_fields.contains(field)
                    || !c
                        .outcomes
                        .values()
                        .any(|schema| schema_has_field(schema, field))
                {
                    issue(
                        &mut issues,
                        "sensitive_field",
                        &path,
                        "protected output fields must exist in a declared outcome and cannot be public projection fields",
                    );
                }
            }
            for field in &c.sensitive_input_fields {
                if !schema_has_field(&c.input, field) {
                    issue(
                        &mut issues,
                        "sensitive_field",
                        &path,
                        "sensitive input field must exist in input object",
                    );
                }
            }
        }
        for (name, e) in &self.registry.events {
            let path = format!("events.{name}");
            if name != &e.name
                || !valid_name(name)
                || e.version.is_empty()
                || e.trusted_producers.is_empty()
                || e.scopes.is_empty()
                || e.max_payload_bytes == 0
                || e.max_payload_bytes > 16_777_216
                || e.retention_ms == 0
            {
                issue(
                    &mut issues,
                    "event_contract",
                    &path,
                    "invalid event identity, producer, scope or resource bound",
                );
            }
            schema_issue(&e.payload, &path, &mut issues);
            if e.sensitive_payload_fields
                .iter()
                .any(|field| !schema_has_field(&e.payload, field))
            {
                issue(
                    &mut issues,
                    "sensitive_field",
                    &path,
                    "sensitive event payload fields must exist in the declared payload schema",
                );
            }
        }
        for (name, d) in &self.definitions {
            if !valid_name(name)
                || d.version.is_empty()
                || digest(&d.value).ok().as_ref() != Some(&d.digest)
            {
                issue(
                    &mut issues,
                    "definition",
                    &format!("definitions.{name}"),
                    "captured definition identity/version/hash invalid",
                );
            }
        }
        let mut bindings = BTreeSet::new();
        for (name, d) in &self.dependencies {
            let path = format!("dependencies.{name}");
            if !valid_name(name)
                || !valid_name(&d.binding)
                || !valid_hash(&d.artifact)
                || d.scope != p.scope
                || d.interfaces.is_empty()
                || d.interfaces.len() > 128
                || d.dependencies.len() > 1024
                || d.dependencies.iter().any(|hash| !valid_hash(hash))
                || !d.capabilities.is_subset(&p.required_capabilities)
            {
                issue(
                    &mut issues,
                    "dependency",
                    &path,
                    "dependency requires a reference artifact, same scope, declared interfaces and covered capabilities",
                );
            }
            if let DependencySelection::Catalog { allowed_artifacts } = &d.selection
                && (allowed_artifacts.len() > 1024
                    || allowed_artifacts.iter().any(|hash| !valid_hash(hash)))
            {
                issue(
                    &mut issues,
                    "dependency_selection",
                    &path,
                    "catalog artifact allowlist must contain bounded SHA-256 addresses",
                );
            }
            if d.dependencies.contains(&d.artifact) {
                issue(
                    &mut issues,
                    "recursive_call",
                    &path,
                    "recursive dependency closure",
                );
            }
            if !bindings.insert(d.binding.clone()) {
                issue(
                    &mut issues,
                    "dependency",
                    &path,
                    "duplicate callable binding",
                );
            }
            for e in d.interfaces.values() {
                if e.outcomes.is_empty() || e.outcomes.len() > 64 {
                    issue(
                        &mut issues,
                        "outcomes",
                        &path,
                        "dependency interface requires one to 64 outcomes",
                    );
                }
                schema_issue(&e.input, &path, &mut issues);
                for s in e.outcomes.values() {
                    schema_issue(s, &path, &mut issues);
                }
            }
        }
        // A root's own artifact address is unavailable until packaging, so bind the
        // static logical name too. Runtime resolves and verifies every exact closure.
        for d in self.dependencies.values() {
            if d.binding == p.id {
                issue(
                    &mut issues,
                    "recursive_call",
                    "dependencies",
                    "self-call binding forbidden",
                );
            }
        }
        let all_bind_names: BTreeSet<_> = p
            .steps
            .values()
            .filter_map(|s| match &s.op {
                Operation::Invoke { bind, .. }
                | Operation::Wait { bind, .. }
                | Operation::Call { bind, .. }
                | Operation::Join { bind, .. } => Some(bind.clone()),
                _ => None,
            })
            .collect();
        let mut item_names = BTreeSet::new();
        let mut bound_names = BTreeSet::new();
        for (name, step) in &p.steps {
            let path = format!("steps.{name}");
            if !valid_name(name) {
                issue(&mut issues, "identifier", &path, "invalid step name");
            }
            for target in children(step) {
                if !p.steps.contains_key(target) {
                    issue(
                        &mut issues,
                        "step_reference",
                        &path,
                        format!("unknown target {target}"),
                    );
                }
            }
            let bind = match &step.op {
                Operation::Invoke { bind, .. }
                | Operation::Wait { bind, .. }
                | Operation::Call { bind, .. }
                | Operation::Join { bind, .. } => Some(bind),
                _ => None,
            };
            if let Some(bind) = bind
                && (!valid_name(bind)
                    || matches!(bind.as_str(), "input" | "event" | "definitions" | "vars")
                    || !bound_names.insert(bind.clone()))
            {
                issue(
                    &mut issues,
                    "binding",
                    &path,
                    "bindings must be valid distinct roots, excluding reserved roots",
                );
            }
            if !matches!(step.op, Operation::Return { .. } | Operation::Decide { .. })
                && step.on_error.is_none()
            {
                issue(
                    &mut issues,
                    "unhandled_error",
                    &path,
                    "operation requires explicit on_error continuation",
                );
            }
            match &step.op {
                Operation::Invoke { command, retry, .. } => {
                    if let Some(c) = self.registry.commands.get(&command.name) {
                        if c.fingerprint().ok().as_ref() != Some(&command.fingerprint) {
                            issue(
                                &mut issues,
                                "contract_fingerprint",
                                &path,
                                "command exact contract fingerprint mismatch",
                            );
                        }
                        if !c.scopes.contains(&p.scope)
                            || !c.capabilities.is_subset(&p.required_capabilities)
                        {
                            issue(
                                &mut issues,
                                "authority",
                                &path,
                                "command scope/capabilities not declared by playbook",
                            );
                        }
                        required_edges(step, c.outcomes.keys().cloned(), &path, &mut issues);
                        if let Some(r) = retry
                            && (r.max_attempts < 2
                                || r.max_attempts > 16
                                || r.backoff_ms == 0
                                || r.backoff_ms > l.max_duration_ms
                                || r.outcomes.is_empty()
                                || r.outcomes.iter().any(|o| !c.outcomes.contains_key(o)))
                        {
                            issue(
                                &mut issues,
                                "retry",
                                &path,
                                "invalid bounded business retry policy",
                            );
                        }
                    } else {
                        issue(
                            &mut issues,
                            "unknown_command",
                            &path,
                            format!("unknown command {}", command.name),
                        );
                    }
                }
                Operation::Decide { .. } => required_edges(
                    step,
                    ["true", "false", "unknown"].map(String::from),
                    &path,
                    &mut issues,
                ),
                Operation::Wait {
                    event,
                    correlation,
                    minimum_revision,
                    deadline_ms,
                    ..
                } => {
                    if *deadline_ms == 0 || *deadline_ms > l.max_duration_ms {
                        issue(
                            &mut issues,
                            "deadline",
                            &path,
                            "wait deadline must be bounded by root duration",
                        );
                    }
                    if let Some(r) = event {
                        if let Some(e) = self.registry.events.get(&r.name) {
                            if e.fingerprint().ok().as_ref() != Some(&r.fingerprint) {
                                issue(
                                    &mut issues,
                                    "contract_fingerprint",
                                    &path,
                                    "event exact contract fingerprint mismatch",
                                );
                            }
                            if !e.scopes.contains(&p.scope) || *deadline_ms > e.retention_ms {
                                issue(
                                    &mut issues,
                                    "wait_horizon",
                                    &path,
                                    "event scope or retained correlation horizon unsupported",
                                );
                            }
                            if correlation.keys().cloned().collect::<BTreeSet<_>>()
                                != e.correlation_fields
                            {
                                issue(
                                    &mut issues,
                                    "correlation",
                                    &path,
                                    "wait must bind every exact event correlation field",
                                );
                            }
                            if minimum_revision.is_some() && e.revision_field.is_none() {
                                issue(
                                    &mut issues,
                                    "revision",
                                    &path,
                                    "minimum revision requires versioned event contract",
                                );
                            }
                        } else {
                            issue(
                                &mut issues,
                                "unknown_event",
                                &path,
                                format!("unknown event {}", r.name),
                            );
                        }
                        required_edges(
                            step,
                            ["received", "timeout"].map(String::from),
                            &path,
                            &mut issues,
                        );
                    } else {
                        if !correlation.is_empty() || minimum_revision.is_some() {
                            issue(
                                &mut issues,
                                "timer_correlation",
                                &path,
                                "timer cannot carry event correlation/revision",
                            );
                        }
                        required_edges(step, ["timeout".into()], &path, &mut issues);
                    }
                }
                Operation::ForEach {
                    item,
                    max_items,
                    concurrency,
                    ..
                } => {
                    if !valid_name(item)
                        || matches!(item.as_str(), "input" | "event" | "definitions" | "vars")
                        || all_bind_names.contains(item)
                        || !item_names.insert(item.clone())
                    {
                        issue(
                            &mut issues,
                            "binding",
                            &path,
                            "invalid/shadowing iteration item binding",
                        );
                    }
                    if *max_items == 0 || *max_items as u64 > l.max_iterations {
                        issue(
                            &mut issues,
                            "iteration_limit",
                            &path,
                            "finite item bound must fit root iteration budget",
                        );
                    }
                    if *concurrency != 1 {
                        issue(
                            &mut issues,
                            "unsupported_feature",
                            &path,
                            "format 1 runner supports serial foreach; use owned async calls and bounded joins for parallel work",
                        );
                    }
                }
                Operation::Call {
                    dependency,
                    entry,
                    mode,
                    ..
                } => {
                    if let Some(d) = self.dependencies.get(dependency) {
                        if let Some(e) = d.interfaces.get(entry) {
                            if *mode == CallMode::Wait {
                                required_edges(
                                    step,
                                    e.outcomes.keys().cloned(),
                                    &path,
                                    &mut issues,
                                );
                            } else {
                                required_edges(step, ["started".into()], &path, &mut issues);
                            }
                        } else {
                            issue(
                                &mut issues,
                                "unknown_interface",
                                &path,
                                "unknown child entry point",
                            );
                        }
                    } else {
                        issue(
                            &mut issues,
                            "unknown_dependency",
                            &path,
                            "unknown statically named dependency",
                        );
                    }
                }
                Operation::Join {
                    members,
                    member_keys,
                    mode,
                    accepted_outcomes,
                    deadline_ms,
                    ..
                } => {
                    if !member_keys.is_empty()
                        && (member_keys.len() != members.len()
                            || member_keys.iter().any(|key| !valid_name(key))
                            || member_keys.iter().collect::<BTreeSet<_>>().len()
                                != member_keys.len())
                    {
                        issue(
                            &mut issues,
                            "join_member_keys",
                            &path,
                            "join member keys must be unique valid identities with exact member cardinality",
                        );
                    }
                    if let Err(error) = self.join_schema(step, false) {
                        issue(&mut issues, "join_member_type", &path, error);
                    }
                    if *mode == JoinMode::AnyCompletion && !accepted_outcomes.is_empty() {
                        issue(
                            &mut issues,
                            "join_qualifier",
                            &path,
                            "any_completion cannot declare accepted_outcomes; use any_accepted to qualify outcomes",
                        );
                    }
                    if members.is_empty()
                        || members.len() > l.max_children as usize
                        || *deadline_ms == 0
                        || *deadline_ms > l.max_duration_ms
                        || (*mode == JoinMode::AnyAccepted && accepted_outcomes.is_empty())
                    {
                        issue(
                            &mut issues,
                            "join",
                            &path,
                            "join requires finite members, bounded deadline, qualifying outcomes for any_accepted",
                        );
                    }
                    required_edges(
                        step,
                        ["completed", "timeout", "no_match"].map(String::from),
                        &path,
                        &mut issues,
                    );
                }
                Operation::Return { .. } => {
                    if !step.edges.is_empty() || step.on_error.is_some() {
                        issue(
                            &mut issues,
                            "return_edges",
                            &path,
                            "return cannot declare transitions",
                        );
                    }
                }
            }
        }
        let mut trigger_ids = BTreeSet::new();
        for t in &p.triggers {
            let path = format!("triggers.{}", t.id);
            if !valid_name(&t.id) || !trigger_ids.insert(t.id.clone()) {
                issue(
                    &mut issues,
                    "trigger_identity",
                    &path,
                    "invalid or duplicate trigger identity",
                );
            }
            if !p.entries.contains_key(&t.entry) {
                issue(
                    &mut issues,
                    "unknown_interface",
                    &path,
                    "trigger entry does not exist",
                );
            }
            match self.registry.events.get(&t.event.name) {
                Some(e) => {
                    if e.fingerprint().ok().as_ref() != Some(&t.event.fingerprint) {
                        issue(
                            &mut issues,
                            "contract_fingerprint",
                            &path,
                            "trigger event fingerprint mismatch",
                        );
                    }
                    if !e.scopes.contains(&p.scope) {
                        issue(
                            &mut issues,
                            "authority",
                            &path,
                            "event scope incompatible with playbook",
                        );
                    }
                    if let Some(entry) = p.entries.get(&t.entry) {
                        let roots = BTreeMap::from([("event".into(), e.payload.clone())]);
                        check_expr_compat(&t.input, &roots, &entry.input, &path, &mut issues);
                    }
                }
                None => issue(&mut issues, "unknown_event", &path, "unknown trigger event"),
            }
        }
        let mut reachable = BTreeSet::new();
        for (name, e) in &p.entries {
            let mut local = BTreeSet::new();
            walk(&e.start, &p.steps, &mut local);
            reachable.extend(local.iter().cloned());
            self.validate_types(name, e, &local, &mut issues);
        }
        for name in p.steps.keys() {
            if !reachable.contains(name) {
                issue(
                    &mut issues,
                    "unreachable_step",
                    &format!("steps.{name}"),
                    "step is unreachable from every entry",
                );
            }
        }
        let mut done = BTreeSet::new();
        let mut active = BTreeSet::new();
        for name in p.steps.keys() {
            if unbounded_cycle(name, &p.steps, &mut done, &mut active) {
                issue(
                    &mut issues,
                    "unbounded_cycle",
                    &format!("steps.{name}"),
                    "control-flow cycle does not cross a bounded foreach anchor",
                );
                break;
            }
        }
        for (name, loc) in &self.source_map {
            if !name.starts_with("steps.")
                || !p.steps.contains_key(name.trim_start_matches("steps."))
                || loc.line == 0
                || loc.column == 0
            {
                issue(
                    &mut issues,
                    "source_map",
                    &format!("source_map.{name}"),
                    "source mapping must identify an existing step with positive line/column",
                );
            }
        }
        match canonical_bytes(self) {
            Err(e) => issue(&mut issues, "canonical", "artifact", e.to_string()),
            Ok(bytes) if bytes.len() > MAX_ARTIFACT_BYTES - 512 => issue(
                &mut issues,
                "artifact_limit",
                "artifact",
                "artifact content exceeds bounded 16 MiB envelope",
            ),
            _ => {}
        }
        issues
    }
}
fn walk(name: &str, steps: &BTreeMap<String, Step>, seen: &mut BTreeSet<String>) {
    let mut pending = vec![name.to_owned()];
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(s) = steps.get(&name) {
            pending.extend(children(s).into_iter().map(str::to_owned));
        }
    }
}
fn unbounded_cycle(
    name: &str,
    steps: &BTreeMap<String, Step>,
    done: &mut BTreeSet<String>,
    active: &mut BTreeSet<String>,
) -> bool {
    if done.contains(name) {
        return false;
    }
    if !active.insert(name.into()) {
        return true;
    }
    if let Some(s) = steps.get(name)
        && !matches!(s.op, Operation::ForEach { .. })
    {
        for n in children(s) {
            if unbounded_cycle(n, steps, done, active) {
                return true;
            }
        }
    }
    active.remove(name);
    done.insert(name.into());
    false
}

fn literal_schema(value: &Value) -> Result<Schema, String> {
    match value {
        Value::Null => Ok(Schema::Null),
        Value::Bool(_) => Ok(Schema::Boolean),
        Value::Number(n) => n
            .as_i64()
            .map(|n| Schema::Integer {
                minimum: n,
                maximum: n,
            })
            .ok_or_else(|| "integer outside i64 domain".into()),
        Value::String(s) => Ok(Schema::String {
            max_length: s.len().max(1),
            values: BTreeSet::from([s.clone()]),
        }),
        Value::Array(a) => {
            let mut item = None;
            for v in a {
                let s = literal_schema(v)?;
                item = Some(match item {
                    None => s,
                    Some(old) => {
                        merge_schema(&old, &s).ok_or("heterogeneous arrays unsupported")?
                    }
                });
            }
            Ok(Schema::Array {
                items: Box::new(item.unwrap_or(Schema::Null)),
                max_items: a.len().max(1),
            })
        }
        Value::Object(o) => Ok(Schema::object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), literal_schema(v)?)))
                .collect::<Result<_, String>>()?,
        )),
    }
}
fn merge_schema(a: &Schema, b: &Schema) -> Option<Schema> {
    if a == b {
        return Some(a.clone());
    }
    match (a, b) {
        (Schema::Null, b @ Schema::Nullable { .. })
        | (b @ Schema::Nullable { .. }, Schema::Null) => Some(b.clone()),
        (Schema::Null, b) => Some(Schema::Nullable {
            inner: Box::new(b.clone()),
        }),
        (a, Schema::Null) => Some(Schema::Nullable {
            inner: Box::new(a.clone()),
        }),
        (Schema::Nullable { inner: a }, Schema::Nullable { inner: b }) => Some(Schema::Nullable {
            inner: Box::new(merge_schema(a, b)?),
        }),
        (Schema::Nullable { inner: a }, b) | (b, Schema::Nullable { inner: a }) => {
            Some(Schema::Nullable {
                inner: Box::new(merge_schema(a, b)?),
            })
        }
        (
            Schema::Integer {
                minimum: a,
                maximum: b,
            },
            Schema::Integer {
                minimum: c,
                maximum: d,
            },
        ) => Some(Schema::Integer {
            minimum: (*a).min(*c),
            maximum: (*b).max(*d),
        }),
        (
            Schema::String {
                max_length: a,
                values: x,
            },
            Schema::String {
                max_length: b,
                values: y,
            },
        ) => Some(Schema::String {
            max_length: (*a).max(*b),
            values: if x.is_empty() || y.is_empty() {
                BTreeSet::new()
            } else {
                x.union(y).cloned().collect()
            },
        }),
        (
            Schema::Array {
                items: a,
                max_items: x,
            },
            Schema::Array {
                items: b,
                max_items: y,
            },
        ) => Some(Schema::Array {
            items: Box::new(merge_schema(a, b)?),
            max_items: (*x).max(*y),
        }),
        (
            Schema::Object {
                properties: a,
                required: x,
            },
            Schema::Object {
                properties: b,
                required: y,
            },
        ) => {
            let mut properties = a.clone();
            for (k, v) in b {
                if let Some(old) = properties.get(k) {
                    properties.insert(k.clone(), merge_schema(old, v)?);
                } else {
                    properties.insert(k.clone(), v.clone());
                }
            }
            Some(Schema::Object {
                properties,
                required: x.intersection(y).cloned().collect(),
            })
        }
        _ => {
            let mut variants = vec![];
            for schema in [a, b] {
                match schema {
                    Schema::Union { variants: inner } => variants.extend(inner.iter().cloned()),
                    other => variants.push(other.clone()),
                }
            }
            variants.sort_by_key(|schema| canonical_bytes(schema).unwrap_or_default());
            variants.dedup();
            if variants.len() > 16 {
                None
            } else {
                Some(Schema::Union { variants })
            }
        }
    }
}
fn union_schemas<'a>(schemas: impl Iterator<Item = &'a Schema>) -> Option<Schema> {
    let mut result = None;
    for s in schemas {
        result = Some(match result {
            None => s.clone(),
            Some(old) => merge_schema(&old, s)?,
        });
    }
    result
}
fn infer_expr(
    expr: &Expr,
    roots: &BTreeMap<String, Schema>,
    depth: usize,
) -> Result<Schema, String> {
    if depth > 32 {
        return Err("expression nesting exceeds 32".into());
    }
    let sub = |e: &Expr| infer_expr(e, roots, depth + 1);
    match expr {
        Expr::Literal { value } => literal_schema(value),
        Expr::Ref { root, path } => {
            let current = roots
                .get(root)
                .ok_or_else(|| format!("unknown or unsupported typed root {root}"))?;
            schema_path(current, path)
        }
        Expr::Object { fields } => Ok(Schema::object(
            fields
                .iter()
                .map(|(k, e)| Ok((k.clone(), sub(e)?)))
                .collect::<Result<_, String>>()?,
        )),
        Expr::Array { items } => {
            let mut s = None;
            for e in items {
                let x = sub(e)?;
                s = Some(match s {
                    None => x,
                    Some(old) => merge_schema(&old, &x)
                        .ok_or("heterogeneous expression array unsupported")?,
                });
            }
            Ok(Schema::Array {
                items: Box::new(s.unwrap_or(Schema::Null)),
                max_items: items.len().max(1),
            })
        }
        Expr::Exists { value } => {
            sub(value)?;
            Ok(Schema::Boolean)
        }
        Expr::Not { value } => {
            if sub(value)? != Schema::Boolean {
                return Err("not requires boolean expression".into());
            }
            Ok(Schema::Boolean)
        }
        Expr::And { values } | Expr::Or { values } => {
            for e in values {
                if sub(e)? != Schema::Boolean {
                    return Err("boolean logic requires booleans".into());
                }
            }
            Ok(Schema::Boolean)
        }
        Expr::Eq { left, right } => {
            let a = sub(left)?;
            let b = sub(right)?;
            if merge_schema(&a, &b).is_none() {
                return Err("equality compares incompatible types".into());
            }
            Ok(Schema::Boolean)
        }
        Expr::Lt { left, right } => {
            let a = sub(left)?;
            let b = sub(right)?;
            if !matches!(
                (&a, &b),
                (Schema::Integer { .. }, Schema::Integer { .. })
                    | (Schema::String { .. }, Schema::String { .. })
            ) {
                return Err("ordering requires two integers or strings".into());
            }
            Ok(Schema::Boolean)
        }
        Expr::Contains { collection, value } => {
            let Schema::Array { items, .. } = sub(collection)? else {
                return Err("membership requires bounded array".into());
            };
            if merge_schema(&items, &sub(value)?).is_none() {
                return Err("membership value has incompatible type".into());
            }
            Ok(Schema::Boolean)
        }
        Expr::Map {
            collection,
            item,
            value,
            max_items,
        }
        | Expr::Filter {
            collection,
            item,
            predicate: value,
            max_items,
        } => {
            let Schema::Array {
                items,
                max_items: bound,
            } = sub(collection)?
            else {
                return Err("collection expression requires bounded array".into());
            };
            if *max_items == 0 || bound > *max_items || *max_items > 100_000 {
                return Err("collection expression limit cannot cover input schema".into());
            }
            if roots.contains_key(item) || !valid_name(item) {
                return Err("invalid/shadowing collection lexical root".into());
            }
            let mut local = roots.clone();
            local.insert(item.clone(), *items.clone());
            let result = infer_expr(value, &local, depth + 1)?;
            if matches!(expr, Expr::Filter { .. }) {
                if result != Schema::Boolean {
                    return Err("filter predicate requires boolean".into());
                }
                Ok(Schema::Array {
                    items,
                    max_items: bound,
                })
            } else {
                Ok(Schema::Array {
                    items: Box::new(result),
                    max_items: bound,
                })
            }
        }
        Expr::Difference {
            left,
            right,
            identity,
            max_items,
        } => {
            let Schema::Array {
                items: a,
                max_items: x,
            } = sub(left)?
            else {
                return Err("difference requires bounded arrays".into());
            };
            let Schema::Array {
                items: b,
                max_items: y,
            } = sub(right)?
            else {
                return Err("difference requires bounded arrays".into());
            };
            if *max_items == 0 || x > *max_items || y > *max_items || identity.is_empty() {
                return Err(
                    "difference requires explicit identity and covering element bound".into(),
                );
            }
            let roots = BTreeMap::from([("left".into(), *a.clone()), ("right".into(), *b)]);
            let a_key = infer_expr(
                &Expr::Ref {
                    root: "left".into(),
                    path: identity.clone(),
                },
                &roots,
                depth + 1,
            )?;
            let b_key = infer_expr(
                &Expr::Ref {
                    root: "right".into(),
                    path: identity.clone(),
                },
                &roots,
                depth + 1,
            )?;
            if merge_schema(&a_key, &b_key).is_none() {
                return Err("difference identities incompatible".into());
            }
            Ok(Schema::Array {
                items: a,
                max_items: x,
            })
        }
    }
}
fn check_expr_compat(
    expr: &Expr,
    roots: &BTreeMap<String, Schema>,
    target: &Schema,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    // Empty literals and arrays have no inherent element type; validate literal
    // values directly against the expected schema rather than inventing one.
    if let Expr::Literal { value } = expr {
        if let Err(e) = target.validate(value) {
            issue(issues, "schema_compatibility", path, e.to_string());
        }
        return;
    }
    if let (
        Expr::Object { fields },
        Schema::Object {
            properties,
            required,
        },
    ) = (expr, target)
    {
        if required.iter().any(|k| !fields.contains_key(k))
            || fields.keys().any(|k| !properties.contains_key(k))
        {
            issue(
                issues,
                "schema_compatibility",
                path,
                "expression object keys incompatible with closed target schema",
            );
        }
        for (k, e) in fields {
            if let Some(t) = properties.get(k) {
                check_expr_compat(e, roots, t, path, issues);
            }
        }
        return;
    }
    if let (
        Expr::Array { items },
        Schema::Array {
            items: target,
            max_items,
        },
    ) = (expr, target)
    {
        if items.len() > *max_items {
            issue(
                issues,
                "schema_compatibility",
                path,
                "expression array exceeds target bound",
            );
        }
        for e in items {
            check_expr_compat(e, roots, target, path, issues);
        }
        return;
    }
    match infer_expr(expr, roots, 0) {
        Ok(source) => {
            if !source.assignable_to(target) {
                issue(
                    issues,
                    "schema_compatibility",
                    path,
                    "expression type/bounds not assignable to target schema",
                );
            }
        }
        Err(e) => issue(issues, "expression_type", path, e),
    }
}
impl ArtifactContent {
    fn validate_types(
        &self,
        entry_name: &str,
        entry: &EntryPoint,
        reachable: &BTreeSet<String>,
        issues: &mut Vec<ValidationIssue>,
    ) {
        let p = &self.playbook;
        let mut roots = BTreeMap::from([("input".into(), entry.input.clone())]);
        if let Ok(definitions) = literal_schema(&Value::Object(
            self.definitions
                .iter()
                .map(|(k, d)| (k.clone(), d.value.clone()))
                .collect(),
        )) {
            roots.insert("definitions".into(), definitions);
        }
        for name in reachable {
            let Some(s) = p.steps.get(name) else {
                continue;
            };
            match &s.op {
                Operation::Invoke { command, bind, .. } => {
                    if let Some(c) = self.registry.commands.get(&command.name)
                        && let Some(s) = union_schemas(c.outcomes.values())
                    {
                        roots.insert(bind.clone(), s);
                    }
                }
                Operation::Wait {
                    event: Some(event),
                    bind,
                    ..
                } => {
                    if let Some(e) = self.registry.events.get(&event.name) {
                        roots.insert(bind.clone(), e.payload.clone());
                    }
                }
                Operation::Call {
                    dependency,
                    entry,
                    mode,
                    bind,
                    ..
                } => {
                    if *mode == CallMode::Wait {
                        if let Some(e) = self
                            .dependencies
                            .get(dependency)
                            .and_then(|d| d.interfaces.get(entry))
                            && let Some(s) = union_schemas(e.outcomes.values())
                        {
                            roots.insert(bind.clone(), s);
                        }
                    } else {
                        roots.insert(
                            bind.clone(),
                            Schema::object(BTreeMap::from([("child".into(), Schema::string(128))])),
                        );
                    }
                }
                Operation::Join { bind, .. } => {
                    if let Ok(Some(schema)) = self.join_schema(s, false) {
                        roots.insert(bind.clone(), schema);
                    }
                }
                _ => {}
            }
        }
        // Iteration collections may depend on an outer captured iteration item.
        // Resolve lexical item schemas to a fixed point bounded by step count.
        for _ in 0..=p.limits.max_iteration_depth {
            let old = roots.len();
            for name in reachable {
                if let Some(Step {
                    op:
                        Operation::ForEach {
                            collection, item, ..
                        },
                    ..
                }) = p.steps.get(name)
                    && let Ok(Schema::Array { items, .. }) = infer_expr(collection, &roots, 0)
                {
                    roots.entry(item.clone()).or_insert(*items);
                }
            }
            if old == roots.len() {
                break;
            }
        }
        let contexts = self.outcome_contexts(entry, &roots);
        for name in reachable {
            let Some(s) = p.steps.get(name) else {
                continue;
            };
            let mut roots = contexts
                .get(name)
                .map(|c| c.roots.clone())
                .unwrap_or_else(|| roots.clone());
            let vars = Schema::Object {
                properties: roots
                    .iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "input" | "definitions" | "vars"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                required: BTreeSet::new(),
            };
            roots.insert("vars".into(), vars);
            let path = format!("steps.{name}");
            let infer = |e: &Expr, issues: &mut Vec<ValidationIssue>| {
                if let Err(e) = infer_expr(e, &roots, 0) {
                    issue(issues, "expression_type", &path, e);
                }
            };
            match &s.op {
                Operation::Invoke { command, input, .. } => {
                    if let Some(c) = self.registry.commands.get(&command.name) {
                        check_expr_compat(input, &roots, &c.input, &path, issues);
                    }
                }
                Operation::Decide { predicate } => {
                    check_expr_compat(predicate, &roots, &Schema::Boolean, &path, issues)
                }
                Operation::Wait {
                    correlation,
                    minimum_revision,
                    ..
                } => {
                    for e in correlation.values() {
                        check_expr_compat(e, &roots, &Schema::string(1024), &path, issues);
                    }
                    if let Some(e) = minimum_revision {
                        check_expr_compat(
                            e,
                            &roots,
                            &Schema::Integer {
                                minimum: 0,
                                maximum: i64::MAX,
                            },
                            &path,
                            issues,
                        );
                    }
                }
                Operation::ForEach {
                    collection,
                    max_items,
                    ..
                } => match infer_expr(collection, &roots, 0) {
                    Ok(Schema::Array {
                        max_items: bound, ..
                    }) => {
                        if bound > *max_items {
                            issue(
                                issues,
                                "iteration_limit",
                                &path,
                                "foreach item bound does not cover collection schema",
                            );
                        }
                    }
                    Ok(_) => issue(
                        issues,
                        "expression_type",
                        &path,
                        "foreach collection must be bounded array",
                    ),
                    Err(e) => issue(issues, "expression_type", &path, e),
                },
                Operation::Call {
                    dependency,
                    entry,
                    input,
                    ..
                } => {
                    if let Some(e) = self
                        .dependencies
                        .get(dependency)
                        .and_then(|d| d.interfaces.get(entry))
                    {
                        check_expr_compat(input, &roots, &e.input, &path, issues);
                    }
                }
                Operation::Join { members, .. } => {
                    for e in members {
                        infer(e, issues);
                    }
                }
                Operation::Return { outcome, output } => {
                    if let Some(target) = entry.outcomes.get(outcome) {
                        check_expr_compat(output, &roots, target, &path, issues);
                    } else {
                        issue(
                            issues,
                            "return_outcome",
                            &path,
                            format!("outcome {outcome} not declared by entry {entry_name}"),
                        );
                    }
                }
            }
        }
    }
}

#[derive(Clone, PartialEq)]
struct TypeState {
    roots: BTreeMap<String, Schema>,
    untyped: BTreeSet<String>,
}
impl TypeState {
    fn merge(&mut self, other: &Self) -> bool {
        let before = self.clone();
        self.untyped.extend(other.untyped.iter().cloned());
        for key in self
            .roots
            .keys()
            .chain(other.roots.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
        {
            match (self.roots.get(&key), other.roots.get(&key)) {
                (Some(a), Some(b)) if !self.untyped.contains(&key) => {
                    if let Some(s) = merge_schema(a, b) {
                        self.roots.insert(key, s);
                    } else {
                        self.untyped.insert(key.clone());
                        self.roots.remove(&key);
                    }
                }
                _ => {
                    self.untyped.insert(key.clone());
                    self.roots.remove(&key);
                }
            }
        }
        *self != before
    }
    fn narrow(&mut self, root: &str, schema: &Schema) {
        self.untyped.remove(root);
        self.roots.insert(root.into(), schema.clone());
    }
}
impl ArtifactContent {
    fn outcome_contexts(
        &self,
        entry: &EntryPoint,
        roots: &BTreeMap<String, Schema>,
    ) -> BTreeMap<String, TypeState> {
        let mut contexts = BTreeMap::from([(
            entry.start.clone(),
            TypeState {
                roots: roots.clone(),
                untyped: BTreeSet::new(),
            },
        )]);
        let mut pending = std::collections::VecDeque::from([entry.start.clone()]);
        let mut queued = BTreeSet::from([entry.start.clone()]);
        while let Some(name) = pending.pop_front() {
            queued.remove(&name);
            let Some(step) = self.playbook.steps.get(&name) else {
                continue;
            };
            let current = contexts[&name].clone();
            let mut targets: Vec<(String, TypeState)> = vec![];
            for (outcome, target) in &step.edges {
                let mut next = current.clone();
                match &step.op {
                    Operation::Invoke { command, bind, .. } => {
                        if let Some(schema) = self
                            .registry
                            .commands
                            .get(&command.name)
                            .and_then(|c| c.outcomes.get(outcome))
                        {
                            next.narrow(bind, schema);
                        }
                    }
                    Operation::Call {
                        dependency,
                        entry,
                        mode: CallMode::Wait,
                        bind,
                        ..
                    } => {
                        if let Some(schema) = self
                            .dependencies
                            .get(dependency)
                            .and_then(|d| d.interfaces.get(entry))
                            .and_then(|e| e.outcomes.get(outcome))
                        {
                            next.narrow(bind, schema);
                        }
                    }
                    Operation::Join { bind, .. } if outcome == "completed" => {
                        if let Ok(Some(schema)) = self.join_schema(step, true) {
                            next.narrow(bind, &schema);
                        }
                    }
                    _ => {}
                }
                targets.push((target.clone(), next));
            }
            if let Some(target) = &step.on_error {
                targets.push((target.clone(), current.clone()));
            }
            if let Operation::ForEach { body, next, .. } = &step.op {
                targets.push((body.clone(), current.clone()));
                targets.push((next.clone(), current.clone()));
            }
            for (target, next) in targets {
                let changed = if let Some(existing) = contexts.get_mut(&target) {
                    existing.merge(&next)
                } else {
                    contexts.insert(target.clone(), next);
                    true
                };
                if changed && queued.insert(target.clone()) {
                    pending.push_back(target);
                }
            }
        }
        contexts
    }
}

fn schema_path(schema: &Schema, path: &[String]) -> Result<Schema, String> {
    let Some((key, rest)) = path.split_first() else {
        return Ok(schema.clone());
    };
    match schema {
        Schema::Object { properties, .. } => schema_path(
            properties
                .get(key)
                .ok_or_else(|| format!("unknown field {key}"))?,
            rest,
        ),
        Schema::Array { items, .. } => {
            key.parse::<usize>()
                .map_err(|_| "array index must be nonnegative integer".to_string())?;
            schema_path(items, rest)
        }
        Schema::Union { variants } => {
            let schemas = variants
                .iter()
                .map(|schema| schema_path(schema, path))
                .collect::<Result<Vec<_>, _>>()?;
            union_schemas(schemas.iter())
                .ok_or_else(|| "union projection exceeds alternative limit".into())
        }
        _ => Err(format!(
            "cannot traverse scalar/nullable reference at {key}"
        )),
    }
}
impl ArtifactContent {
    fn member_outcomes(&self, member: &Expr) -> Result<BTreeMap<String, Schema>, String> {
        let Expr::Ref { root, path } = member else {
            return Err("named join members must reference statically declared owned child or operation handles".into());
        };
        let (binding, path) = if root == "vars" {
            let Some((binding, path)) = path.split_first() else {
                return Err("join member vars reference needs a handle name".into());
            };
            (binding.as_str(), path)
        } else {
            (root.as_str(), path.as_slice())
        };
        for step in self.playbook.steps.values() {
            match &step.op {
                Operation::Call {
                    dependency,
                    entry,
                    mode: CallMode::OwnedAsync,
                    bind,
                    ..
                } if bind == binding && (path.is_empty() || path == ["child"]) => {
                    return self
                        .dependencies
                        .get(dependency)
                        .and_then(|d| d.interfaces.get(entry))
                        .map(|entry| entry.outcomes.clone())
                        .ok_or_else(|| "join references unknown child interface".into());
                }
                Operation::Invoke { command, bind, .. }
                    if bind == binding && (path.is_empty() || path == ["handle"]) =>
                {
                    let command = self
                        .registry
                        .commands
                        .get(&command.name)
                        .ok_or("join references unknown command")?;
                    let asynchronous = command
                        .asynchronous
                        .as_ref()
                        .ok_or("join operation must have asynchronous contract")?;
                    let Schema::Object { properties, .. } = &asynchronous.completion else {
                        return Err("async completion must be object".into());
                    };
                    let Some(Schema::String { values, .. }) = properties.get("outcome") else {
                        return Err("async completion requires outcome enum".into());
                    };
                    return Ok(command
                        .outcomes
                        .iter()
                        .filter(|(outcome, _)| values.contains(*outcome))
                        .map(|(name, schema)| (name.clone(), schema.clone()))
                        .collect());
                }
                _ => {}
            }
        }
        Err(format!(
            "join member {binding} is not a statically declared owned asynchronous handle"
        ))
    }
    fn join_schema(&self, step: &Step, completed: bool) -> Result<Option<Schema>, String> {
        let Operation::Join {
            members,
            member_keys,
            mode,
            accepted_outcomes,
            ..
        } = &step.op
        else {
            return Ok(None);
        };
        if member_keys.is_empty() {
            return Ok(None);
        }
        if member_keys.len() != members.len() {
            return Err("join key/member cardinality mismatch".into());
        }
        let guaranteed_business = completed && *mode != JoinMode::AnyCompletion;
        let mut properties = BTreeMap::new();
        for (key, member) in member_keys.iter().zip(members) {
            let outcomes = self.member_outcomes(member)?;
            let selected: BTreeMap<_, _> = outcomes
                .iter()
                .filter(|(outcome, _)| {
                    !guaranteed_business
                        || accepted_outcomes.is_empty()
                        || accepted_outcomes.contains(*outcome)
                })
                .map(|(outcome, schema)| (outcome.clone(), schema.clone()))
                .collect();
            // A member whose vocabulary has no qualifying outcome cannot appear
            // in an AnyAccepted result; the optional key is absent in that case.
            if guaranteed_business && selected.is_empty() {
                continue;
            }
            let output = union_schemas(selected.values())
                .ok_or("join member result exceeds bounded union limit")?;
            let outcome = Schema::String {
                max_length: selected.keys().map(String::len).max().unwrap_or(1).max(1),
                values: selected.keys().cloned().collect(),
            };
            let (outcome, output) = if guaranteed_business {
                (outcome, output)
            } else {
                (
                    Schema::Nullable {
                        inner: Box::new(outcome),
                    },
                    Schema::Nullable {
                        inner: Box::new(output),
                    },
                )
            };
            properties.insert(
                key.clone(),
                Schema::object(BTreeMap::from([
                    ("participant".into(), Schema::string(128)),
                    ("outcome".into(), outcome),
                    ("output".into(), output),
                ])),
            );
        }
        let required = if guaranteed_business && *mode == JoinMode::All {
            properties.keys().cloned().collect()
        } else {
            BTreeSet::new()
        };
        Ok(Some(Schema::Object {
            properties,
            required,
        }))
    }
}
