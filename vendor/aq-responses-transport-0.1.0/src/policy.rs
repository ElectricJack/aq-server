use super::*;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use zeroize::Zeroize;
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_NODES: usize = 65536;
const MAX_DEPTH: usize = 32;
/// Exact immutable attempt selection. Construction validates a byte policy;
/// it supplies no execution Grant, upstream quota or aggregate budget authority.
pub struct ResponsesPolicy {
    owner_scope: Digest,
    scope_digest: Digest,
    model: String,
    reasoning: String,
    cap: u64,
    deadline_ns: u64,
}
impl ResponsesPolicy {
    pub fn new(owner_scope: Digest, model: &ModelSettings, deadline_ns: u64) -> Result<Self> {
        model.provider.validate()?;
        model.model.validate()?;
        model.reasoning.validate()?;
        ensure!(
            model.provider.as_str() == "openai"
                && model.spending_budget.is_none()
                && model.max_output_tokens >= 16
                && model.max_output_tokens <= 1048576
                && matches!(model.reasoning.as_str(), "low" | "medium" | "high"),
            "unsupported Responses selection"
        );
        let mut recipe = serde_json::json!({"contract":"aq.responses-policy-1","owner_scope":owner_scope,"model":model,"original_deadline_boot_ns":deadline_ns});
        recipe.sort_all_objects();
        let scope_digest = Digest::of(&serde_json::to_vec(&recipe)?);
        let scope = Self {
            owner_scope,
            scope_digest,
            model: model.model.as_str().into(),
            reasoning: model.reasoning.as_str().into(),
            cap: model.max_output_tokens,
            deadline_ns,
        };
        scope.remaining()?;
        Ok(scope)
    }
    pub fn owner_scope(&self) -> &Digest {
        &self.owner_scope
    }
    pub fn scope_digest(&self) -> &Digest {
        &self.scope_digest
    }
    pub fn original_deadline_boot_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub(super) fn remaining(&self) -> Result<Duration> {
        let now = boot_ns()?;
        ensure!(now < self.deadline_ns, "scoped Responses authority expired");
        Ok(Duration::from_nanos(self.deadline_ns - now))
    }
    /// Preserves every accepted JSON setting except adding the selected cap.
    /// Duplicate fields, nonstreaming/stateful routes and changed selection
    /// refuse before any upstream credential-bearing request.
    pub fn prepare(&self, body: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        self.remaining()?;
        ensure!(
            !body.is_empty() && body.len() <= MAX_REQUEST_BYTES,
            "Responses request byte bound"
        );
        let mut deserializer = serde_json::Deserializer::from_slice(body);
        let mut nodes = 0;
        let mut root = Erased(
            Seed {
                nodes: &mut nodes,
                depth: 0,
            }
            .deserialize(&mut deserializer)?,
        );
        deserializer.end()?;
        let object = root
            .0
            .as_object_mut()
            .context("Responses body must be object")?;
        for name in object.keys() {
            ensure!(
                matches!(
                    name.as_str(),
                    "model"
                        | "stream"
                        | "service_tier"
                        | "input"
                        | "tools"
                        | "tool_choice"
                        | "parallel_tool_calls"
                        | "reasoning"
                        | "store"
                        | "stream_options"
                        | "include"
                        | "prompt_cache_key"
                        | "text"
                        | "client_metadata"
                        | "access_programs"
                        | "max_output_tokens"
                ),
                "unsupported Responses request setting"
            );
        }
        ensure!(
            object.get("model").and_then(Value::as_str) == Some(self.model.as_str())
                && object.get("stream") == Some(&Value::Bool(true))
                && object.get("store") == Some(&Value::Bool(false))
                && object.get("input").is_some_and(Value::is_array),
            "Responses request selection/stream/storage mismatch"
        );
        ensure!(
            object
                .get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(Value::as_str)
                == Some(self.reasoning.as_str()),
            "Responses reasoning selection mismatch"
        );
        ensure!(
            object.get("access_programs").is_none_or(Value::is_null),
            "unqualified Responses access program"
        );
        super::items::validate(object)?;
        if let Some(existing) = object.get("max_output_tokens") {
            ensure!(
                existing.as_u64() == Some(self.cap),
                "Responses conflicting output cap"
            );
        }
        object.insert("max_output_tokens".into(), Value::from(self.cap));
        let bytes = Zeroizing::new(serde_json::to_vec(&root.0)?);
        ensure!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "Responses injected request bound"
        );
        Ok(bytes)
    }
}
struct Erased(Value);
impl Drop for Erased {
    fn drop(&mut self) {
        erase(&mut self.0);
    }
}
fn erase(value: &mut Value) {
    match value {
        Value::String(s) => s.zeroize(),
        Value::Array(a) => a.iter_mut().for_each(erase),
        Value::Object(o) => {
            for (mut k, mut v) in std::mem::take(o) {
                k.zeroize();
                erase(&mut v);
            }
        }
        _ => {}
    }
}
struct Seed<'a> {
    nodes: &'a mut usize,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Value, D::Error> {
        *self.nodes += 1;
        if *self.nodes > MAX_NODES || self.depth > MAX_DEPTH {
            return Err(serde::de::Error::custom("Responses JSON structural bound"));
        }
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded unique-field JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("nonfinite JSON"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Seed {
            nodes: self.nodes,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut object = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate Responses JSON field"));
            }
            let value = map.next_value_seed(Seed {
                nodes: self.nodes,
                depth: self.depth + 1,
            })?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    pub fn scope() -> ResponsesPolicy {
        let model: ModelSettings = serde_json::from_value(serde_json::json!({"provider":"openai","model":"offline-model","reasoning":"medium","max_output_tokens":100,"spending_budget":null})).unwrap();
        ResponsesPolicy::new(
            Digest::of(b"opaque fixture owner"),
            &model,
            boot_ns().unwrap() + 10_000_000_000,
        )
        .unwrap()
    }
    pub fn body() -> Vec<u8> {
        br#"{"model":"offline-model","stream":true,"store":false,"input":[{"type":"message","role":"user","content":"literal startup"}],"reasoning":{"effort":"medium","summary":"auto"},"tools":[],"tool_choice":"auto","parallel_tool_calls":true,"text":{"verbosity":"low"},"include":[],"client_metadata":{"a":"b"}}"#.to_vec()
    }
    #[test]
    fn injects_exact_response_cap_preserving_every_other_setting() {
        let scope = scope();
        assert_eq!(
            scope.prepare(&body()).unwrap().as_slice(),
            include_bytes!("goldens/capped-request.json"),
            "pre-extraction accepted request bytes must stay exact"
        );
        let original: Value = serde_json::from_slice(&body()).unwrap();
        let mut got: Value = serde_json::from_slice(&scope.prepare(&body()).unwrap()).unwrap();
        assert_eq!(
            got.as_object_mut().unwrap().remove("max_output_tokens"),
            Some(Value::from(100))
        );
        assert_eq!(got, original);
    }
    #[test]
    fn changed_selection_ambiguous_json_stateful_and_unbounded_requests_refuse() {
        let scope = scope();
        for (field, value) in [
            ("model", Value::from("other-model")),
            ("stream", Value::from(false)),
            ("store", Value::from(true)),
            ("max_output_tokens", Value::from(101)),
            ("max_output_tokens", Value::from(99)),
            ("previous_response_id", Value::from("resp_foreign")),
            ("background", Value::from(true)),
            ("access_programs", serde_json::json!({"cyber":"standard"})),
            ("reasoning", serde_json::json!({"effort":"high"})),
        ] {
            let mut body: Value = serde_json::from_slice(&body()).unwrap();
            body[field] = value;
            assert!(
                scope.prepare(&serde_json::to_vec(&body).unwrap()).is_err(),
                "{field}"
            );
        }
        for input in [
            br#"{"model":"a","model":"b"}"#.as_slice(),
            br#"{"reasoning":{"effort":"medium","effort":"high"}}"#.as_slice(),
            br#"[]"#.as_slice(),
            br#"{} trailing"#.as_slice(),
        ] {
            assert!(scope.prepare(input).is_err());
        }
        assert!(scope.prepare(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
        let depth = format!("{}0{}", "[".repeat(40), "]".repeat(40));
        assert!(scope.prepare(depth.as_bytes()).is_err());
        let nodes = format!("[{}]", vec!["0"; MAX_NODES + 1].join(","));
        assert!(scope.prepare(nodes.as_bytes()).is_err());
    }
    #[test]
    fn expired_scope_and_identical_cap_refuse_or_remain_exact() {
        let mut scope = scope();
        let mut body: Value = serde_json::from_slice(&body()).unwrap();
        body["max_output_tokens"] = 100.into();
        assert_eq!(
            serde_json::from_slice::<Value>(
                &scope.prepare(&serde_json::to_vec(&body).unwrap()).unwrap()
            )
            .unwrap(),
            body
        );
        scope.deadline_ns = 0;
        assert!(scope.prepare(&self::body()).is_err());
    }
    #[test]
    fn server_side_references_and_hosted_tools_refuse_before_cap_insertion() {
        let scope = scope();
        for item in [
            serde_json::json!({"id":"item_foreign"}),
            serde_json::json!({"type":"item_reference","id":"item_foreign"}),
            serde_json::json!({"type":"message","role":"user","content":[{"type":"input_file","file_id":"file_foreign"}]}),
            serde_json::json!({"type":"message","role":"user","content":[{"type":"input_image","image_url":"https://foreign.example/image"}]}),
            serde_json::json!({"type":"additional_tools","role":"developer","tools":[{"type":"web_search"}]}),
        ] {
            let mut value: Value = serde_json::from_slice(&body()).unwrap();
            value["input"] = serde_json::json!([item]);
            assert!(scope.prepare(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        for tool in [
            "web_search",
            "file_search",
            "mcp",
            "computer_use_preview",
            "code_interpreter",
        ] {
            let mut value: Value = serde_json::from_slice(&body()).unwrap();
            value["tools"] = serde_json::json!([{"type":tool,"name":"foreign"}]);
            assert!(scope.prepare(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }
}
