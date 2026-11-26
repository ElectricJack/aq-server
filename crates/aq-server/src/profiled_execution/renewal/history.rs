//! Immutable API-owned renewal receipts; transport stamps are not retained as
//! authority. Same-privilege direct SQL remains the trusted operator boundary.
use super::health::Evidence;
use crate::error::AppError;
use aq_admission::{
    Digest, LaunchBinding,
    control::{DeliveryStamp, RenewedAuthority, Versions},
    scoped_presence::AuthorityAnchor,
    transport::{DeliveryClock, Renew},
};
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub const MAX_PER_ATTEMPT: usize = 256;
pub const MAX_PER_NODE: i64 = 4096;
pub const MAX_GLOBAL: i64 = 65536;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub operation_id: Uuid,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub reservation_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub journal_revision: u64,
    pub execution_deadline_ms: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub versions: Versions,
    pub health: Evidence,
    pub registry_revision: u64,
    pub inventory_revision: u64,
    pub binding_observation_digest: Digest,
    pub task_observation_digest: Digest,
    pub source_observation_digest: Digest,
    pub material_observed_at_ms: u64,
    pub task_observed_at_ms: u64,
    pub task_received_at_ms: u64,
    pub task_expires_at_ms: u64,
    pub source_observed_at_ms: u64,
    pub source_expires_at_ms: u64,
    pub binding_observed_at_ms: u64,
    pub binding_expires_at_ms: u64,
    pub inventory_expires_at_ms: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stored {
    schema_version: u32,
    kind: Kind,
    pub body: Body,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Renewed,
}
fn invalid() -> AppError {
    AppError::Conflict("renewal receipt identity or history changed".into())
}
fn ensure(consistent: bool) -> Result<(), AppError> {
    if consistent { Ok(()) } else { Err(invalid()) }
}
fn request_digest(request: &Renew) -> Result<Digest, AppError> {
    aq_selection::digest(request).map_err(|_| AppError::Internal)
}
impl Body {
    fn validate_identity(&self) -> Result<(), AppError> {
        let ids = [
            self.operation_id,
            self.grant_id,
            self.reservation_id,
            self.guardian_id,
            self.launch_id,
        ];
        let revisions = [
            self.versions.grant_revision,
            self.registry_revision,
            self.inventory_revision,
        ];
        ensure(
            !ids.iter().any(Uuid::is_nil)
                && (1..=i64::MAX as u64).contains(&self.journal_revision)
                && !revisions.contains(&0),
        )
    }
    fn validate_lifetime(&self) -> Result<(), AppError> {
        let ceilings = [
            self.execution_deadline_ms,
            self.task_expires_at_ms,
            self.source_expires_at_ms,
            self.binding_expires_at_ms,
            self.inventory_expires_at_ms,
        ];
        ensure(
            self.issued_at_ms != 0
                && self.expires_at_ms > self.issued_at_ms
                && ceilings
                    .iter()
                    .all(|ceiling| self.expires_at_ms <= *ceiling),
        )
    }
    fn validate_observations(&self) -> Result<(), AppError> {
        let observations = [
            (self.material_observed_at_ms, self.issued_at_ms),
            (self.task_observed_at_ms, self.task_received_at_ms),
            (self.source_observed_at_ms, self.issued_at_ms),
            (self.binding_observed_at_ms, self.issued_at_ms),
            (self.health.evaluated_at_ms, self.issued_at_ms),
        ];
        let ordered = |&(observed, received): &(u64, u64)| observed != 0 && observed <= received;
        ensure(
            observations.iter().all(ordered)
                && self.task_received_at_ms <= self.issued_at_ms
                && self.health.matching_sources <= 1024
                && self.health.state != super::health::State::Denied,
        )
    }
    /// This receipt is the one a scoped presence anchor names for the grant.
    fn anchors(&self, binding: &LaunchBinding, grant: Uuid, anchor: &AuthorityAnchor) -> bool {
        self.binding == *binding
            && self.grant_id == grant
            && self.operation_id == anchor.operation_id
            && self.versions.grant_revision == anchor.grant_revision
            && self.expires_at_ms == anchor.expires_at_ms
    }
}
impl Stored {
    pub fn new(body: Body) -> Result<Self, AppError> {
        let result = Self {
            schema_version: 1,
            kind: Kind::Renewed,
            body,
        };
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> Result<(), AppError> {
        self.body.binding.validate().map_err(|_| invalid())?;
        self.body.versions.validate().map_err(|_| invalid())?;
        ensure(self.schema_version == 1)?;
        self.body.validate_identity()?;
        self.body.validate_lifetime()?;
        self.body.validate_observations()?;
        let encoded = serde_json::to_vec(self).map_err(|_| AppError::Internal)?;
        ensure(encoded.len() <= 16384)
    }
    pub fn decode(value: Value) -> Result<Self, AppError> {
        let result: Self = serde_json::from_value(value).map_err(|_| invalid())?;
        result.validate()?;
        Ok(result)
    }
    pub fn matches(&self, request: &Renew) -> bool {
        let b = &self.body;
        b.operation_id == request.operation_id
            && b.binding == request.binding
            && b.grant_id == request.grant_id
            && b.reservation_id == request.reservation_id
            && b.guardian_id == request.guardian_id
            && b.launch_id == request.launch_id
            && b.journal_revision == request.journal_revision
    }
    pub fn deliver(&self, nonce: Uuid, now: u64) -> Result<RenewedAuthority, AppError> {
        self.validate()?;
        let b = &self.body;
        ensure(now >= b.issued_at_ms)?;
        let reply = RenewedAuthority {
            schema_version: 1,
            operation_id: b.operation_id,
            binding: b.binding.clone(),
            grant_id: b.grant_id,
            reservation_id: b.reservation_id,
            guardian_id: b.guardian_id,
            launch_id: b.launch_id,
            journal_revision: b.journal_revision,
            execution_deadline_ms: b.execution_deadline_ms,
            versions: b.versions.clone(),
            delivery: DeliveryStamp {
                nonce,
                clock: DeliveryClock::new(now, b.expires_at_ms).map_err(|_| invalid())?,
            },
        };
        aq_admission::control::ControlReply::Renewed(reply.clone())
            .validate_shape()
            .map_err(|_| invalid())?;
        Ok(reply)
    }
}
pub async fn lookup(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    request: &Renew,
) -> Result<Option<Stored>, AppError> {
    let row=sqlx::query("SELECT kind,request_digest,response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2 FOR SHARE").bind(node).bind(request.operation_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?;
    let Some(row) = row else { return Ok(None) };
    ensure(
        row.get::<String, _>("kind") == "renew"
            && row.get::<String, _>("request_digest") == request_digest(request)?.as_str(),
    )?;
    let stored = Stored::decode(row.get("response"))?;
    ensure(stored.matches(request))?;
    Ok(Some(stored))
}
/// The counter is acknowledged by the server, not a claimed worker journal
/// digest. Closure callers combine it with real physical checkpoint counters.
/// No unknown table/format is interpreted as an empty history.
pub(crate) async fn latest_renewal_revision_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
) -> Result<Option<u64>, AppError> {
    binding.validate().map_err(|_| invalid())?;
    ensure(
        matches!(binding.session,aq_admission::SessionIdentity::Remote{node_id,..}if node_id==node),
    )?;
    // The node PK prefix is bounded by this API's finite node quota. LIMIT257
    // additionally bounds decoding/rows for one attempt even on corrupt data.
    let rows=sqlx::query("SELECT response FROM profiled_execution_receipts WHERE node_id=$1 AND kind='renew' AND (COALESCE(response#>>'{body,binding,attempt_id}',response#>>'{body,grant,binding,attempt_id}')=$2 OR COALESCE(response#>>'{body,binding,attempt_id}',response#>>'{body,grant,binding,attempt_id}') IS NULL) ORDER BY operation_id LIMIT 257 FOR SHARE").bind(node).bind(binding.attempt_id.to_string()).fetch_all(&mut **tx).await.map_err(|_|AppError::Internal)?;
    ensure(rows.len() <= MAX_PER_ATTEMPT)?;
    let mut latest = None;
    for row in rows {
        let stored = Stored::decode(row.get("response"))?;
        ensure(stored.body.binding == *binding)?;
        latest = latest.max(Some(stored.body.journal_revision));
    }
    Ok(latest)
}
struct HistoryUsage {
    global_count: i64,
    node_count: i64,
    attempt_count: i64,
    global_bytes: i64,
    node_bytes: i64,
}
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for HistoryUsage {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        Ok(Self {
            global_count: row.try_get("global_count")?,
            node_count: row.try_get("node_count")?,
            attempt_count: row.try_get("attempt_count")?,
            global_bytes: row.try_get("global_bytes")?,
            node_bytes: row.try_get("node_bytes")?,
        })
    }
}
impl HistoryUsage {
    fn ensure_capacity(&self, incoming: i64) -> Result<(), AppError> {
        let rows_free = self.global_count < MAX_GLOBAL
            && self.node_count < MAX_PER_NODE
            && self.attempt_count < MAX_PER_ATTEMPT as i64;
        let fits = |used: i64, limit: i64| used.checked_add(incoming).is_some_and(|n| n <= limit);
        let bytes_free = fits(self.global_bytes, 67_108_864) && fits(self.node_bytes, 8_388_608);
        if incoming > 32768 || !rows_free || !bytes_free {
            return Err(AppError::UpdatesUnavailable(
                "retained renewal history capacity reached".into(),
            ));
        }
        Ok(())
    }
}
pub async fn reserve_quota(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    stored: &Stored,
) -> Result<Value, AppError> {
    stored.validate()?;
    let value = serde_json::to_value(stored).map_err(|_| AppError::Internal)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-native-renewal-history-v1',0))")
        .execute(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
    let incoming: i64 = sqlx::query_scalar("SELECT (octet_length($1::JSONB::TEXT)+512)::BIGINT")
        .bind(&value)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
    let usage = sqlx::query_as::<_, HistoryUsage>(
        "SELECT count(*) AS global_count,count(*) FILTER(WHERE node_id=$1) AS node_count,count(*) FILTER(WHERE node_id=$1 AND response#>>'{body,binding,attempt_id}'=$2) AS attempt_count,COALESCE(sum(octet_length(response::TEXT)+512),0)::BIGINT AS global_bytes,COALESCE(sum(octet_length(response::TEXT)+512) FILTER(WHERE node_id=$1),0)::BIGINT AS node_bytes FROM profiled_execution_receipts WHERE kind='renew'",
    )
    .bind(node)
    .bind(binding.attempt_id.to_string())
    .fetch_one(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?;
    usage.ensure_capacity(incoming)?;
    Ok(value)
}
pub async fn insert(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    request: &Renew,
    response: Value,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'renew',$3,$4)").bind(node).bind(request.operation_id).bind(request_digest(request)?.as_str()).bind(response).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
    Ok(())
}

/// Actual immutable issuance facts for a retained renewal operation. This is not
/// current authorization; callers hold node/attempt locks and recheck live Hot.
pub(crate) struct Issuance {
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub execution_deadline_ms: u64,
}
pub(crate) async fn issuance_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant: Uuid,
    anchor: &AuthorityAnchor,
) -> Result<Issuance, AppError> {
    anchor.validate().map_err(|_| invalid())?;
    let row=sqlx::query("SELECT kind,response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2 FOR SHARE").bind(node).bind(anchor.operation_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    ensure(row.get::<String, _>("kind") == "renew")?;
    let b = Stored::decode(row.get("response"))?.body;
    ensure(b.anchors(binding, grant, anchor))?;
    Ok(Issuance {
        issued_at_ms: b.issued_at_ms,
        expires_at_ms: b.expires_at_ms,
        execution_deadline_ms: b.execution_deadline_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_admission::{SessionIdentity, transport::WorkerIdentity};
    use serde_json::json;

    fn receipt() -> Stored {
        let binding = LaunchBinding {
            schema_version: 1,
            session: SessionIdentity::Remote {
                node_id: Uuid::from_u128(1),
                boot_id: Uuid::from_u128(2),
                attempt_id: Uuid::from_u128(3),
                assignment_generation: 1,
                session_id: Uuid::from_u128(4),
            },
            offer_id: Uuid::from_u128(5),
            attempt_id: Uuid::from_u128(3),
            assignment_generation: 1,
            execution_spec_digest: Digest::of(b"spec"),
            launch_manifest_digest: Digest::of(b"manifest"),
            graph_access_profile_digest: Digest::of(b"profile"),
        };
        Stored::new(Body {
            operation_id: Uuid::from_u128(6),
            binding,
            grant_id: Uuid::from_u128(7),
            reservation_id: Uuid::from_u128(8),
            guardian_id: Uuid::from_u128(9),
            launch_id: Uuid::from_u128(10),
            journal_revision: 1,
            execution_deadline_ms: 200,
            issued_at_ms: 100,
            expires_at_ms: 200,
            versions: Versions {
                record_revision: 3,
                authority_revision: 2,
                grant_revision: 1,
            },
            health: Evidence {
                state: super::super::health::State::Unknown,
                observations_digest: Digest::of(b"observations"),
                matching_sources: 0,
                evaluated_at_ms: 100,
            },
            registry_revision: 1,
            inventory_revision: 1,
            binding_observation_digest: Digest::of(b"binding"),
            task_observation_digest: Digest::of(b"task"),
            source_observation_digest: Digest::of(b"source"),
            material_observed_at_ms: 100,
            task_observed_at_ms: 90,
            task_received_at_ms: 100,
            task_expires_at_ms: 200,
            source_observed_at_ms: 100,
            source_expires_at_ms: 200,
            binding_observed_at_ms: 100,
            binding_expires_at_ms: 200,
            inventory_expires_at_ms: 200,
        })
        .unwrap()
    }

    #[test]
    fn retained_receipt_round_trip_and_delivery_preserve_issuance() {
        let stored = receipt();
        let encoded = serde_json::to_value(&stored).unwrap();
        let decoded = Stored::decode(encoded.clone()).unwrap();
        let first = decoded.deliver(Uuid::new_v4(), 100).unwrap();
        let replay = decoded.deliver(Uuid::new_v4(), 150).unwrap();
        assert_eq!(
            first.delivery.clock.expires_at_ms,
            replay.delivery.clock.expires_at_ms
        );
        assert_eq!(first.versions, replay.versions);
        assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
        assert!(decoded.deliver(Uuid::new_v4(), 99).is_err());
        assert!(decoded.deliver(Uuid::new_v4(), 200).is_err());
        assert!(decoded.deliver(Uuid::nil(), 150).is_err());
        let mut request = Renew {
            schema_version: 1,
            operation_id: decoded.body.operation_id,
            identity: WorkerIdentity {
                node_id: Uuid::from_u128(1),
                boot_id: Uuid::from_u128(2),
            },
            binding: decoded.body.binding.clone(),
            grant_id: decoded.body.grant_id,
            reservation_id: decoded.body.reservation_id,
            guardian_id: decoded.body.guardian_id,
            launch_id: decoded.body.launch_id,
            journal_revision: decoded.body.journal_revision,
        };
        assert!(decoded.matches(&request));
        request.journal_revision += 1;
        assert!(!decoded.matches(&request));
    }

    #[test]
    fn retained_receipt_refuses_changed_identity_evidence_and_deadline_bounds() {
        let valid = serde_json::to_value(receipt()).unwrap();
        for (pointer, replacement) in [
            ("/schema_version", json!(2)),
            ("/kind", json!("unknown")),
            ("/body/grant_id", json!(Uuid::nil())),
            ("/body/journal_revision", json!(0)),
            ("/body/journal_revision", json!(i64::MAX as u64 + 1)),
            ("/body/versions/grant_revision", json!(0)),
            ("/body/registry_revision", json!(0)),
            ("/body/inventory_revision", json!(0)),
            ("/body/issued_at_ms", json!(0)),
            ("/body/expires_at_ms", json!(100)),
            ("/body/execution_deadline_ms", json!(199)),
            ("/body/task_expires_at_ms", json!(199)),
            ("/body/source_expires_at_ms", json!(199)),
            ("/body/binding_expires_at_ms", json!(199)),
            ("/body/inventory_expires_at_ms", json!(199)),
            ("/body/material_observed_at_ms", json!(0)),
            ("/body/material_observed_at_ms", json!(101)),
            ("/body/task_observed_at_ms", json!(101)),
            ("/body/task_received_at_ms", json!(101)),
            ("/body/source_observed_at_ms", json!(101)),
            ("/body/binding_observed_at_ms", json!(101)),
            ("/body/health/evaluated_at_ms", json!(101)),
            ("/body/health/matching_sources", json!(1025)),
            ("/body/health/state", json!("denied")),
        ] {
            let mut changed = valid.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert!(Stored::decode(changed).is_err(), "{pointer}");
        }
        let mut changed = valid;
        changed["body"]["extra"] = json!(true);
        assert!(Stored::decode(changed).is_err());
    }

    #[test]
    fn history_capacity_keeps_count_byte_and_overflow_limits() {
        let empty = || HistoryUsage {
            global_count: 0,
            node_count: 0,
            attempt_count: 0,
            global_bytes: 0,
            node_bytes: 0,
        };
        assert!(empty().ensure_capacity(32768).is_ok());
        assert!(empty().ensure_capacity(32769).is_err());
        let full = |change: fn(&mut HistoryUsage)| {
            let mut usage = empty();
            change(&mut usage);
            usage
        };
        for usage in [
            full(|u| u.global_count = MAX_GLOBAL),
            full(|u| u.node_count = MAX_PER_NODE),
            full(|u| u.attempt_count = MAX_PER_ATTEMPT as i64),
            full(|u| u.global_bytes = 67_108_864),
            full(|u| u.node_bytes = 8_388_608),
            full(|u| u.global_bytes = i64::MAX),
            full(|u| u.node_bytes = i64::MAX),
        ] {
            assert!(usage.ensure_capacity(1).is_err());
        }
        let at_boundary = HistoryUsage {
            global_count: MAX_GLOBAL - 1,
            node_count: MAX_PER_NODE - 1,
            attempt_count: MAX_PER_ATTEMPT as i64 - 1,
            global_bytes: 67_108_864 - 1,
            node_bytes: 8_388_608 - 1,
        };
        assert!(at_boundary.ensure_capacity(1).is_ok());
    }
}
