//! Context qualification renewal (spec 0007 §7.3). A service node re-runs the
//! three bounded measurement calls of a configured qualification's counting
//! evidence and submits the Claude `result` events. This crate checks them
//! exactly as `scripts/ms1-profiled.py` `measured()` does and renews the fact's
//! 24 h window; it also merges stored renewals with the configured facts. Pure:
//! no database, no clock.
use aq_protocol::MeasurementSubmission;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The qualification TTL cap (`context_budget.rs`), and a renewal's window.
pub const TTL_MS: u64 = 86_400_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenewalError {
    #[error("call set")]
    CallSet,
    #[error("incomplete call {0}")]
    Incomplete(String),
    #[error("model {0}")]
    Model(String),
    #[error("context window")]
    Window,
    #[error("bound {0}")]
    Bound(String),
    #[error("prompt {0}")]
    Prompt(String),
    #[error("base fact shape")]
    BaseShape,
}
use RenewalError::*;

fn number(v: &Value, key: &str) -> Result<u64, RenewalError> {
    v[key].as_u64().ok_or(BaseShape)
}

/// `base`: a Qualification as JSON. `counting_body`: the parsed body of
/// `base.counting_evidence` (`{"measurements":[{"call","prompt_utf8_bytes",
/// "prompt_sha256",...}], ...}`). Returns the base fact with `observed_at_ms =
/// effective_at_ms` and `expires_at_ms = effective_at_ms + TTL_MS`.
///
/// Every call must succeed, report exactly the base model with the base's
/// context window, and keep its total input (input + cache creation + cache
/// read) within the counting bound `prompt_utf8_bytes ×
/// maximum_tokens_per_utf8_byte + fixed_overhead_tokens`: the bound offers are
/// budgeted with, which `measured()` checks with its `TOKENS_PER_BYTE` and
/// `FIXED_OVERHEAD_TOKENS`.
pub fn renew(
    base: &Value,
    counting_body: &Value,
    submission: &MeasurementSubmission,
    effective_at_ms: u64,
) -> Result<Value, RenewalError> {
    let model = base["subject"]["model"].as_str().ok_or(BaseShape)?;
    let window = number(base, "capacity_tokens")?;
    let per_byte = number(base, "maximum_tokens_per_utf8_byte")?;
    let overhead = number(base, "fixed_overhead_tokens")?;
    // The counting evidence was measured against the same bound, when it says so.
    for (key, value) in [
        ("maximum_tokens_per_utf8_byte", per_byte),
        ("fixed_overhead_tokens", overhead),
    ] {
        if counting_body
            .get(key)
            .is_some_and(|v| v.as_u64() != Some(value))
        {
            return Err(BaseShape);
        }
    }
    let mut expected = BTreeMap::new();
    for row in counting_body["measurements"].as_array().ok_or(BaseShape)? {
        let call = row["call"].as_str().ok_or(BaseShape)?;
        let prompt = (
            number(row, "prompt_utf8_bytes")?,
            row["prompt_sha256"].as_str().ok_or(BaseShape)?,
        );
        if expected.insert(call, prompt).is_some() {
            return Err(BaseShape);
        }
    }
    if expected.is_empty() || !base.is_object() {
        return Err(BaseShape);
    }
    let calls: BTreeSet<&str> = submission.calls.iter().map(|c| c.call.as_str()).collect();
    if calls.len() != submission.calls.len() || !calls.iter().eq(expected.keys()) {
        return Err(CallSet);
    }
    for c in &submission.calls {
        let name = || c.call.clone();
        let (bytes, sha) = expected[c.call.as_str()];
        if c.prompt_utf8_bytes != bytes || c.prompt_sha256 != sha {
            return Err(Prompt(name()));
        }
        let r = &c.result;
        if r["type"] != "result" || r["subtype"] != "success" || r["is_error"] != false {
            return Err(Incomplete(name()));
        }
        let total = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .try_fold(0u64, |n, k| n.checked_add(r["usage"][k].as_u64()?))
        .ok_or_else(|| Incomplete(name()))?;
        let models = r["modelUsage"]
            .as_object()
            .ok_or_else(|| Incomplete(name()))?;
        if models.len() != 1 || !models.contains_key(model) {
            let key = models.keys().find(|k| *k != model);
            return Err(Model(key.cloned().unwrap_or_default()));
        }
        if models[model]["contextWindow"].as_u64() != Some(window) {
            return Err(Window);
        }
        let bound = bytes
            .checked_mul(per_byte)
            .and_then(|b| b.checked_add(overhead));
        if bound.is_none_or(|b| total > b) {
            return Err(Bound(name()));
        }
    }
    let mut fact = base.clone();
    fact["observed_at_ms"] = effective_at_ms.into();
    fact["expires_at_ms"] = effective_at_ms.checked_add(TTL_MS).ok_or(BaseShape)?.into();
    Ok(fact)
}

