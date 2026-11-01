//! Duplicate-free graph JSON. Graph metadata permits finite fractional numbers;
//! the stricter integer-only agent envelope parser is a different boundary.
use serde::Deserialize;
use serde_json::Value;
pub(in crate::agent_access) fn parse(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    #[derive(Debug)]
    struct GraphValue(Value);
    impl<'de> Deserialize<'de> for GraphValue {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = GraphValue;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("duplicate-free graph JSON")
                }
                fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<GraphValue, E> {
                    Ok(GraphValue(v.into()))
                }
                fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<GraphValue, E> {
                    Ok(GraphValue(v.into()))
                }
                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<GraphValue, E> {
                    Ok(GraphValue(v.into()))
                }
                fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<GraphValue, E> {
                    serde_json::Number::from_f64(v)
                        .map(|v| GraphValue(Value::Number(v)))
                        .ok_or_else(|| E::custom("nonfinite number"))
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<GraphValue, E> {
                    Ok(GraphValue(v.into()))
                }
                fn visit_string<E: serde::de::Error>(self, v: String) -> Result<GraphValue, E> {
                    Ok(GraphValue(v.into()))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<GraphValue, E> {
                    Ok(GraphValue(Value::Null))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<GraphValue, A::Error> {
                    let mut items = Vec::new();
                    while let Some(GraphValue(value)) = seq.next_element()? {
                        items.push(value);
                    }
                    Ok(GraphValue(Value::Array(items)))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<GraphValue, A::Error> {
                    let mut result = serde_json::Map::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if result.contains_key(&key) {
                            return Err(serde::de::Error::custom("duplicate JSON field"));
                        }
                        let GraphValue(value) = map.next_value()?;
                        result.insert(key, value);
                    }
                    Ok(GraphValue(Value::Object(result)))
                }
            }
            d.deserialize_any(Visitor)
        }
    }
    serde_json::from_slice::<GraphValue>(bytes).map(|v| v.0)
}
