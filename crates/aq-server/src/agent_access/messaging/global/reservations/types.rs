use super::*;
use schemars::JsonSchema;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    GlobalSupervisorReservation,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalPolicy {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    pub kind: PolicyKind,
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub enabled: bool,
}
impl LocalPolicy {
    pub(super) fn validate(&self) -> Result<()> {
        if self.schema_version != 1 || self.actor_id.is_nil() || self.conversation_id.is_nil() {
            return Err(invalid());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Configure {
    pub operation_id: Uuid,
    pub actor_id: Uuid,
    pub expected_actor_revision: u64,
    pub policy: LocalPolicy,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reserve {
    pub operation_id: Uuid,
    pub reservation_id: Uuid,
    pub actor_id: Uuid,
    pub expected_actor_revision: u64,
    pub expected_policy_revision: u64,
    pub scan_after: u64,
    pub through_cursor: u64,
    pub selection_digest: Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    pub operation_id: Uuid,
    pub actor_id: Uuid,
    pub reservation_id: Uuid,
    pub expected_reservation_revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub actor_id: Uuid,
    pub expected_actor_revision: u64,
    pub scan_after: u64,
    #[serde(default)]
    pub through_cursor: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewStatus {
    Ready,
    NoEligibleMail,
    PartialScan,
    ReservationOutstanding,
    Disabled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub actor_revision: u64,
    pub policy_revision: u64,
    pub scan_after: u64,
    pub through_cursor: u64,
    pub scanned_through_cursor: u64,
    pub batch: Option<MailBatch>,
    pub body_bytes: u64,
    pub coverage: Coverage,
    pub has_more: bool,
    pub status: PreviewStatus,
    pub selection_digest: Digest,
    pub execution_history: Unavailable,
    pub history_scope: ReservationOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    Unavailable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReservationOnly {
    ReservationOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    Consumed,
    AwaitingPreparedPlan,
    CancelledBeforePlan,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReservationView {
    pub id: Uuid,
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub revision: u64,
    pub admission_actor_revision: u64,
    pub reserve_operation_id: Uuid,
    pub cancel_operation_id: Option<Uuid>,
    pub policy_revision: u64,
    pub batch: MailBatch,
    pub batch_digest: Digest,
    pub body_bytes: u64,
    pub state: ReservationState,
    pub runtime: Unavailable,
    pub prepared_plan: (),
    pub session: (),
    pub execution_history: Unavailable,
    pub history_scope: ReservationOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Configured,
    NotConfigured,
    Reserved,
    NotReserved,
    CancelledBeforePlan,
    NotCancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    RevisionChanged,
    PolicyDisabled,
    ReservationOutstanding,
    SelectionChanged,
    NoEligibleMail,
    ReservationNotCurrent,
    ReservationIdConflict,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub contract: String,
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub operation_id: Uuid,
    pub initiating_user_id: Uuid,
    pub request_digest: Digest,
    pub actor_revision: u64,
    pub policy_revision: u64,
    pub disposition: Disposition,
    pub reason: Option<Reason>,
    pub reservation: Option<ReservationView>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActorView {
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub revision: u64,
    pub policy_revision: u64,
    pub policy: LocalPolicy,
    pub policy_digest: Digest,
    pub retained_operations: u64,
    pub retained_bytes: u64,
    pub current_reservation: Option<ReservationView>,
    pub execution_history: Unavailable,
    pub history_scope: ReservationOnly,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Lookup {
    pub receipt: Receipt,
    pub current_reservation: Option<ReservationView>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub actor_id: Uuid,
    #[schemars(length(max = 16))]
    pub items: Vec<ReservationView>,
    pub next_before: Option<u64>,
    pub has_more: bool,
    pub execution_history: Unavailable,
    pub history_scope: ReservationOnly,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "document",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Document {
    Policy(LocalPolicy),
    Configure(Configure),
    Reserve(Reserve),
    Cancel(Cancel),
    PreviewRequest(PreviewRequest),
    Preview(Preview),
    Receipt(Receipt),
    Actor(ActorView),
    Lookup(Lookup),
    History(History),
}
pub fn schema() -> Value {
    let mut s =
        serde_json::to_value(schemars::schema_for!(Document)).expect("static reservation schema");
    fn narrow(value: &mut Value) {
        if let Some(object) = value.as_object_mut() {
            if object.get("type").is_some_and(|v| v == "integer")
                && object.get("format").is_some_and(|v| v == "uint64")
            {
                object.insert("maximum".into(), json!(i64::MAX));
            }
            if object.get("format").is_some_and(|v| v == "uuid") {
                object.insert("not".into(), json!({"const": Uuid::nil()}));
            }
            for value in object.values_mut() {
                narrow(value);
            }
        } else if let Some(array) = value.as_array_mut() {
            for value in array {
                narrow(value);
            }
        }
    }
    narrow(&mut s);
    s["$id"] = json!("urn:agent-q:central-reservation:1");
    s["x-execution-authority"] = json!(false);
    s["x-required-validation"] = json!(
        "Current Owner, source, cross-field and encoded byte-budget validation by the owning service"
    );
    s["$defs"]["Receipt"]["properties"]["contract"] = json!({"type":"string","const":CONTRACT});
    s
}