/// Renews every base (`(Qualification JSON, counting evidence body text)`) the
/// submission satisfies, with its index. When none renews: the first refusal,
/// or `None` without bases. A body that is not JSON is `BaseShape`.
pub fn renew_all(
    bases: &[(Value, Option<&str>)],
    submission: &MeasurementSubmission,
    effective_at_ms: u64,
) -> Result<Vec<(usize, Value)>, Option<RenewalError>> {
    let mut refusal = None;
    let mut renewed = vec![];
    for (i, (base, body)) in bases.iter().enumerate() {
        let body = body.and_then(|b| serde_json::from_str::<Value>(b).ok());
        match body
            .ok_or(BaseShape)
            .and_then(|b| renew(base, &b, submission, effective_at_ms))
        {
            Ok(fact) => renewed.push((i, fact)),
            Err(e) => {
                refusal.get_or_insert(e);
            }
        }
    }
    if renewed.is_empty() {
        Err(refusal)
    } else {
        Ok(renewed)
    }
}

/// `observed <= now < expires`.
pub fn effective((observed, expires): (u64, u64), now_ms: u64) -> bool {
    observed <= now_ms && now_ms < expires
}

/// One fact per configured fact, in configured order: the newest of the
/// configured fact and its renewals (`(index of the configured base, fact)`)
/// that is effective at `now_ms`, else the configured fact itself. Subjects stay
/// unique, so the result is a valid qualification catalog.
pub fn merged<T: Clone>(
    configured: &[T],
    renewals: &[(usize, T)],
    window: impl Fn(&T) -> (u64, u64),
    now_ms: u64,
) -> Vec<T> {
    configured
        .iter()
        .enumerate()
        .map(|(i, base)| {
            renewals
                .iter()
                .filter(|(b, _)| *b == i)
                .map(|(_, f)| f)
                .chain([base])
                .filter(|f| effective(window(f), now_ms))
                .max_by_key(|f| window(f).0)
                .unwrap_or(base)
                .clone()
        })
        .collect()
}

