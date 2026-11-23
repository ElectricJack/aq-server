use aq_intelligence_config::{CredentialConfig, IntelligenceConfig, LoadedRegistry};
use aq_internal_work::{ObservedAt, Reason, Way, resolve};
use aq_profiles::roles::{Kind, Snapshot, SourceDocument};
use aq_selection::{
    ArtifactRef, AuthorizedCredential, Availability, AvailabilityObservation, CredentialKind,
    Digest, ObservationScope, ObservationSource, Text,
};
use uuid::Uuid;

const NOW: u64 = 1_000_000;

/// A candidate row of a level document: `provider`'s fixture model at
/// `reasoning` on `harnesses`.
pub(super) type Row<'a> = (&'a str, &'a str, &'a [&'a str]);

/// The Markdown of the level `id` with `rows`, and `selection` lines
/// under `## Selection` when there are any.
pub(super) fn level(id: &str, rows: &[Row], selection: &[&str]) -> String {
    let rows: String = (rows.iter())
        .map(|(provider, reasoning, harnesses)| {
            let harnesses = harnesses.join(", ");
            format!("| {provider} | {provider}-model | {reasoning} | {harnesses} |\n")
        })
        .collect();
    let selection = match selection {
        [] => String::new(),
        lines => format!("\n## Selection\n- {}\n", lines.join("\n- ")),
    };
    format!(
        "---\nid: {id}\nname: Level {id}\ndescription: Fixture level {id}.\n---\n\
         ## Candidates\n| provider | model | reasoning | harnesses |\n|---|---|---|---|\n\
         {rows}{selection}"
    )
}

/// The shipped roles and profiles with the level `internal` added.
fn snapshot_with(rows: &[Row], selection: &[&str]) -> Snapshot {
    let mut sources = aq_profiles::roles::default_documents();
    sources.push(SourceDocument {
        kind: Kind::Intelligence,
        label: "internal".into(),
        text: level("internal", rows, selection),
    });
    aq_profiles::roles::compile(&sources, &Default::default(), &Default::default())
        .unwrap_or_else(|problems| panic!("{problems:?}"))
}
fn snapshot(rows: &[Row]) -> Snapshot {
    snapshot_with(rows, &[])
}

fn profile_ref(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}

pub(super) fn credential(name: &str, provider: &str, enabled: bool) -> AuthorizedCredential {
    AuthorizedCredential {
        profile: profile_ref(name),
        provider: provider.into(),
        account_reference: format!("{name}-account"),
        endpoint_reference: format!("{name}-endpoint"),
        revision: 1,
        enabled,
        provisionable: false,
        material_owner: None,
        allowed_backends: aq_selection::default_allowed_backends(),
    }
}

pub(super) fn registry(items: Vec<(AuthorizedCredential, CredentialKind)>) -> LoadedRegistry {
    IntelligenceConfig {
        accounting: Default::default(),
        material_store: None,
        revision: 1,
        credentials: items
            .into_iter()
            .map(|(credential, kind)| CredentialConfig {
                kind,
                allowed_backends: aq_selection::default_allowed_backends(),
                credential,
                allowed_node_ids: vec![],
                allow_enrolled_nodes: true,
                secret_file: None,
                delivery_mode: None,
            })
            .collect(),
        qualified_harnesses: vec![],
    }
    .load()
    .unwrap()
}

fn observation(
    credential: &AuthorizedCredential,
    state: Availability,
    expires_at_ms: u64,
    cooldown_until_ms: u64,
) -> AvailabilityObservation {
    AvailabilityObservation {
        observation_id: Uuid::new_v4(),
        scope: ObservationScope {
            credential_revision: credential.revision,
            provider: credential.provider.clone(),
            credential_profile: credential.profile.clone(),
            account_reference: credential.account_reference.clone(),
            endpoint_reference: credential.endpoint_reference.clone(),
            model: None,
            node_id: None,
        },
        source: ObservationSource::Operator {
            principal_id: Uuid::from_u128(1),
        },
        sequence: 1,
        revision: 1,
        state,
        received_at_ms: NOW - 10,
        expires_at_ms,
        cooldown_until_ms,
    }
}

const TWO: [Row<'static>; 2] = [
    ("anthropic", "high", &["claude"]),
    ("openai", "low", &["codex"]),
];
fn two_providers() -> (Snapshot, LoadedRegistry, AuthorizedCredential) {
    let catalog = snapshot(&TWO);
    let login = credential("anthropic-login", "anthropic", true);
    let registry = registry(vec![
        (login.clone(), CredentialKind::SubscriptionLogin),
        (
            credential("openai-key", "openai", true),
            CredentialKind::ApiKey,
        ),
    ]);
    (catalog, registry, login)
}

fn observed(observations: &[AvailabilityObservation]) -> ObservedAt<'_> {
    ObservedAt {
        observations,
        now_ms: NOW,
    }
}

