use aq_accounting::{pricing::*, reconcile::*, *};
use uuid::Uuid;
use wire::*;
fn text(s: &str) -> Text {
    Text::new(s).unwrap()
}
fn artifact(s: &str) -> ArtifactRef {
    ArtifactRef {
        id: text(s),
        version: text("1.0.0"),
        digest: Digest::of(s.as_bytes()),
    }
}
fn known(n: u64) -> Counter {
    Counter::Known { value: n }
}
fn counters(input: u64, output: u64, read: u64, write: u64, reason: u64) -> Counters {
    Counters {
        input: known(input),
        output: known(output),
        cache_read: known(read),
        cache_write: known(write),
        reasoning: known(reason),
    }
}
fn layout() -> Layout {
    Layout {
        policy: artifact("categories"),
        input_includes_cache_read: true,
        input_includes_cache_write: true,
        output_includes_reasoning: true,
    }
}
fn policy() -> Policy {
    Policy {
        reference: artifact("accounting-policy"),
        account_scope: Digest::of(b"actual-account"),
        call_namespace: Digest::of(b"native-conversation"),
        provider: text("provider"),
        layout: layout(),
        parsers: vec![
            ParserAuthority {
                parser: artifact("parser-1"),
                category_claim: artifact("categories"),
                priority: 1,
                complete_accounting_evidence: Some(artifact("synthetic-qualification")),
            },
            ParserAuthority {
                parser: artifact("parser-2"),
                category_claim: artifact("categories"),
                priority: 2,
                complete_accounting_evidence: Some(artifact("synthetic-qualification")),
            },
        ],
    }
}
fn record() -> ObservationRecord {
    let attempt = Uuid::new_v4();
    let session = SessionIdentity::Remote {
        node_id: Uuid::new_v4(),
        boot_id: Uuid::new_v4(),
        attempt_id: attempt,
        assignment_generation: 1,
        session_id: Uuid::new_v4(),
    };
    let body = Observation {
        binding: Binding {
            project_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            lineage_id: Uuid::new_v4(),
            attempt_id: attempt,
            attempt_generation: 1,
            session,
        },
        provenance: Provenance {
            source_id: Uuid::new_v4(),
            source_generation: 1,
            source_kind: SourceKind::NativeTranscript,
            native_session_key: text("native-session"),
            cursor: SourceCursor::Record {
                id: text("record-1"),
            },
            parser: artifact("parser-1"),
            raw_record: RawRecordEvidence::Available {
                digest: Digest::of(b"x"),
                byte_length: 1,
            },
        },
        sequence: 1,
        observed_at_ms: 1000,
        kind: ObservationKind::Usage {
            usage: Box::new(Usage {
                call: ProviderCall {
                    provider: text("provider"),
                    account_reference: text("untrusted-label"),
                    model: text("model"),
                    native_call_id: text("call-1"),
                },
                basis: CounterBasis::PerCall,
                counters: counters(100, 50, 20, 10, 15),
                category_semantics: CategorySemantics::MayOverlap {
                    policy: artifact("categories"),
                },
                cost: Cost::ProviderReported {
                    amount_microunits: 999999,
                    currency: text("USD"),
                    receipt: artifact("unverified-charge"),
                },
                completeness: Completeness::ReportedComplete,
                correction_of: None,
            }),
        },
    };
    ObservationRecord::seal(Uuid::new_v4(), body).unwrap()
}
fn usage(r: &mut ObservationRecord) -> &mut Usage {
    let ObservationKind::Usage { usage } = &mut r.body.kind else {
        panic!()
    };
    usage
}
fn reseal(r: ObservationRecord) -> ObservationRecord {
    ObservationRecord::seal(Uuid::new_v4(), r.body).unwrap()
}
fn context<'a>(r: &ObservationRecord, p: &'a Policy, current: Option<&'a Fact>) -> Context<'a> {
    Context {
        lineage: r.body.binding.lineage_id,
        policy: p,
        current,
        correction_target: None,
        baseline: None,
        pricing_reference: None,
    }
}
fn appended(d: Decision) -> Fact {
    let Decision::Append { fact, .. } = d else {
        panic!("expected append, got {d:?}")
    };
    *fact
}
#[test]
fn exact_call_replay_and_cross_source_precedence_do_not_add_two_bills() {
    let p = policy();
    let r = record();
    let a = appended(decide(&r, context(&r, &p, None)).unwrap());
    assert_eq!(
        decide(&r, context(&r, &p, Some(&a))).unwrap(),
        Decision::NoCharge {
            reason: NoChargeReason::Identical
        }
    );
    let mut other = r.clone();
    other.body.provenance.source_id = Uuid::new_v4();
    other.body.provenance.parser = artifact("parser-2");
    other.body.provenance.source_kind = SourceKind::StructuredProviderEvent;
    let other = reseal(other);
    let decision = decide(&other, context(&other, &p, Some(&a))).unwrap();
    let Decision::Append {
        fact,
        supersedes,
        previous_contribution,
    } = decision
    else {
        panic!()
    };
    assert_eq!(fact.contribution, a.contribution);
    assert_eq!(supersedes, Some(a.event.clone()));
    assert_eq!(previous_contribution, Some(a.contribution));
    assert_eq!(
        decide(&r, context(&r, &p, Some(&fact))).unwrap(),
        Decision::NoCharge {
            reason: NoChargeReason::LowerPrecedence
        }
    );
}
#[test]
fn explicit_correction_replaces_prior_contribution_and_rejects_stale_target() {
    let p = policy();
    let r = record();
    let a = appended(decide(&r, context(&r, &p, None)).unwrap());
    let mut corrected = r.clone();
    usage(&mut corrected).counters.input = known(90);
    usage(&mut corrected).correction_of = Some(a.event.clone());
    let corrected = reseal(corrected);
    let mut c = context(&corrected, &p, Some(&a));
    c.correction_target = Some(&a);
    let b = appended(decide(&corrected, c).unwrap());
    assert_eq!(b.contribution.input, known(90));
    let mut stale = corrected.clone();
    usage(&mut stale).counters.input = known(85);
    let stale = reseal(stale);
    let mut c = context(&stale, &p, Some(&b));
    c.correction_target = Some(&a);
    assert_eq!(
        decide(&stale, c).unwrap(),
        Decision::Unresolved {
            reason: UnresolvedReason::StaleCorrection
        }
    );
}
#[test]
fn unknown_corrections_and_equal_precedence_conflicts_remain_incomplete() {
    let p = policy();
    let r = record();
    let a = appended(decide(&r, context(&r, &p, None)).unwrap());
    let mut changed = r.clone();
    usage(&mut changed).counters.input = known(90);
    let changed = reseal(changed);
    assert_eq!(
        decide(&changed, context(&changed, &p, Some(&a))).unwrap(),
        Decision::Unresolved {
            reason: UnresolvedReason::ConflictingCall
        }
    );
    let mut changed = changed;
    usage(&mut changed).correction_of = Some(a.event.clone());
    let changed = reseal(changed);
    assert_eq!(
        decide(&changed, context(&changed, &p, Some(&a))).unwrap(),
        Decision::Unresolved {
            reason: UnresolvedReason::UnknownCorrection
        }
    );
}
#[test]
fn history_import_preserves_original_lineage_and_namespace_separates_unrelated_calls() {
    let p = policy();
    let r = record();
    let a = appended(decide(&r, context(&r, &p, None)).unwrap());
    let mut successor = r.clone();
    successor.body.binding.lineage_id = Uuid::new_v4();
    let successor = reseal(successor);
    assert_eq!(
        decide(&successor, context(&successor, &p, Some(&a))).unwrap(),
        Decision::NoCharge {
            reason: NoChargeReason::ImportedHistory
        }
    );
    let mut unrelated = p.clone();
    unrelated.call_namespace = Digest::of(b"different-conversation");
    assert_ne!(key(&r, &p).unwrap(), key(&r, &unrelated).unwrap());
    let mut changed = successor;
    usage(&mut changed).counters.input = known(91);
    let changed = reseal(changed);
    assert_eq!(
        decide(&changed, context(&changed, &p, Some(&a))).unwrap(),
        Decision::Unresolved {
            reason: UnresolvedReason::ForeignLineageCorrection
        }
    );
}
#[test]
fn parser_revision_and_model_attribution_are_not_new_call_identity() {
    let p = policy();
    let r = record();
    let key1 = key(&r, &p).unwrap();
    let mut changed = r;
    changed.body.provenance.parser = artifact("parser-2");
    usage(&mut changed).call.model = text("corrected-model");
    let changed = reseal(changed);
    assert_eq!(key1, key(&changed, &p).unwrap());
    let mut unknown = changed;
    unknown.body.provenance.parser = artifact("unregistered-parser");
    let unknown = reseal(unknown);
    assert!(decide(&unknown, context(&unknown, &p, None)).is_err());
}
#[test]
fn cumulative_usage_requires_a_verified_baseline_and_preserves_it_on_updates() {
    let p = policy();
    let mut r = record();
    usage(&mut r).basis = CounterBasis::Cumulative {
        counter_id: text("conversation"),
        counter_generation: 1,
    };
    let r = reseal(r);
    assert_eq!(
        decide(&r, context(&r, &p, None)).unwrap(),
        Decision::Unresolved {
            reason: UnresolvedReason::MissingBaseline
        }
    );
    let baseline = Baseline {
        counter_key: key(&r, &p).unwrap(),
        reference: artifact("retained-history"),
        counters: counters(80, 30, 15, 5, 10),
    };
    let mut c = context(&r, &p, None);
    c.baseline = Some(&baseline);
    let a = appended(decide(&r, c).unwrap());
    assert_eq!(a.contribution, counters(20, 20, 5, 5, 5));
    let mut next = r;
    usage(&mut next).counters = counters(110, 60, 25, 10, 20);
    let next = reseal(next);
    let b = appended(decide(&next, context(&next, &p, Some(&a))).unwrap());
    assert_eq!(b.baseline, Some(baseline));
    assert_eq!(b.contribution, counters(30, 30, 10, 5, 10));
}
#[test]
fn cumulative_reset_mixed_model_and_unknown_baselines_cannot_manufacture_new_spending() {
    let p = policy();
    let mut r = record();
    usage(&mut r).basis = CounterBasis::Cumulative {
        counter_id: text("conversation"),
        counter_generation: 1,
    };
    let r = reseal(r);
    let baseline = Baseline {
        counter_key: key(&r, &p).unwrap(),
        reference: artifact("fresh-zero"),
        counters: zeros(),
    };
    let mut c = context(&r, &p, None);
    c.baseline = Some(&baseline);
    let a = appended(decide(&r, c).unwrap());
    let mut reset = r.clone();
    usage(&mut reset).counters = counters(90, 45, 20, 10, 10);
    let reset = reseal(reset);
    assert_eq!(
        decide(&reset, context(&reset, &p, Some(&a))).unwrap(),
        Decision::Unresolved {
            reason: UnresolvedReason::CounterReset
        }
    );
    let mut mixed = r;
    usage(&mut mixed).call.model = text("other-model");
    let mixed = reseal(mixed);
    assert_eq!(
        decide(&mixed, context(&mixed, &p, Some(&a))).unwrap(),
        Decision::Unresolved {
            reason: UnresolvedReason::AttributionChanged
        }
    );
    let mut b = baseline;
    b.counters.input = Counter::Unknown {
        reason: UnknownReason::Incomplete,
    };
    // Unknown baseline propagates rather than being coerced to zero.
    assert!(matches!(
        subtract(&counters(100, 50, 20, 10, 15), &b.counters)
            .unwrap()
            .input,
        Counter::Unknown { .. }
    ));
}
#[test]
fn reported_zero_is_distinct_from_unknown_and_overlap_is_not_double_counted() {
    let l = layout();
    assert_eq!(
        l.unique_total(&counters(100, 50, 20, 10, 15)).unwrap(),
        known(150)
    );
    assert_eq!(l.unique_total(&zeros()).unwrap(), known(0));
    let mut c = zeros();
    c.input = Counter::Unknown {
        reason: UnknownReason::NotReported,
    };
    assert!(matches!(
        l.unique_total(&c).unwrap(),
        Counter::Unknown { .. }
    ));
    assert!(l.check_counters(&counters(10, 50, 20, 0, 15)).is_err());
}
fn rate() -> RateCard {
    RateCard {
        reference: artifact("rate-1"),
        allocation_policy: artifact("ceil-per-call"),
        currency: text("USD"),
        provider: text("provider"),
        model: text("model"),
        layout: layout(),
        rules: vec![
            Rule {
                category: Category::Input,
                subtract: vec![Category::CacheRead, Category::CacheWrite],
                microunits_per_million_tokens: 2_000_000,
            },
            Rule {
                category: Category::CacheRead,
                subtract: vec![],
                microunits_per_million_tokens: 500_000,
            },
            Rule {
                category: Category::CacheWrite,
                subtract: vec![],
                microunits_per_million_tokens: 3_000_000,
            },
            Rule {
                category: Category::Output,
                subtract: vec![],
                microunits_per_million_tokens: 4_000_000,
            },
        ],
    }
}
#[test]
fn deterministic_pricing_uses_explicit_disjoint_billing_rules_not_all_raw_fields() {
    let r = rate();
    let p = r
        .estimate(
            &text("provider"),
            &text("model"),
            &counters(100, 50, 20, 10, 15),
        )
        .unwrap();
    assert!(matches!(
        p.cost,
        Cost::NonInvoiceEstimate {
            amount_microunits: 380,
            ..
        }
    ));
    assert!(p.missing_categories.is_empty());
    // Reasoning is included in output and therefore is not charged again.
    let mut double = r.clone();
    double.rules.push(Rule {
        category: Category::Reasoning,
        subtract: vec![],
        microunits_per_million_tokens: 4_000_000,
    });
    assert!(double.validate().is_err());
    let mut missing = r;
    missing.rules.remove(1);
    assert!(missing.validate().is_err());
}
#[test]
fn partial_pricing_exposes_lower_bound_currency_and_unknown_cost() {
    let mut c = counters(100, 50, 20, 10, 15);
    c.input = Counter::Unknown {
        reason: UnknownReason::NotReported,
    };
    let r = rate();
    let p = r.estimate(&text("provider"), &text("model"), &c).unwrap();
    assert!(matches!(p.cost, Cost::Unknown { .. }));
    assert!(matches!(
        p.known_lower_bound,
        Cost::NonInvoiceEstimate {
            amount_microunits: 240,
            ..
        }
    ));
    assert_eq!(p.missing_categories, vec![Category::Input]);
    assert!(
        r.estimate(&text("provider"), &text("different-model"), &c)
            .is_err()
    );
    let mut r = r;
    r.currency = text("usd");
    assert!(r.validate().is_err());
}
#[test]
fn pricing_rounds_once_per_contribution_and_refuses_overflow() {
    let mut r = rate();
    for rule in &mut r.rules {
        rule.microunits_per_million_tokens = 1;
    }
    let p = r
        .estimate(&text("provider"), &text("model"), &counters(2, 2, 0, 0, 0))
        .unwrap();
    assert!(matches!(
        p.cost,
        Cost::NonInvoiceEstimate {
            amount_microunits: 1,
            ..
        }
    ));
    for rule in &mut r.rules {
        rule.microunits_per_million_tokens = u64::MAX;
    }
    assert!(
        r.estimate(
            &text("provider"),
            &text("model"),
            &counters(u64::MAX, u64::MAX, 0, 0, 0)
        )
        .is_err()
    );
}
#[test]
fn price_reference_is_pinned_and_unverified_reported_cost_does_not_change_tokens() {
    let p = policy();
    let r = record();
    let rate = artifact("original-rate");
    let mut c = context(&r, &p, None);
    c.pricing_reference = Some(&rate);
    let a = appended(decide(&r, c).unwrap());
    let mut changed = r.clone();
    usage(&mut changed).cost = Cost::Unknown {
        reason: UnknownReason::NotReported,
    };
    let changed = reseal(changed);
    assert_eq!(
        decide(&changed, context(&changed, &p, Some(&a))).unwrap(),
        Decision::NoCharge {
            reason: NoChargeReason::Identical
        }
    );
    let mut corrected = changed;
    usage(&mut corrected).counters.input = known(90);
    usage(&mut corrected).correction_of = Some(a.event.clone());
    let corrected = reseal(corrected);
    let new_rate = artifact("new-rate");
    let mut c = context(&corrected, &p, Some(&a));
    c.correction_target = Some(&a);
    c.pricing_reference = Some(&new_rate);
    let b = appended(decide(&corrected, c).unwrap());
    assert_eq!(b.pricing_reference, Some(rate));
}
#[test]
fn stored_fact_mutation_and_unknown_policy_are_refused() {
    let p = policy();
    let r = record();
    let mut a = appended(decide(&r, context(&r, &p, None)).unwrap());
    a.contribution.input = known(0);
    assert!(a.validate().is_err());
    let mut r = r;
    usage(&mut r).category_semantics = CategorySemantics::MayOverlap {
        policy: artifact("invented-policy"),
    };
    let r = reseal(r);
    assert!(decide(&r, context(&r, &p, None)).is_err());
}

