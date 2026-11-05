use aq_result_media::*;
use serde_json::{Value, json};
fn example() -> Value {
    serde_json::from_str(include_str!(
        "../../../protocol/human-result-media/v1/example.json"
    ))
    .unwrap()
}
fn page(value: &Value) -> Result<MediaPage> {
    MediaPage::decode(&serde_json::to_vec(value).unwrap())
}
fn request() -> Value {
    json!({"operation_id":"00000000-0000-0000-0000-000000000009","input_digest":format!("sha256:{}","c".repeat(64)),"input_bytes":512,"filename":"work.png","capture":null})
}
fn status() -> UploadStatus {
    let p = page(&example()).unwrap();
    let a = p.items[0].clone();
    let r = &a.receipt;
    UploadStatus {
        contract: Contract::HumanResultMedia1,
        operation_id: r.artifact_id,
        actor_id: r.uploader_id,
        request_digest: Digest::of(b"literal request bytes"),
        binding: r.binding.clone(),
        input: r.input.clone(),
        capture: r.capture.clone(),
        state: UploadState::Retained,
        created_at_ms: r.retained_at_ms,
        upload_expires_at_ms: r.retained_at_ms + MAX_UPLOAD_TTL_MS,
        retain_until_ms: r.retain_until_ms,
        recovery: Recovery {
            put_allowed: false,
            finalize_allowed: false,
            reason: Some(RecoveryReason::AlreadyRetained),
        },
        rejection: None,
        artifact: Some(a),
        limits: Limits::default(),
        media: MediaKind::Image,
        poster_artifact_id: None,
    }
}
#[test]
fn closed_example_roundtrips_and_receipt_digest_uses_exact_canonical_object() {
    let p = page(&example()).unwrap();
    assert_eq!(serde_json::to_value(&p).unwrap(), example());
    let a = &p.items[0];
    assert_eq!(a.receipt.digest().unwrap(), a.receipt_digest);
    assert_eq!(
        canonical_bytes(&a.receipt).unwrap(),
        canonical_bytes(&serde_json::to_value(&a.receipt).unwrap()).unwrap()
    );
    status().validate().unwrap();
}
#[test]
fn unsupported_native_coverage_and_missing_coverage_cannot_be_empty_media_success() {
    for coverage in [
        Some(json!("phase_aware_native")),
        Some(json!("all_reports")),
        None,
    ] {
        let mut p = example();
        p["items"] = json!([]);
        match coverage {
            Some(v) => p["binding"]["coverage"] = v,
            None => {
                p["binding"].as_object_mut().unwrap().remove("coverage");
            }
        }
        assert!(page(&p).is_err());
    }
}
#[test]
fn unknown_duplicate_missing_nullable_and_oversized_json_are_rejected() {
    let good = serde_json::to_vec(&request()).unwrap();
    assert!(UploadCreate::decode(&good).is_ok());
    for field in ["capture", "filename", "operation_id"] {
        let mut q = request();
        q.as_object_mut().unwrap().remove(field);
        assert!(UploadCreate::decode(&serde_json::to_vec(&q).unwrap()).is_err());
    }
    let mut q = request();
    q["unknown"] = json!(true);
    assert!(UploadCreate::decode(&serde_json::to_vec(&q).unwrap()).is_err());
    let text = String::from_utf8(good).unwrap();
    let duplicate = text.replacen('{', "{\"capture\":null,", 1);
    assert!(UploadCreate::decode(duplicate.as_bytes()).is_err());
    assert!(UploadCreate::decode(&vec![b' '; MAX_JSON_BYTES + 1]).is_err());
    assert!(Empty::decode(b"{}").is_ok());
    assert!(Empty::decode(b"[]").is_err());
    assert!(Empty::decode(b"{\"operation_id\":1}").is_err());
    let mut p = example();
    p["items"][0]["availability"]
        .as_object_mut()
        .unwrap()
        .remove("reason");
    assert!(page(&p).is_err());
}
#[test]
fn positional_nested_struct_sequences_do_not_bypass_closed_object_contract() {
    let mut q = request();
    q["capture"] = json!({"scenario":"scenario","captured_at_ms":1,"viewport":[390,844,2000],"environment":"test","claimed_source":null});
    assert!(UploadCreate::decode(&serde_json::to_vec(&q).unwrap()).is_err());
    let mut p = example();
    p["items"] = json!([[]]);
    assert!(page(&p).is_err());
}
#[test]
fn identifiers_digests_filenames_and_capture_are_bounded_without_path_interpretation() {
    assert!(Id::parse("00000000-0000-0000-0000-000000000000").is_err());
    assert!(Id::parse("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA").is_err());
    for digest in [
        "a".repeat(64),
        format!("sha256:{}", "A".repeat(64)),
        format!("sha256:{}", "a".repeat(63)),
    ] {
        assert!(Digest::parse(&digest).is_err());
    }
    for name in [
        "../a.png",
        "C:\\a.png",
        "..",
        "a\0.png",
        "a\n.png",
        &"é".repeat(129),
    ] {
        let mut q = request();
        q["filename"] = json!(name);
        assert!(UploadCreate::decode(&serde_json::to_vec(&q).unwrap()).is_err());
    }
    let mut q = request();
    q["capture"] = json!({"scenario":"test/path is a label","captured_at_ms":1,"viewport":{"width_css":390,"height_css":844,"dpr_milli":2000},"environment":"isolated/test","claimed_source":null});
    assert!(UploadCreate::decode(&serde_json::to_vec(&q).unwrap()).is_ok());
    q["capture"]["viewport"]["dpr_milli"] = json!(0);
    assert!(UploadCreate::decode(&serde_json::to_vec(&q).unwrap()).is_err());
}
#[test]
fn receipt_and_page_scope_variant_and_digest_mismatches_fail_closed() {
    let mut p = page(&example()).unwrap();
    p.items[0].receipt.input.filename = "changed.png".into();
    assert!(p.validate().is_err());
    let mut p = page(&example()).unwrap();
    p.items[0].receipt.binding.task_id = Id::new(uuid::Uuid::from_u128(123)).unwrap();
    p.items[0].receipt_digest = p.items[0].receipt.digest().unwrap();
    assert!(p.validate().is_err());
    let mut p = page(&example()).unwrap();
    p.items[0].receipt.variants.preview.width = 319;
    assert!(p.items[0].receipt.validate().is_err());
    let mut p = page(&example()).unwrap();
    p.items[0].receipt.variants.render.bytes = MAX_RENDER_BYTES as u64 + 1;
    assert!(p.items[0].receipt.validate().is_err());
    let mut p = page(&example()).unwrap();
    p.items.push(p.items[0].clone());
    assert!(p.validate().is_err());
    let mut p = example();
    p["items"][0]["receipt"]["variants"]["render"]["mime"] = json!("text/html");
    assert!(page(&p).is_err());
}
#[test]
fn availability_is_distinct_from_receipt_and_current_policy_limits() {
    let mut p = page(&example()).unwrap();
    p.server_time_ms = p.items[0].receipt.retain_until_ms;
    assert!(p.validate().is_err());
    p.items[0].availability = Availability {
        state: AvailabilityState::Expired,
        observed_at_ms: p.server_time_ms,
        reason: Some(AvailabilityReason::RetentionExpired),
    };
    p.validate().unwrap();
    let mut p = page(&example()).unwrap();
    p.limits.max_render_bytes = 1;
    assert!(p.validate().is_err());
    p.items[0].availability.state = AvailabilityState::Withheld;
    p.items[0].availability.reason = Some(AvailabilityReason::OwnerWithheld);
    p.validate().unwrap();
    p.items[0].availability.reason = Some(AvailabilityReason::FilesMissing);
    assert!(p.validate().is_err());
}
#[test]
fn terminal_status_and_exact_intent_receipt_cannot_offer_replay_or_another_task() {
    let s = status();
    s.validate().unwrap();
    let mut bad = s.clone();
    bad.recovery.put_allowed = true;
    assert!(bad.validate().is_err());
    let mut bad = s.clone();
    bad.operation_id = Id::new(uuid::Uuid::from_u128(123)).unwrap();
    assert!(bad.validate().is_err());
    let mut bad = s.clone();
    bad.state = UploadState::Finalizing;
    assert!(bad.validate().is_err());
    let mut bad = s.clone();
    bad.created_at_ms = 0;
    assert!(bad.validate().is_err());
    let mut bad = s.clone();
    bad.retain_until_ms += 1;
    assert!(bad.validate().is_err());
    let mut bad = s;
    bad.state = UploadState::Rejected;
    bad.artifact = None;
    bad.rejection = Some(Rejection::InvalidPng);
    bad.recovery.reason = Some(RecoveryReason::Rejected);
    bad.validate().unwrap();
    bad.recovery.finalize_allowed = true;
    assert!(bad.validate().is_err());
}
#[test]
fn raw_request_digest_is_independent_of_typed_equivalence() {
    let raw = serde_json::to_vec(&request()).unwrap();
    let spaced = format!(" {} ", String::from_utf8(raw.clone()).unwrap());
    assert_eq!(
        UploadCreate::decode(&raw).unwrap(),
        UploadCreate::decode(spaced.as_bytes()).unwrap()
    );
    assert_ne!(Digest::of(&raw), Digest::of(spaced.as_bytes()));
}
#[test]
fn lowered_admission_policy_cannot_validate_a_larger_frozen_input() {
    let request = UploadCreate::decode(&serde_json::to_vec(&request()).unwrap()).unwrap();
    let limits = Limits {
        max_input_bytes: 511,
        ..Limits::default()
    };
    assert!(request.validate_with_limits(&limits).is_err());
    let mut status = status();
    status.limits = limits;
    assert!(status.validate().is_err());
}
