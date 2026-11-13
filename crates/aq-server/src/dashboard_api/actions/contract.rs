pub use aq_dashboard_contract::action_contract::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `ActionRequest` bounds as an `ApiError`.
pub(crate) trait Validate {
    fn validate(&self) -> Result<(), super::ApiError> {
        self.validate_with_limit(65536)
    }
    fn validate_with_limit(&self, max_bytes: usize) -> Result<(), super::ApiError>;
}
impl Validate for ActionRequest {
    fn validate_with_limit(&self, max_bytes: usize) -> Result<(), super::ApiError> {
        self.check(max_bytes).map_err(|e| match e {
            RequestError::Invalid => super::ApiError::invalid(),
            RequestError::TooLarge => {
                super::ApiError::for_status(super::StatusCode::PAYLOAD_TOO_LARGE)
            }
        })
    }
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
enum Wire {
    Request(ActionRequest),
    Operation(Box<OperationResponse>),
    Error(super::super::errors::ErrorEnvelope),
}
pub fn schema(registry: &super::Registry) -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(Wire)).expect("static schema");
    let request = &mut schema["$defs"]["ActionRequest"];
    request["properties"]["expected_versions"]["maxProperties"] = serde_json::json!(32);
    request["properties"]["expected_versions"]["propertyNames"] =
        serde_json::json!({"minLength":1,"maxLength":128});
    request["properties"]["expected_versions"]["additionalProperties"] =
        serde_json::json!({"type":"string","pattern":"^[1-9][0-9]{0,39}$"});
    if registry.schemas().is_empty() {
        request["not"] = serde_json::json!({});
    } else {
        request["oneOf"]=Value::Array(registry.schemas().iter().map(|(kind,payload)| {
            let mut payload=payload.clone();
            payload["$id"]=Value::String(format!("urn:aq:dashboard-actions:1:{kind}:payload"));
            serde_json::json!({"properties":{"kind":{"const":kind},"payload":payload},"required":["kind","payload"]})
        }).collect());
    }
    // R1 check.py: a resolver that ignores the payloads' `$id` resolves their
    // `#/$defs/...` refs at the root, so their definitions are also listed there.
    let mut hoisted = serde_json::Map::new();
    for payload in registry.schemas().values() {
        if let Some(Value::Object(defs)) = payload.get("$defs") {
            hoisted.extend(defs.clone());
        }
    }
    let root = schema["$defs"].as_object_mut().expect("root definitions");
    for (name, definition) in hoisted {
        root.entry(name).or_insert(definition);
    }
    schema["x-capability-version"] = serde_json::json!(1);
    schema
}
pub fn schema_bytes() -> Vec<u8> {
    let mut bytes =
        serde_json::to_vec_pretty(&schema(&super::existing::schemas())).expect("static schema");
    bytes.push(b'\n');
    bytes
}