#[test]
fn projection_corrects_in_place_without_resetting_other_attempts_or_unknown_categories() {
    use aq_accounting::projection::Projection;
    let first = counters(100, 50, 20, 10, 15);
    let second = counters(80, 10, 0, 0, 0);
    let a = Projection::default()
        .replace(None, &first)
        .unwrap()
        .replace(None, &second)
        .unwrap();
    assert_eq!(a.canonical_calls, 2);
    assert_eq!(a.counters().unwrap().input, known(180));
    let mut corrected = first.clone();
    corrected.input = known(90);
    corrected.output = Counter::Unknown {
        reason: UnknownReason::SourceGap,
    };
    let b = a.replace(Some(&first), &corrected).unwrap();
    assert_eq!(b.canonical_calls, 2);
    assert_eq!(b.known_subtotals[0], 170);
    assert_eq!(b.known_subtotals[1], 10);
    assert!(matches!(
        b.counters().unwrap().output,
        Counter::Unknown { .. }
    ));
    assert!(!b.all_categories_represented().unwrap());
    let c = b.replace(Some(&corrected), &first).unwrap();
    assert_eq!(c, a);
    assert!(c.all_categories_represented().unwrap());
    assert!(
        Projection::default()
            .replace(Some(&first), &second)
            .is_err()
    );
}
#[test]
fn cumulative_baseline_cannot_be_borrowed_from_another_counter() {
    let p = policy();
    let mut r = record();
    usage(&mut r).basis = CounterBasis::Cumulative {
        counter_id: text("counter"),
        counter_generation: 1,
    };
    let r = reseal(r);
    let baseline = Baseline {
        counter_key: Digest::of(b"foreign-counter"),
        reference: artifact("fresh-zero"),
        counters: zeros(),
    };
    let mut c = context(&r, &p, None);
    c.baseline = Some(&baseline);
    assert!(decide(&r, c).is_err());
}

