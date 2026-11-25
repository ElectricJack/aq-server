use super::*;

#[test]
fn fenced_json_is_accepted_and_prose_is_invalid_output() {
    let shape = choice_shape();
    let fenced = "```json\n{\"choice\": \"small\", \"rationale\": \"ok\"}\n```";
    assert_eq!(
        validate::answer(&shape, fenced, 4_096).unwrap(),
        json!({"choice": "small", "rationale": "ok"})
    );
    let bare = "```\n{\"choice\": \"large\", \"rationale\": \"\"}\n```";
    assert!(validate::answer(&shape, bare, 4_096).is_ok());
    let err = validate::answer(&shape, "I think the task is small.", 4_096).unwrap_err();
    assert!(err.contains("not JSON"), "{err}");
    let err = validate::answer(&shape, "[1]", 4_096).unwrap_err();
    assert!(err.contains("not a JSON object"), "{err}");
    let prompt = validate::prompt(&request(false));
    assert!(prompt.starts_with("Classify the task.\n\nTask: fix the login page\n\n"));
    assert!(prompt.ends_with(
        "Answer with one JSON object and nothing else: {\"choice\": one of [\"small\",\"large\"], \"rationale\": string}"
    ));
}

#[test]
fn choice_outside_the_list_and_oversized_rationale_are_invalid() {
    let shape = choice_shape();
    let err =
        validate::answer(&shape, r#"{"choice": "huge", "rationale": "x"}"#, 4_096).unwrap_err();
    assert!(err.contains("huge") && err.contains("choices"), "{err}");
    let long = format!(
        r#"{{"choice": "small", "rationale": "{}"}}"#,
        "x".repeat(17)
    );
    let err = validate::answer(&shape, &long, 4_096).unwrap_err();
    assert!(err.contains("rationale") && err.contains("16"), "{err}");
    let err = validate::answer(&shape, r#"{"choice": "small"}"#, 4_096).unwrap_err();
    assert!(err.contains("rationale"), "{err}");
}

#[test]
fn json_answer_is_checked_against_the_schema() {
    let shape = AnswerShape::Json {
        schema: json!({
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"],
            "additionalProperties": false
        }),
    };
    assert_eq!(
        validate::answer(&shape, r#"{"n": 3}"#, 4_096).unwrap(),
        json!({"n": 3})
    );
    assert!(validate::answer(&shape, r#"{"n": "three"}"#, 4_096).is_err());
    assert!(validate::answer(&shape, "{}", 4_096).is_err());
    let secret_text = "s".repeat(1_000);
    let wrong = format!(r#"{{"n": "{secret_text}"}}"#);
    let err = validate::answer(&shape, &wrong, 4_096).unwrap_err();
    assert!(
        err.len() <= 220 && !err.contains(&secret_text),
        "{}",
        err.len()
    );
    let bad_schema = AnswerShape::Json {
        schema: json!({"type": 12}),
    };
    assert!(validate::answer(&bad_schema, "{}", 4_096).is_err());
}

#[test]
fn oversized_output_is_invalid_and_the_error_does_not_echo_it() {
    let shape = choice_shape();
    let big = format!("zzz-marker-{}", "z".repeat(5_000));
    let err = validate::answer(&shape, &big, 100).unwrap_err();
    assert!(err.contains("5011") && err.contains("100"), "{err}");
    assert!(!err.contains("zzz"), "{err}");
    let err = validate::answer(&shape, &format!("{{{}", "y".repeat(5_000)), 9_999).unwrap_err();
    assert!(err.len() <= 220, "{}", err.len());
}
