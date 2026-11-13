//! Route types for the routing policy (spec 0011, section 3). Pure: no I/O.

pub mod classify;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
pub mod plan;
pub mod policy;

pub use plan::{Classified, Plan, TaskFacts, plan};

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Metadata key under which the compact route is stored.
pub const ROUTE_KEY: &str = "aq:route";
/// Largest allowed JSON size of a `CompactRoute`.
pub const COMPACT_ROUTE_MAX_BYTES: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown class {0:?}")]
    UnknownClass(String),
    #[error("compact route is {bytes} bytes, over the {max} byte limit")]
    TooLarge { bytes: usize, max: usize },
    #[error("invalid {key} metadata: {message}")]
    Metadata { key: &'static str, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Parent {
    Simple,
    Standard,
    Complex,
    Deep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Class {
    pub parent: Parent,
    pub effort: Effort,
}

impl Parent {
    pub fn as_str(self) -> &'static str {
        match self {
            Parent::Simple => "simple",
            Parent::Standard => "standard",
            Parent::Complex => "complex",
            Parent::Deep => "deep",
        }
    }
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.parent.as_str(), self.effort.as_str())
    }
}

impl FromStr for Class {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let bad = || Error::UnknownClass(s.to_string());
        let (p, e) = s.split_once('-').ok_or_else(bad)?;
        let parent = match p {
            "simple" => Parent::Simple,
            "standard" => Parent::Standard,
            "complex" => Parent::Complex,
            "deep" => Parent::Deep,
            _ => return Err(bad()),
        };
        let effort = match e {
            "low" => Effort::Low,
            "medium" => Effort::Medium,
            "high" => Effort::High,
            "max" => Effort::Max,
            _ => return Err(bad()),
        };
        Ok(Class { parent, effort })
    }
}

impl Serialize for Class {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Class {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Router,
    Owner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderIntent {
    Any,
    Pinned,
}

/// Codes: hint_clamped, class_raised_for_risk, risk_relaxed, risk_assumed,
/// escalated_after_failure, deep_requires_reasoning, lane_matched,
/// reserved_harness_removed, classification_failed, owner_set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteReason {
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub class: Class,
    pub harnesses: Vec<String>,
    pub providers: Vec<String>,
    pub prefer: Vec<String>,
    pub lane: Option<String>,
    pub provider_intent: ProviderIntent,
    pub reasons: Vec<RouteReason>,
    pub classification: Option<classify::Classification>,
    pub policy_digest: Option<String>,
    pub structure_revision: Option<u64>,
    pub source: Source,
}

/// What is stored under `aq:route`. No reasons, no classification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactRoute {
    pub c: Class,
    #[serde(default)]
    pub h: Vec<String>,
    #[serde(default)]
    pub p: Vec<String>,
    #[serde(default)]
    pub pf: Vec<String>,
    pub l: Option<String>,
    pub i: ProviderIntent,
    pub d: Option<String>,
    pub s: Source,
    pub n: u64,
}

/// Restrictions a route places on selection. An empty `harnesses` or
/// `providers` means no restriction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteLimits {
    pub harnesses: Vec<String>,
    pub providers: Vec<String>,
    pub prefer: Vec<String>,
    pub pinned: bool,
}

impl Route {
    /// Compact form for storage; `sequence` is stored as `n`. `d` is the first
    /// 16 hex characters of the policy digest.
    pub fn compact(&self, sequence: u64) -> Result<CompactRoute, Error> {
        let compact = CompactRoute {
            c: self.class,
            h: self.harnesses.clone(),
            p: self.providers.clone(),
            pf: self.prefer.clone(),
            l: self.lane.clone(),
            i: self.provider_intent,
            d: self
                .policy_digest
                .as_ref()
                .map(|d| d.chars().take(16).collect()),
            s: self.source,
            n: sequence,
        };
        let bytes = serde_json::to_vec(&compact)
            .map_err(|e| Error::Metadata {
                key: ROUTE_KEY,
                message: e.to_string(),
            })?
            .len();
        if bytes > COMPACT_ROUTE_MAX_BYTES {
            return Err(Error::TooLarge {
                bytes,
                max: COMPACT_ROUTE_MAX_BYTES,
            });
        }
        Ok(compact)
    }
}

