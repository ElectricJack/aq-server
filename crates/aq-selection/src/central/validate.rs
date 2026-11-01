use super::*;
use std::collections::BTreeSet;
fn positive(v: u64) -> Result<()> {
    ensure(v > 0 && v <= i64::MAX as u64, "central revision/time range")
}
fn id(v: uuid::Uuid) -> Result<()> {
    ensure(!v.is_nil(), "central nonnil identity")
}
fn reference(v: &ArtifactRef) -> Result<()> {
    v.id.validate()
        .map_err(|_| SelectionError::Invalid("central artifact id".into()))?;
    v.version
        .validate()
        .map_err(|_| SelectionError::Invalid("central artifact version".into()))
}
fn window(observed: u64, expires: u64) -> Result<()> {
    positive(observed)?;
    positive(expires)?;
    ensure(
        expires > observed && expires - observed <= 120_000,
        "central bounded snapshot window",
    )
}
fn same<T: serde::Serialize>(a: &T, b: &T) -> Result<bool> {
    Ok(pins::canonical(a, 64 * 1024)? == pins::canonical(b, 64 * 1024)?)
}
pub(super) struct Validated<'a> {
    pub intelligence: &'a aq_profiles::Intelligence,
    pub execution: &'a aq_profiles::Execution,
}
pub(super) fn validate<'a>(
    catalog: &'a aq_profiles::ValidatedCatalog,
    input: &CentralSelectionInputs,
) -> Result<Validated<'a>> {
    pins::bounded(input, MAX_INPUT_BYTES)?;
    ensure(input.schema_version == 1, "central selection version")?;
    positive(input.now_ms)?;
    positive(input.original_expires_at_ms)?;
    ensure(
        input.original_expires_at_ms.saturating_sub(input.now_ms) <= 3_600_000,
        "central original expiry bound",
    )?;
    ensure(
        !input.compositions.is_empty() && input.compositions.len() <= 16,
        "central composition cardinality",
    )?;
    input.reservation.verify()?;
    input.policy.verify()?;
    input.registry.verify()?;
    input.installation.verify()?;
    let r = &input.reservation.value;
    let p = &input.policy.value;
    r.policy.verify()?;
    for v in [r.reservation_id, r.actor_id, r.conversation_id] {
        id(v)?;
    }
    for v in [
        r.conversation_revision,
        r.actor_revision,
        r.reservation_revision,
        r.policy_revision,
        r.through_cursor,
    ] {
        positive(v)?;
    }
    ensure(
        r.reservation_revision == 1,
        "central reservation must be pre-plan",
    )?;
    let local = &r.policy.value;
    ensure(
        local.schema_version == 1
            && local.actor_id == r.actor_id
            && local.conversation_id == r.conversation_id,
        "central reservation local policy binding",
    )?;
    ensure(
        !r.messages.is_empty() && r.messages.len() <= 16,
        "central reserved mail bound",
    )?;
    let mut previous = 0;
    let mut ids = BTreeSet::new();
    for m in &r.messages {
        id(m.message_id)?;
        positive(m.cursor)?;
        ensure(
            m.cursor > previous && m.cursor <= r.through_cursor && ids.insert(m.message_id),
            "central reserved mail order/identity",
        )?;
        previous = m.cursor;
    }
    ensure(
        p.schema_version == 1
            && p.reservation == input.reservation.reference
            && p.reservation_policy == r.policy.reference,
        "central selection policy reservation pins",
    )?;
    for v in [
        &p.reservation,
        &p.reservation_policy,
        &p.composition_policy,
        &p.compatibility,
        &p.catalog,
    ] {
        reference(v)?;
    }
    ensure(
        p.max_output_tokens > 0 && p.max_output_tokens <= 1_048_576,
        "central output token ceiling",
    )?;
    positive(p.revision)?;
    let c = &p.ceilings;
    ensure(
        c.context_bytes > 0
            && c.context_bytes <= 1_048_576
            && c.wall_time_ms > 0
            && c.wall_time_ms <= 3_600_000
            && c.memory_bytes > 0
            && c.cpu_millis > 0
            && c.process_limit > 0,
        "central positive turn ceilings",
    )?;
    ensure(
        c.cost_limit_microunits.is_some() == c.currency.is_some(),
        "central cost currency pairing",
    )?;
    if let Some(currency) = &c.currency {
        ensure(
            currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()),
            "central currency",
        )?;
    }
    ensure(
        !p.allowed_environments.is_empty() && p.allowed_environments.len() <= 32,
        "central allowed environment bound",
    )?;
    let mut environments = BTreeSet::new();
    for e in &p.allowed_environments {
        reference(e)?;
        ensure(
            environments.insert((e.id.clone(), e.version.clone())),
            "central duplicate environment identity",
        )?;
    }
    let reg = &input.registry.value;
    let inst = &input.installation.value;
    positive(reg.revision)?;
    positive(inst.revision)?;
    id(inst.installation_id)?;
    id(inst.incarnation_id)?;
    window(reg.observed_at_ms, reg.expires_at_ms)?;
    window(inst.observed_at_ms, inst.expires_at_ms)?;
    ensure(
        reg.credentials.len() <= 32
            && reg.observations.len() <= 128
            && inst.harnesses.len() <= 16
            && inst.credential_bindings.len() <= 32,
        "central registry/installation bound",
    )?;
    let bindings: Vec<_> = inst
        .credential_bindings
        .iter()
        .map(|b| CredentialBinding {
            credential_revision: b.credential_revision,
            credential_profile: b.credential_profile.clone(),
            available: b.available,
            preparation: None,
            observed_at_ms: b.observed_at_ms,
            expires_at_ms: b.expires_at_ms,
        })
        .collect();
    crate::validate::validate_installation(
        &reg.credentials,
        &bindings,
        None,
        &inst.harnesses,
        &reg.observations,
        input.now_ms,
    )?;
    for c in &reg.credentials {
        reference(&c.profile)?;
        positive(c.revision)?;
        if let Some(o) = &c.material_owner {
            reference(o)?;
        }
    }
    for h in &inst.harnesses {
        for a in [&h.adapter, &h.environment, &h.qualification] {
            reference(a)?;
        }
        if let Some(a) = &h.failure_classifier {
            reference(a)?;
        }
    }
    for b in &inst.credential_bindings {
        reference(&b.credential_profile)?;
        window(b.observed_at_ms, b.expires_at_ms)?;
    }
    let mut combinations = BTreeSet::new();
    for b in &input.compositions {
        aq_profiles::central::verify_central(catalog, b).map_err(|_| {
            SelectionError::Invalid("central composition rederivation failed".into())
        })?;
        let c = &b.composition;
        ensure(
            combinations.insert((&c.harness, &c.environment)),
            "central duplicate composition combination",
        )?;
        ensure(
            same(&c.subject, &p.subject)?
                && c.catalog == p.catalog
                && c.compatibility == p.compatibility
                && same(&c.agent_profile, &p.agent_profile)?
                && same(&c.intelligence_profile, &p.intelligence_profile)?
                && same(&c.execution_profile, &p.execution_profile)?,
            "central profile/policy compatibility binding",
        )?;
        let aq_profiles::central::CentralProfileSubject::CentralConversation {
            actor_id,
            conversation_id,
            conversation_revision,
            policy,
        } = &c.subject;
        ensure(
            *actor_id == r.actor_id
                && *conversation_id == r.conversation_id
                && *conversation_revision == r.conversation_revision
                && *policy == p.composition_policy
                && b.policy.reference == p.composition_policy,
            "central subject/reservation/composition policy binding",
        )?;
        let request = aq_profiles::central::decode_request(b.request.utf8.as_bytes())
            .map_err(|_| SelectionError::Invalid("central original request".into()))?;
        let mail: Vec<_> = request
            .turn
            .messages
            .iter()
            .map(|m| ReservedMail {
                message_id: m.message_id,
                cursor: m.cursor,
                payload_digest: m.payload_digest.clone(),
            })
            .collect();
        ensure(
            request.turn.conversation_id == r.conversation_id
                && request.turn.through_cursor == r.through_cursor
                && mail == r.messages,
            "central original reserved mail binding",
        )?;
        ensure(
            c.startup_bytes == b.startup.utf8.len() as u64,
            "central rendered startup byte length",
        )?;
    }
    let name = p
        .agent_profile
        .identity
        .strip_prefix("agent:")
        .ok_or_else(|| SelectionError::Invalid("central agent source".into()))?;
    let defs = &catalog.artifact().catalog;
    let agent = defs
        .agents
        .iter()
        .find(|a| aq_profiles::key(&a.id, a.version) == name)
        .ok_or_else(|| SelectionError::Invalid("central agent absent".into()))?;
    let intelligence = defs
        .intelligence
        .iter()
        .find(|a| aq_profiles::key(&a.id, a.version) == agent.intelligence)
        .ok_or_else(|| SelectionError::Invalid("central intelligence absent".into()))?;
    let execution = defs
        .execution
        .iter()
        .find(|a| aq_profiles::key(&a.id, a.version) == agent.execution)
        .ok_or_else(|| SelectionError::Invalid("central execution absent".into()))?;
    if let Some(policy) = &intelligence.selection_policy {
        policy
            .validate()
            .map_err(|e| SelectionError::Invalid(e.to_string()))?;
    }
    Ok(Validated {
        intelligence,
        execution,
    })
}