#[test]
fn empty_projection_and_unresolved_events_do_not_look_like_reported_zero() {
    use aq_accounting::projection::Projection;
    let empty = Projection::default();
    assert!(!empty.all_categories_represented().unwrap());
    assert!(matches!(
        empty.counters().unwrap().input,
        Counter::Unknown {
            reason: UnknownReason::NotReported
        }
    ));
    let zero = empty.replace(None, &zeros()).unwrap();
    assert_eq!(zero.counters().unwrap().input, known(0));
    assert!(zero.all_categories_represented().unwrap());
    let mut gap = zero;
    gap.unresolved_events = 1;
    assert!(!gap.all_categories_represented().unwrap());
    assert!(matches!(
        gap.counters().unwrap().input,
        Counter::Unknown {
            reason: UnknownReason::Incomplete
        }
    ));
}

#[test]
fn canonical_fact_pricing_requires_its_pinned_card_and_category_semantics() {
    let p = policy();
    let r = record();
    let card = rate();
    let mut c = context(&r, &p, None);
    c.pricing_reference = Some(&card.reference);
    let fact = appended(decide(&r, c).unwrap());
    assert!(matches!(
        card.estimate_fact(&fact).unwrap().cost,
        Cost::NonInvoiceEstimate {
            amount_microunits: 380,
            ..
        }
    ));
    let mut wrong = card.clone();
    wrong.reference = artifact("other-rate");
    assert!(wrong.estimate_fact(&fact).is_err());
    let mut wrong = card;
    wrong.layout.policy = artifact("other-categories");
    assert!(wrong.estimate_fact(&fact).is_err());
}