impl CompactRoute {
    /// Reads `aq:route` from task metadata. A missing key is `None`.
    pub fn from_metadata(metadata: &serde_json::Value) -> Result<Option<Self>, Error> {
        match metadata.get(ROUTE_KEY) {
            None => Ok(None),
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .map_err(|e| Error::Metadata {
                    key: ROUTE_KEY,
                    message: e.to_string(),
                }),
        }
    }

    pub fn limits(&self) -> RouteLimits {
        RouteLimits {
            harnesses: self.h.clone(),
            providers: self.p.clone(),
            prefer: self.pf.clone(),
            pinned: self.i == ProviderIntent::Pinned,
        }
    }
}

/// How placement treats a ready task (spec 0011 §4).
#[derive(Clone, Debug, PartialEq)]
pub enum Placement {
    /// Placed as without routing: the binding is not on an automatic router, or
    /// routing is off and the task has no route.
    Unchanged,
    /// Not offered: routing is on and the task has no route yet.
    Wait,
    /// Not offered and routed again: the route cannot be read, or its class has no profile.
    Reroute,
    /// Placed on the route's class profile within its limits.
    Routed(CompactRoute),
}

impl Placement {
    /// The placement of a task with `metadata` under a binding on an automatic
    /// router or not. A route that exists is honored with routing off too.
    pub fn of(automatic_router: bool, routing_on: bool, metadata: &serde_json::Value) -> Self {
        if !automatic_router {
            return Placement::Unchanged;
        }
        match CompactRoute::from_metadata(metadata) {
            Err(_) => Placement::Reroute,
            Ok(Some(route)) => Placement::Routed(route),
            Ok(None) if routing_on => Placement::Wait,
            Ok(None) => Placement::Unchanged,
        }
    }
}

/// Outcome of writing or clearing a task's route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    Routed {
        sequence: u64,
    },
    /// The task changed since the expected version, or an owner route blocks the router.
    Stale,
}

/// One entry of a task's route history.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredRoute {
    pub sequence: u64,
    /// `None` records a cleared route.
    pub route: Option<Route>,
    pub at_ms: i64,
}

/// Whether a write by `by` may replace the stored route: the router never
/// overwrites an owner route.
pub fn may_replace(stored: Option<&CompactRoute>, by: Source) -> bool {
    !(by == Source::Router && stored.is_some_and(|r| r.s == Source::Owner))
}