/// The newest renewal that is not yet effective at `now_ms`, if any.
pub fn pending<T>(
    renewals: &[(usize, T)],
    window: impl Fn(&T) -> (u64, u64),
    now_ms: u64,
) -> Option<&T> {
    renewals
        .iter()
        .map(|(_, f)| f)
        .filter(|f| window(f).0 > now_ms)
        .max_by_key(|f| window(f).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_protocol::MeasuredCall;
    use serde_json::json;

    // The constants of scripts/ms1-profiled.py.
    const TOKENS_PER_BYTE: u64 = 1;
    const FIXED_OVERHEAD_TOKENS: u64 = 32768;
    const MODEL: &str = "claude-sonnet-5-5";
    const WINDOW: u64 = 200_000;
    const CALLS: [(&str, u64); 3] = [("minimal", 67), ("mixed", 2738), ("adversarial", 5844)];

    fn sha(call: &str) -> String {
        format!("{:0>64}", call.len())
    }
    fn base() -> Value {
        json!({"schema_version":1,"subject":{"model":MODEL},"maximum_tokens_per_utf8_byte":TOKENS_PER_BYTE,
            "fixed_overhead_tokens":FIXED_OVERHEAD_TOKENS,"capacity_tokens":WINDOW,
            "observed_at_ms":1000,"expires_at_ms":2000})
    }
    fn counting() -> Value {
        let rows: Vec<Value> = CALLS
            .iter()
            .map(|(c, b)| json!({"call":c,"prompt_utf8_bytes":b,"prompt_sha256":sha(c),"total_input_tokens":1}))
            .collect();
        json!({"schema_version":1,"maximum_tokens_per_utf8_byte":TOKENS_PER_BYTE,
            "fixed_overhead_tokens":FIXED_OVERHEAD_TOKENS,"measurements":rows})
    }
    /// The shape of a `result` event in ~/.local/share/aq-ms1/measure/*.jsonl.
    fn result(input: u64, creation: u64, read: u64) -> Value {
        json!({"type":"result","subtype":"success","is_error":false,"session_id":"s",
            "usage":{"input_tokens":input,"cache_creation_input_tokens":creation,"cache_read_input_tokens":read,"output_tokens":4},
            "modelUsage":{MODEL:{"inputTokens":input,"outputTokens":4,"contextWindow":WINDOW}}})
    }
    fn submission() -> MeasurementSubmission {
        MeasurementSubmission {
            schema_version: 1,
            calls: CALLS
                .iter()
                .map(|(c, b)| MeasuredCall {
                    call: (*c).into(),
                    prompt_utf8_bytes: *b,
                    prompt_sha256: sha(c),
                    result: result(2, 2025, 16800),
                })
                .collect(),
        }
    }
    fn check(s: &MeasurementSubmission) -> Result<Value, RenewalError> {
        renew(&base(), &counting(), s, 5_000)
    }

    #[test]
    fn valid_submission_renews_with_new_window() {
        let fact = check(&submission()).unwrap();
        assert_eq!(fact["observed_at_ms"], 5_000);
        assert_eq!(fact["expires_at_ms"], 5_000 + TTL_MS);
        let mut rest = fact.clone();
        rest["observed_at_ms"] = 1000.into();
        rest["expires_at_ms"] = 2000.into();
        assert_eq!(rest, base(), "only the window changes");
    }

    #[test]
    fn missing_or_duplicate_call_is_call_set() {
        let mut s = submission();
        s.calls.pop();
        assert_eq!(check(&s), Err(CallSet));
        let mut s = submission();
        s.calls[2] = s.calls[0].clone();
        assert_eq!(check(&s), Err(CallSet));
        let mut s = submission();
        s.calls.push(s.calls[0].clone());
        assert_eq!(check(&s), Err(CallSet));
    }

    #[test]
    fn changed_prompt_is_prompt_error() {
        let mut s = submission();
        s.calls[1].prompt_sha256 = sha("other prompt");
        assert_eq!(check(&s), Err(Prompt("mixed".into())));
        let mut s = submission();
        s.calls[0].prompt_utf8_bytes += 1;
        assert_eq!(check(&s), Err(Prompt("minimal".into())));
    }

    #[test]
    fn other_model_or_window_refused() {
        let mut s = submission();
        s.calls[0].result["modelUsage"] = json!({"claude-opus-5-5":{"contextWindow":WINDOW}});
        assert_eq!(check(&s), Err(Model("claude-opus-5-5".into())));
        let mut s = submission();
        s.calls[0].result["modelUsage"]["claude-haiku-5"] = json!({"contextWindow":WINDOW});
        assert_eq!(check(&s), Err(Model("claude-haiku-5".into())));
        let mut s = submission();
        s.calls[2].result["modelUsage"][MODEL]["contextWindow"] = json!(1_000_000);
        assert_eq!(check(&s), Err(Window));
    }

    /// The server enforces the counting bound itself (W10 review): exactly
    /// `prompt bytes × TOKENS_PER_BYTE + FIXED_OVERHEAD_TOKENS`, one over refused.
    #[test]
    fn over_bound_refused() {
        let bound = 5844 * TOKENS_PER_BYTE + FIXED_OVERHEAD_TOKENS;
        let mut s = submission();
        s.calls[2].result = result(2, bound - 2 - 17482, 17482);
        assert!(check(&s).is_ok(), "exactly the bound is accepted");
        s.calls[2].result = result(2, bound - 1 - 17482, 17482);
        assert_eq!(check(&s), Err(Bound("adversarial".into())));
        // Cache reads count: a window-wide read alone exceeds the bound.
        let mut s = submission();
        s.calls[0].result = result(0, 0, 67 + FIXED_OVERHEAD_TOKENS + 1);
        assert_eq!(check(&s), Err(Bound("minimal".into())));
        // Counting evidence stating another bound than the fact is refused.
        let mut other = counting();
        other["fixed_overhead_tokens"] = json!(65536);
        assert_eq!(renew(&base(), &other, &submission(), 5_000), Err(BaseShape));
    }

    #[test]
    fn error_result_is_incomplete() {
        for (key, value) in [
            ("is_error", json!(true)),
            ("subtype", json!("error_max_turns")),
            ("type", json!("assistant")),
        ] {
            let mut s = submission();
            s.calls[1].result[key] = value;
            assert_eq!(check(&s), Err(Incomplete("mixed".into())), "{key}");
        }
        let mut s = submission();
        s.calls[1].result["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_read_input_tokens");
        assert_eq!(check(&s), Err(Incomplete("mixed".into())));
    }

    #[test]
    fn renew_all_keeps_successes_or_reports_the_first_refusal() {
        let mut opus = base();
        opus["subject"]["model"] = json!("claude-opus-5-5");
        let body = counting().to_string();
        let bases = [
            (opus.clone(), Some(body.as_str())),
            (base(), Some(body.as_str())),
        ];
        let renewed = renew_all(&bases, &submission(), 5_000).unwrap();
        assert_eq!(renewed.len(), 1);
        assert_eq!(renewed[0].0, 1);
        let refused = renew_all(&bases[..1], &submission(), 5_000);
        assert_eq!(refused, Err(Some(Model(MODEL.into()))));
        assert_eq!(
            renew_all(&[(base(), Some("not json"))], &submission(), 1),
            Err(Some(BaseShape))
        );
        assert_eq!(renew_all(&[], &submission(), 1), Err(None));
    }

    #[test]
    fn merged_prefers_the_newest_effective_fact_and_keeps_order() {
        let w = |f: &(u64, u64)| *f;
        let configured = [(10, 20), (10, 100)];
        // Base 0 expired at 50; its renewal (40, 140) is effective; a pending one (60, 160) is not yet.
        let renewals = [(0, (40, 140)), (0, (60, 160)), (1, (55, 155))];
        assert_eq!(
            merged(&configured, &renewals, w, 50),
            vec![(40, 140), (10, 100)]
        );
        assert_eq!(
            merged(&configured, &renewals, w, 60),
            vec![(60, 160), (55, 155)]
        );
        assert_eq!(pending(&renewals, w, 50), Some(&(60, 160)));
        assert_eq!(pending(&renewals, w, 60), None);
        // Nothing effective: the configured fact (expired) stays, so evaluate refuses it.
        assert_eq!(merged(&configured, &[], w, 500), configured.to_vec());
    }
}
