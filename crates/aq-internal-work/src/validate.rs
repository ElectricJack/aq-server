//! The prompt a model is given and the check of what it answers (spec 0012 §3).
use crate::{AnswerShape, Request};
use serde_json::{Value, json};

/// Most bytes of model text an error string may carry.
const CLIP_BYTES: usize = 200;

/// Instructions, then input, then the answer contract.
pub fn prompt(request: &Request) -> String {
    let contract = match &request.answer {
        AnswerShape::Choice { choices, .. } => format!(
            "Answer with one JSON object and nothing else: {{\"choice\": one of {}, \"rationale\": string}}",
            Value::from(choices.clone())
        ),
        AnswerShape::Json { schema } => format!(
            "Answer with one JSON object and nothing else, matching this JSON Schema:\n{schema}"
        ),
    };
    format!(
        "{}\n\n{}\n\n{contract}",
        request.instructions, request.input
    )
}

/// Checks the model's raw text against the shape and returns the answer.
///
/// The error names the first violation; it carries at most 200 bytes of the
/// model's text, because it is stored in a database row.
pub fn answer(shape: &AnswerShape, raw: &str, max_output_bytes: u64) -> Result<Value, String> {
    if raw.len() as u64 > max_output_bytes {
        return Err(format!(
            "output is {} bytes, over the limit of {max_output_bytes}",
            raw.len()
        ));
    }
    let value: Value = serde_json::from_str(strip_fence(raw))
        .map_err(|e| clip(&format!("output is not JSON: {e}")))?;
    match shape {
        AnswerShape::Choice {
            choices,
            rationale_max_bytes,
        } => {
            let object = value
                .as_object()
                .ok_or_else(|| "output is not a JSON object".to_string())?;
            let choice = object
                .get("choice")
                .and_then(Value::as_str)
                .ok_or_else(|| "\"choice\" is missing or not a string".to_string())?;
            if !choices.iter().any(|c| c == choice) {
                return Err(clip(&format!(
                    "choice {choice:?} is not one of the choices"
                )));
            }
            let rationale = object
                .get("rationale")
                .and_then(Value::as_str)
                .ok_or_else(|| "\"rationale\" is missing or not a string".to_string())?;
            if rationale.len() as u64 > *rationale_max_bytes {
                return Err(format!(
                    "rationale is {} bytes, over the limit of {rationale_max_bytes}",
                    rationale.len()
                ));
            }
            Ok(json!({"choice": choice, "rationale": rationale}))
        }
        AnswerShape::Json { schema } => {
            let validator = jsonschema::validator_for(schema)
                .map_err(|e| clip(&format!("answer schema is unusable: {e}")))?;
            if let Some(violation) = validator.iter_errors(&value).next() {
                return Err(clip(&format!(
                    "output violates the schema at {}: {violation}",
                    violation.instance_path()
                )));
            }
            Ok(value)
        }
    }
}

/// Removes one surrounding Markdown code fence, with or without a language tag.
fn strip_fence(raw: &str) -> &str {
    let text = raw.trim();
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some(rest) = rest.strip_suffix("```") else {
        return text;
    };
    match rest.split_once('\n') {
        Some((_tag, body)) => body.trim(),
        None => rest.trim(),
    }
}

fn clip(text: &str) -> String {
    if text.len() <= CLIP_BYTES {
        return text.to_owned();
    }
    let mut end = CLIP_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}