#[test]
fn candidates_keep_profile_order_and_carry_the_way_of_their_credential() {
    let (catalog, registry, _) = two_providers();
    let resolved = resolve(&catalog, &registry, observed(&[]), "internal").unwrap();
    assert_eq!(resolved.profile_id, "internal");
    let got: Vec<_> = resolved
        .candidates
        .iter()
        .map(|c| {
            (
                c.provider.as_str(),
                c.way,
                c.harness.as_deref(),
                c.credential_index,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("anthropic", Way::Login, Some("claude"), 0),
            ("openai", Way::Api, None, 1),
        ]
    );
    assert_eq!(resolved.candidates[0].reasoning.as_deref(), Some("high"));
    assert_eq!(resolved.candidates[1].reasoning.as_deref(), Some("low"));
    assert_eq!(resolved.candidates[1].model, "openai-model");
}

/// The one candidate of a level listing openai on a Codex login at
/// `reasoning`.
fn codex_login(reasoning: &str) -> aq_internal_work::Candidate {
    let catalog = snapshot(&[("openai", reasoning, &["codex"])]);
    let registry = registry(vec![(
        credential("openai-login", "openai", true),
        CredentialKind::SubscriptionLogin,
    )]);
    let resolved = resolve(&catalog, &registry, observed(&[]), "internal").unwrap();
    assert_eq!(resolved.candidates.len(), 1);
    resolved.candidates[0].clone()
}

#[test]
fn a_candidate_carries_the_effort_its_level_states() {
    let codex = codex_login("low");
    assert_eq!(codex.harness.as_deref(), Some("codex"));
    assert_eq!(codex.reasoning.as_deref(), Some("low"));
    assert_eq!(codex.model, "openai-model");
}

/// `default` is the harness's own setting: no effort is passed.
#[test]
fn the_effort_default_leaves_the_harness_its_own_setting() {
    assert_eq!(codex_login("default").reasoning, None);
}

/// Any level of the snapshot resolves, the shipped ones too: no model
/// list and no model choice stands between a level and its candidates.
#[test]
fn a_shipped_level_resolves_to_the_candidates_of_its_document() {
    let (catalog, _, _) = two_providers();
    let registry = registry(vec![(
        credential("anthropic-key", "anthropic", true),
        CredentialKind::ApiKey,
    )]);
    let level = catalog.intelligence("standard-high").unwrap();
    let resolved = resolve(&catalog, &registry, observed(&[]), "standard-high").unwrap();
    assert_eq!(resolved.profile_id, "standard-high");
    let got: Vec<_> = (resolved.candidates.iter())
        .map(|c| (c.provider.clone(), c.model.clone(), c.way))
        .collect();
    let stated: Vec<_> = (level.candidates.iter())
        .map(|c| (c.provider.clone(), c.model.clone(), Way::Api))
        .collect();
    assert_eq!(got, stated);
    assert!(!got.is_empty());
}

#[test]
fn quota_unavailable_login_is_skipped_so_the_next_provider_is_first() {
    let (catalog, registry, login) = two_providers();
    for state in [
        Availability::QuotaUnavailable,
        Availability::AuthenticationFailed,
        Availability::TemporaryUnavailable,
    ] {
        for cooldown in [0, NOW + 30_000] {
            let seen = [observation(&login, state, NOW + 60_000, cooldown)];
            let resolved = resolve(&catalog, &registry, observed(&seen), "internal").unwrap();
            assert_eq!(
                resolved.candidates.len(),
                1,
                "{state:?} cooldown {cooldown}"
            );
            assert_eq!(resolved.candidates[0].provider, "openai");
        }
        let seen = [observation(&login, state, NOW + 60_000, NOW + 30_000)];
        let resolved = resolve(&catalog, &registry, observed(&seen), "internal").unwrap();
        assert_eq!(resolved.candidates.len(), 1, "{state:?}");
        assert_eq!(resolved.candidates[0].provider, "openai");
    }
    let available = [observation(
        &login,
        Availability::Available,
        NOW + 60_000,
        0,
    )];
    let resolved = resolve(&catalog, &registry, observed(&available), "internal").unwrap();
    assert_eq!(resolved.candidates[0].provider, "anthropic");
}

fn lapsed_without_cooldown(login: &AuthorizedCredential) -> AvailabilityObservation {
    observation(login, Availability::QuotaUnavailable, NOW, 0)
}

#[test]
fn expired_observation_does_not_skip() {
    let (catalog, registry, login) = two_providers();
    let lapsed = observation(&login, Availability::QuotaUnavailable, NOW, NOW + 30_000);
    for seen in [lapsed, lapsed_without_cooldown(&login)] {
        let resolved = resolve(&catalog, &registry, observed(&[seen]), "internal").unwrap();
        assert_eq!(resolved.candidates[0].provider, "anthropic");
    }
}

#[test]
fn missing_profile_and_profile_without_usable_credentials_are_no_candidate() {
    let (catalog, usable, _) = two_providers();
    assert_eq!(
        resolve(&catalog, &usable, observed(&[]), "nope").unwrap_err(),
        Reason::NoCandidate
    );
    let disabled = registry(vec![
        (
            credential("anthropic-login", "anthropic", false),
            CredentialKind::SubscriptionLogin,
        ),
        (
            credential("elsewhere", "other", true),
            CredentialKind::ApiKey,
        ),
    ]);
    assert_eq!(
        resolve(&catalog, &disabled, observed(&[]), "internal").unwrap_err(),
        Reason::NoCandidate
    );
}

#[test]
fn login_candidate_whose_only_harness_is_gemini_is_dropped() {
    let catalog = snapshot(&[
        ("google", "default", &["gemini"]),
        ("openai", "default", &["codex", "gemini"]),
    ]);
    let registry = registry(vec![
        (
            credential("google-login", "google", true),
            CredentialKind::SubscriptionLogin,
        ),
        (
            credential("openai-login", "openai", true),
            CredentialKind::SubscriptionLogin,
        ),
    ]);
    let resolved = resolve(&catalog, &registry, observed(&[]), "internal").unwrap();
    assert_eq!(resolved.candidates.len(), 1);
    assert_eq!(resolved.candidates[0].provider, "openai");
    assert_eq!(resolved.candidates[0].harness.as_deref(), Some("codex"));
}

#[test]
fn recent_observation_older_than_the_policy_age_without_cooldown_does_not_skip() {
    let (_, registry, login) = two_providers();
    // The level states its own observation age, over the default.
    let catalog = snapshot_with(&TWO, &["observation_max_age_ms: 1000"]);
    let mut old = observation(&login, Availability::QuotaUnavailable, NOW + 60_000, 0);
    old.received_at_ms = NOW - 5_000;
    let resolved = resolve(&catalog, &registry, observed(std::slice::from_ref(&old)), "internal").unwrap();
    assert_eq!(resolved.candidates[0].provider, "anthropic");
    old.received_at_ms = NOW - 500;
    let resolved = resolve(&catalog, &registry, observed(&[old]), "internal").unwrap();
    assert_eq!(resolved.candidates[0].provider, "openai");
    // Without the override the default age (30 s) still covers it.
    let (catalog, _, _) = two_providers();
    let mut old = observation(&login, Availability::QuotaUnavailable, NOW + 60_000, 0);
    old.received_at_ms = NOW - 5_000;
    let resolved = resolve(&catalog, &registry, observed(&[old]), "internal").unwrap();
    assert_eq!(resolved.candidates[0].provider, "openai");
}
