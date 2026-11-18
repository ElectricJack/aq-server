//! Trusted accounting catalog: derives an attempt's observation source from the
//! owner-configured intelligence registry. Policy, account scope, call namespace
//! and parser priority come from here, never from the worker.
use super::*;
use crate::intelligence::config::LoadedRegistry;
use aq_accounting::pricing::RateCardBook;
use aq_admission::observation_source;

/// The configured rate-card book; empty when no registry is configured.
pub fn rate_book(state: &AppState) -> RateCardBook {
    state
        .config
        .intelligence
        .as_ref()
        .and_then(|r| r.config.accounting.book().ok())
        .unwrap_or_default()
}

fn scoped(tag: &str, provider: &str, account: &str) -> Result<Digest> {
    Ok(Digest::of(
        &serde_json::to_vec(&json!([tag, provider, account])).map_err(|_| Error::Invalid)?,
    ))
}

/// Pure derivation at `generation`. `None` when the catalog does not cover the
/// selected (provider, harness) or the credential is unknown.
pub fn owner_source(
    registry: &LoadedRegistry,
    binding: Binding,
    selection: &aq_selection::ResolvedSelection,
    generation: u64,
) -> Result<Option<OwnerSource>> {
    let choice = &selection.choice;
    let Some(entry) = registry
        .config
        .accounting
        .source(&choice.provider, &choice.harness)
    else {
        return Ok(None);
    };
    let Some(credential) = registry
        .config
        .credentials
        .iter()
        .find(|c| c.credential.profile == choice.credential_profile)
    else {
        return Ok(None);
    };
    let billing = match credential.kind {
        aq_selection::CredentialKind::ApiKey => EstimateBasis::TokenRates,
        aq_selection::CredentialKind::SubscriptionLogin => EstimateBasis::SubscriptionAllocation,
    };
    let text = |s: &str| Text::new(s).map_err(|_| Error::Invalid);
    let attempt = binding.attempt_id;
    let source = OwnerSource {
        // Shared with the worker, which derives both from the delivered entry.
        source_id: observation_source::source_id(attempt, generation, &entry.reference)
            .map_err(|_| Error::Invalid)?,
        generation,
        native_session_key: observation_source::session_key(attempt, generation)
            .map_err(|_| Error::Invalid)?,
        account_claim: text(&choice.account_reference)?,
        policy: Policy {
            reference: entry.reference.clone(),
            // Stable across retries and wakes of the same account so replayed
            // history deduplicates by call key instead of recounting.
            account_scope: scoped(
                "aq.account-scope/v1",
                &choice.provider,
                &choice.account_reference,
            )?,
            call_namespace: scoped(
                "aq.call-namespace/v1",
                &choice.provider,
                &choice.account_reference,
            )?,
            provider: text(&choice.provider)?,
            layout: entry.layout.clone(),
            parsers: entry.parsers.clone(),
        },
        binding,
        billing: Some(billing),
    };
    source.validate()?;
    Ok(Some(source))
}

/// The attempt's newest unrevoked source, across nodes: (generation, digest).
async fn current_source(state: &AppState, attempt: Uuid) -> Result<Option<(u64, String)>> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT s.generation,s.authority_digest FROM observation_sources s \
         WHERE s.attempt_id=$1 AND NOT EXISTS(SELECT 1 FROM observation_source_revocations r \
          WHERE r.node_id=s.node_id AND r.source_id=s.source_id AND r.generation=s.generation) \
         ORDER BY s.generation DESC LIMIT 1",
    )
    .bind(attempt)
    .fetch_optional(&state.store.observation_pool)
    .await?;
    row.map(|(g, d)| Ok((u64::try_from(g).map_err(|_| Error::Invalid)?, d)))
        .transpose()
}

/// Loads the attempt's offer and resolved selection and derives the source the
/// catalog currently owns for it. That is the enrolled generation while the
/// derivation is unchanged; after a catalog or credential change, or a node
/// move, it is the next generation, which `register_source` enrolls while
/// revoking its predecessor.
pub async fn source_for_attempt(state: &AppState, attempt: Uuid) -> Result<Option<OwnerSource>> {
    let Some(registry) = state.config.intelligence.as_ref() else {
        return Ok(None);
    };
    let Some(row) = sqlx::query(
        "SELECT a.state,s.selection FROM execution_attempts a JOIN intelligence_attempt_selections s USING(attempt_id) WHERE a.attempt_id=$1 AND a.kind='profiled_v1'",
    )
    .bind(attempt)
    .fetch_optional(&state.store.observation_pool)
    .await?
    else {
        return Ok(None);
    };
    let profiled: crate::execution::profiled::ProfiledState = parse(row.get("state"))?;
    let selection: aq_selection::ResolvedSelection = parse(row.get("selection"))?;
    let binding =
        aq_admission::transport::lifecycle_binding(&profiled.offer).map_err(|_| Error::Invalid)?;
    let Some((generation, digest)) = current_source(state, attempt).await? else {
        return owner_source(registry, binding, &selection, 1);
    };
    let Some(same) = owner_source(registry, binding.clone(), &selection, generation)? else {
        return Ok(None);
    };
    if same.digest()?.as_str() == digest {
        return Ok(Some(same));
    }
    let next = generation.checked_add(1).ok_or(Error::Capacity)?;
    owner_source(registry, binding, &selection, next)
}

/// Production attempt path: registers the catalog-derived source. Idempotent;
/// `Ok(false)` when the catalog does not cover the attempt. `node`, when given,
/// must own the attempt's session.
pub async fn register_attempt(state: &AppState, attempt: Uuid, node: Option<Uuid>) -> Result<bool> {
    let Some(source) = source_for_attempt(state, attempt).await? else {
        return Ok(false);
    };
    if node.is_some_and(|n| node_of(&source.binding).ok() != Some(n)) {
        return Err(Error::Refused);
    }
    register_source(state, &source).await?;
    Ok(true)
}