#[test]
fn parser_artifact_category_claims_map_to_stable_owner_layout_during_reparse() {
    let mut p = policy();
    p.parsers[0].category_claim = artifact("parser-1");
    p.parsers[1].category_claim = artifact("parser-2");
    let mut r = record();
    usage(&mut r).category_semantics = CategorySemantics::MayOverlap {
        policy: artifact("parser-1"),
    };
    let r = reseal(r);
    let a = appended(decide(&r, context(&r, &p, None)).unwrap());
    let mut corrected = r;
    corrected.body.provenance.parser = artifact("parser-2");
    usage(&mut corrected).category_semantics = CategorySemantics::MayOverlap {
        policy: artifact("parser-2"),
    };
    usage(&mut corrected).counters.input = known(90);
    usage(&mut corrected).correction_of = Some(a.event.clone());
    let corrected = reseal(corrected);
    let mut c = context(&corrected, &p, Some(&a));
    c.correction_target = Some(&a);
    let b = appended(decide(&corrected, c).unwrap());
    assert_eq!(a.key, b.key);
    assert_eq!(a.layout, b.layout);
    assert_eq!(b.contribution.input, known(90));
    let mut false_claim = corrected;
    usage(&mut false_claim).category_semantics = CategorySemantics::MayOverlap {
        policy: artifact("parser-1"),
    };
    let false_claim = reseal(false_claim);
    assert!(decide(&false_claim, context(&false_claim, &p, Some(&b))).is_err());
}