/// Task metadata with `aq:route` set to `route`, or removed for `None`. A
/// graph metadata patch replaces the object, so every other key is kept.
pub fn with_route(
    metadata: &serde_json::Value,
    route: Option<&CompactRoute>,
) -> Result<serde_json::Value, Error> {
    let bad = |message: String| Error::Metadata {
        key: ROUTE_KEY,
        message,
    };
    let mut object = metadata
        .as_object()
        .cloned()
        .ok_or_else(|| bad("task metadata is not an object".into()))?;
    match route {
        Some(r) => {
            let value = serde_json::to_value(r).map_err(|e| bad(e.to_string()))?;
            object.insert(ROUTE_KEY.into(), value);
        }
        None => {
            object.remove(ROUTE_KEY);
        }
    }
    Ok(serde_json::Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn class(s: &str) -> Class {
        s.parse().unwrap()
    }

    fn route() -> Route {
        Route {
            class: class("standard-high"),
            harnesses: vec!["codex".into()],
            providers: vec!["openai".into()],
            prefer: vec!["openai".into()],
            lane: Some("lane".into()),
            provider_intent: ProviderIntent::Pinned,
            reasons: vec![RouteReason {
                code: "lane_matched".into(),
                detail: "d".into(),
            }],
            classification: None,
            policy_digest: Some("0123456789abcdef0123456789abcdef".into()),
            structure_revision: Some(3),
            source: Source::Router,
        }
    }

    #[test]
    fn class_round_trips_as_parent_dash_effort_and_orders_by_parent_then_effort() {
        let c = class("standard-high");
        assert_eq!(
            c,
            Class {
                parent: Parent::Standard,
                effort: Effort::High
            }
        );
        assert_eq!(c.to_string(), "standard-high");
        assert_eq!(serde_json::to_string(&c).unwrap(), "\"standard-high\"");
        assert_eq!(
            serde_json::from_str::<Class>("\"deep-max\"").unwrap(),
            class("deep-max")
        );
        assert!(class("standard-high") < class("complex-low"));
        assert!(class("standard-low") < class("standard-max"));
    }

    #[test]
    fn unknown_parent_or_effort_is_an_error() {
        assert!("fast-high".parse::<Class>().is_err());
        assert!("standard-ultra".parse::<Class>().is_err());
        assert!("standard".parse::<Class>().is_err());
        assert!(serde_json::from_str::<Class>("\"fast-high\"").is_err());
    }

    #[test]
    fn compact_route_fits_2048_bytes_and_refuses_an_oversized_allowlist() {
        let compact = route().compact(7).unwrap();
        assert_eq!(compact.d.as_deref(), Some("0123456789abcdef"));
        assert_eq!(compact.n, 7);
        assert!(serde_json::to_vec(&compact).unwrap().len() <= 2048);
        let limits = compact.limits();
        assert!(limits.pinned);
        assert_eq!(limits.harnesses, vec!["codex".to_string()]);
        let mut big = route();
        big.providers = (0..200).map(|i| format!("provider-number-{i}")).collect();
        assert!(matches!(big.compact(1), Err(Error::TooLarge { .. })));
    }

    #[test]
    fn from_metadata_reads_aq_route_and_treats_a_missing_key_as_none_and_garbage_as_an_error() {
        let compact = route().compact(2).unwrap();
        let md = json!({"aq:route": serde_json::to_value(&compact).unwrap(), "other": 1});
        assert_eq!(CompactRoute::from_metadata(&md).unwrap(), Some(compact));
        assert_eq!(
            CompactRoute::from_metadata(&json!({"other": 1})).unwrap(),
            None
        );
        assert_eq!(CompactRoute::from_metadata(&json!(null)).unwrap(), None);
        assert!(CompactRoute::from_metadata(&json!({"aq:route": "garbage"})).is_err());
    }

    #[test]
    fn router_may_not_replace_an_owner_route_but_the_owner_may_replace_anything() {
        let router = route().compact(1).unwrap();
        let mut owner = router.clone();
        owner.s = Source::Owner;
        assert!(may_replace(None, Source::Router));
        assert!(may_replace(Some(&router), Source::Router));
        assert!(!may_replace(Some(&owner), Source::Router));
        assert!(may_replace(Some(&owner), Source::Owner));
        assert!(may_replace(Some(&router), Source::Owner));
    }

    #[test]
    fn placement_honors_a_route_only_on_an_automatic_router_and_waits_only_with_routing_on() {
        let compact = route().compact(3).unwrap();
        let routed = json!({"aq:route": serde_json::to_value(&compact).unwrap()});
        let unrouted = json!({"aq:priority": 1});
        let garbage = json!({"aq:route": {"c": "fast-high"}});
        for on in [false, true] {
            for md in [&routed, &unrouted, &garbage] {
                assert_eq!(Placement::of(false, on, md), Placement::Unchanged);
            }
            assert_eq!(
                Placement::of(true, on, &routed),
                Placement::Routed(compact.clone())
            );
            assert_eq!(Placement::of(true, on, &garbage), Placement::Reroute);
        }
        assert_eq!(Placement::of(true, true, &unrouted), Placement::Wait);
        assert_eq!(Placement::of(true, false, &unrouted), Placement::Unchanged);
    }

    #[test]
    fn with_route_sets_or_removes_aq_route_and_keeps_other_keys() {
        let compact = route().compact(4).unwrap();
        let md = json!({"aq:priority": 5});
        let set = with_route(&md, Some(&compact)).unwrap();
        assert_eq!(set["aq:priority"], 5);
        assert_eq!(CompactRoute::from_metadata(&set).unwrap(), Some(compact));
        let cleared = with_route(&set, None).unwrap();
        assert_eq!(cleared, md);
        assert!(with_route(&json!(null), None).is_err());
    }
}