#[test]
fn partial_source_stays_unknown_until_exact_complete_correction() {
    let p = policy();
    let card = rate();
    let mut r = record();
    usage(&mut r).completeness = Completeness::Partial {
        reason: UnknownReason::Incomplete,
    };
    let r = reseal(r);
    let mut c = context(&r, &p, None);
    c.pricing_reference = Some(&card.reference);
    let a = appended(decide(&r, c).unwrap());
    let projection = projection::Projection::default()
        .replace_fact(None, &a)
        .unwrap();
    assert_eq!(projection.incomplete_calls, 1);
    assert_eq!(projection.known_subtotals[0], 100);
    assert!(matches!(
        projection.counters().unwrap().input,
        Counter::Unknown {
            reason: UnknownReason::Incomplete
        }
    ));
    assert!(!projection.all_categories_represented().unwrap());
    let pricing = card.estimate_fact(&a).unwrap();
    assert!(pricing.source_incomplete);
    assert!(matches!(pricing.cost, Cost::Unknown { .. }));
    assert!(matches!(
        pricing.known_lower_bound,
        Cost::NonInvoiceEstimate { .. }
    ));
    let mut corrected = r;
    usage(&mut corrected).completeness = Completeness::ReportedComplete;
    usage(&mut corrected).correction_of = Some(a.event.clone());
    let corrected = reseal(corrected);
    let mut c = context(&corrected, &p, Some(&a));
    c.correction_target = Some(&a);
    let b = appended(decide(&corrected, c).unwrap());
    let projection = projection.replace_fact(Some(&a), &b).unwrap();
    assert_eq!(projection.incomplete_calls, 0);
    assert_eq!(projection.canonical_calls, 1);
    assert_eq!(projection.known_subtotals[0], 100);
    assert!(projection.all_categories_represented().unwrap());
    assert!(!card.estimate_fact(&b).unwrap().source_incomplete);
}

#[test]
fn worker_complete_claim_requires_owner_parser_qualification() {
    let mut p = policy();
    p.parsers[0].complete_accounting_evidence = None;
    let card = rate();
    let r = record();
    let mut c = context(&r, &p, None);
    c.pricing_reference = Some(&card.reference);
    let a = appended(decide(&r, c).unwrap());
    assert!(matches!(
        a.usage.completeness,
        Completeness::ReportedComplete
    ));
    let projection = projection::Projection::default()
        .replace_fact(None, &a)
        .unwrap();
    assert_eq!(projection.incomplete_calls, 1);
    assert!(!projection.all_categories_represented().unwrap());
    assert!(card.estimate_fact(&a).unwrap().source_incomplete);
    p.parsers[0].complete_accounting_evidence =
        Some(artifact("synthetic-selected-version-qualification"));
    let mut corrected = r;
    usage(&mut corrected).correction_of = Some(a.event.clone());
    let corrected = reseal(corrected);
    let mut c = context(&corrected, &p, Some(&a));
    c.correction_target = Some(&a);
    let b = appended(decide(&corrected, c).unwrap());
    let projection = projection.replace_fact(Some(&a), &b).unwrap();
    assert_eq!(projection.canonical_calls, 1);
    assert_eq!(projection.incomplete_calls, 0);
    assert_eq!(projection.known_subtotals[0], 100);
}
fn revision(revision: u64, input_rate: u64) -> RateCardRevision {
    let mut card = rate();
    card.reference = artifact(&format!("rate-{revision}"));
    card.rules[0].microunits_per_million_tokens = input_rate;
    RateCardRevision {
        revision,
        corrects: (revision > 1).then(|| artifact(&format!("rate-{}", revision - 1))),
        card,
    }
}
fn priced(r: &ObservationRecord, pin: Option<&ArtifactRef>) -> Fact {
    let p = policy();
    let mut c = context(r, &p, None);
    c.pricing_reference = pin;
    appended(decide(r, c).unwrap())
}
#[test]
fn rate_card_corrections_are_revisions_that_reprice_once() {
    let book = RateCardBook::new(vec![revision(1, 2_000_000), revision(2, 1_000_000)]).unwrap();
    assert_eq!(
        book.current(&text("provider"), &text("model"))
            .unwrap()
            .reference,
        artifact("rate-2")
    );
    // A correction must name its predecessor and keep the revision chain dense.
    let mut gap = revision(3, 1);
    gap.corrects = Some(artifact("rate-1"));
    assert!(RateCardBook::new(vec![revision(1, 2_000_000), revision(2, 1), gap]).is_err());
    assert!(RateCardBook::new(vec![revision(2, 1)]).is_err());
    assert!(RateCardBook::new(vec![revision(1, 1), revision(1, 2)]).is_err());
    // A fact pinned to revision 1 is billed once, at the corrected revision.
    let mut r = record();
    usage(&mut r).cost = Cost::Unknown {
        reason: UnknownReason::NotReported,
    };
    let fact = priced(&reseal(r), Some(&artifact("rate-1")));
    let s = summarize(&[(Some(EstimateBasis::TokenRates), &fact)], &book).unwrap();
    // 70 x 1 + 20 x 0.5 + 10 x 3 + 50 x 4 = 310 microunits under revision 2.
    assert_eq!(s.spent_microunits, Some(310));
    assert!(s.complete);
    assert!(matches!(
        &s.costs[..],
        [Cost::NonInvoiceEstimate { amount_microunits: 310, rate_card, .. }] if *rate_card == artifact("rate-2")
    ));
}
#[test]
fn reported_zero_is_distinct_from_unknown() {
    let book = RateCardBook::new(vec![revision(1, 2_000_000)]).unwrap();
    let mut zero = record();
    usage(&mut zero).cost = Cost::ProviderReported {
        amount_microunits: 0,
        currency: text("USD"),
        receipt: artifact("receipt"),
    };
    let zero = priced(&reseal(zero), None);
    let s = summarize(&[(Some(EstimateBasis::TokenRates), &zero)], &book).unwrap();
    assert_eq!(s.spent_microunits, Some(0));
    assert!(s.complete);
    assert!(matches!(
        &s.costs[..],
        [Cost::ProviderReported {
            amount_microunits: 0,
            ..
        }]
    ));
    let mut unknown = record();
    usage(&mut unknown).cost = Cost::Unknown {
        reason: UnknownReason::NotReported,
    };
    usage(&mut unknown).call.native_call_id = text("call-2");
    let unknown = priced(&reseal(unknown), None);
    let s = summarize(&[(Some(EstimateBasis::TokenRates), &unknown)], &book).unwrap();
    assert!(!s.complete);
    assert_eq!(s.spent_microunits, Some(0));
    assert!(matches!(&s.costs[..], [Cost::Unknown { .. }]));
    // Flat subscription usage is labelled, never charged at token rates.
    let s = summarize(
        &[(Some(EstimateBasis::SubscriptionAllocation), &unknown)],
        &book,
    )
    .unwrap();
    assert!(!s.complete);
    assert_eq!(
        s.costs,
        vec![Cost::Unknown {
            reason: UnknownReason::NotApplicable
        }]
    );
    assert!(s.reason.unwrap().contains("subscription"));
}
