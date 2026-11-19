//! Bounded, display-field-only searches over existing owners. Cached pages are
//! immutable observations, fenced against current ACLs and project bindings.
use super::{
    contract::*,
    runtime::{self, Resource, Runtime, Snapshot},
    service, *,
};
use crate::{
    agent_access::human,
    playbooks::{driver::Driver, inspection::definitions::DefinitionsQuery},
    profiled_execution::human_results,
    store::User,
};
use serde_json::{Value, json};
use sqlx::Row;

const TYPES: [SearchType; 5] = [
    SearchType::Task,
    SearchType::Session,
    SearchType::Result,
    SearchType::Playbook,
    SearchType::Knowledge,
];
const BUDGET: Duration = Duration::from_millis(650);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QueryParams {
    q: String,
    scope: Option<String>,
    limit: Option<u16>,
    cursor: Option<String>,
    types: Option<String>,
}
#[derive(Clone, Debug)]
struct ProjectPin {
    project: ProjectRef,
    revision: i64,
    binding: Value,
}
#[derive(Clone, Debug)]
pub(super) struct SnapshotData {
    pub(super) hits: Vec<SearchHit>,
    sources: Vec<SourceObservation>,
    projects: Vec<ProjectPin>,
}
fn name(kind: SearchType) -> &'static str {
    match kind {
        SearchType::Task => "task",
        SearchType::Session => "session",
        SearchType::Result => "result",
        SearchType::Playbook => "playbook",
        SearchType::Knowledge => "knowledge",
    }
}
fn display(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
}
fn hit(
    kind: SearchType,
    id: String,
    revision: ReferenceRevision,
    title: &str,
    excerpt: &str,
    project: ProjectRef,
) -> SearchHit {
    SearchHit {
        r#ref: SearchRef { kind, id, revision },
        title: display(title, 256),
        excerpt: display(excerpt, 512),
        project,
    }
}
fn matches(q: &str, title: &str, excerpt: &str) -> bool {
    title.to_lowercase().contains(q) || excerpt.to_lowercase().contains(q)
}
fn digest(raw: &str) -> Result<ReferenceRevision> {
    let value = raw.strip_prefix("sha256:").unwrap_or(raw);
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Unavailable);
    }
    Ok(ReferenceRevision::Digest(Digest(format!("sha256:{value}"))))
}
fn source(
    kind: SearchType,
    count: usize,
    reason: Option<&str>,
    now: u64,
) -> Result<SourceObservation> {
    let coverage = if reason.is_some() {
        Completeness::Partial
    } else {
        Completeness::Complete
    };
    Ok(SourceObservation {
        source: name(kind).into(),
        project: None,
        observation: if matches!(
            reason,
            Some("owner_unavailable_or_not_visible" | "group_not_requested")
        ) {
            None
        } else {
            Some(Observation {
                observed_at: service::timestamp(now)?,
                fresh_until: service::timestamp(now.saturating_add(runtime::TTL))?,
                source: name(kind).into(),
                quality: Quality::Reported,
                coverage: coverage.clone(),
            })
        },
        coverage: Coverage {
            state: coverage,
            reason: reason.map(str::to_owned),
        },
        total: if reason.is_some() {
            Total::unknown()
        } else {
            Total {
                value: Some(count as u32),
                accuracy: Accuracy::Exact,
            }
        },
    })
}
pub(super) async fn read(
    State(state): State<AppState>,
    DashboardUser(user): DashboardUser,
    Extension(runtime): Extension<Arc<Runtime>>,
    owners: Option<Extension<MountedOwners>>,
    driver: Option<Extension<Option<Arc<Driver>>>>,
    query: std::result::Result<Query<QueryParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope<Search>>> {
    let Query(query) = query.map_err(|_| Error::Invalid)?;
    if query.q.len() > 256 || query.q.chars().any(char::is_control) || query.q.trim().is_empty() {
        return Err(Error::Invalid);
    }
    let q = query.q.trim().to_owned();
    let mut types = match query.types.as_deref() {
        None => TYPES.to_vec(),
        Some(value) => value
            .split(',')
            .map(|s| {
                TYPES
                    .iter()
                    .copied()
                    .find(|k| name(*k) == s)
                    .ok_or(Error::Invalid)
            })
            .collect::<Result<Vec<_>>>()?,
    };
    types.sort();
    types.dedup();
    let page = ReadQuery {
        scope: query.scope,
        limit: query.limit,
        cursor: query.cursor,
    };
    let scope = page.scope()?;
    let limit = page.limit()?;
    let filters = json!({"q":q,"types":types});
    let _permit = runtime.gate.try_acquire().map_err(|_| Error::Unavailable)?;
    let mounted = owners.map(|v| v.0).unwrap_or_default();
    let driver = driver.and_then(|v| v.0);
    bounded(
        tokio::time::timeout(Duration::from_secs(4), async {
            let a = service::auth(&state, &runtime, &user).await?;
            let now = service::stamp(&state)?;
            let (snapshot, offset) = if let Some(cursor) = page.cursor {
                runtime.resume_filtered(
                    &cursor,
                    user.id,
                    &a.partition,
                    (&scope, Resource::Search, &filters),
                    limit,
                    now,
                )?
            } else {
                let projects = if a.owner {
                    projects(&runtime, &scope).await?
                } else {
                    vec![]
                };
                let mut data = SnapshotData {
                    hits: vec![],
                    sources: vec![],
                    projects,
                };
                for kind in TYPES {
                    if !types.contains(&kind) {
                        data.sources
                            .push(source(kind, 0, Some("group_not_requested"), now)?);
                        continue;
                    }
                    let result = tokio::time::timeout(
                        BUDGET,
                        collect(
                            &state,
                            &runtime,
                            &user,
                            &a,
                            &scope,
                            &data.projects,
                            &mounted,
                            driver.as_deref(),
                            kind,
                            &q,
                        ),
                    )
                    .await;
                    let (mut hits, reason) = match result {
                        Ok(Ok(value)) => value,
                        _ => (vec![], Some("owner_unavailable_or_not_visible")),
                    };
                    let remaining = (runtime::MEMBERS - data.hits.len()).min(48);
                    let reason = if hits.len() > remaining {
                        Some("snapshot_sample_truncated")
                    } else {
                        reason
                    };
                    hits.truncate(remaining);
                    data.sources.push(source(kind, hits.len(), reason, now)?);
                    data.hits.extend(hits);
                }
                data.hits.sort_by(|a, b| {
                    (a.r#ref.kind, a.project.id, &a.r#ref.id).cmp(&(
                        b.r#ref.kind,
                        b.project.id,
                        &b.r#ref.id,
                    ))
                });
                let s = Snapshot {
                    id: Uuid::nil(),
                    actor: user.id,
                    partition: a.partition.clone(),
                    scope: scope.clone(),
                    resource: Resource::Search,
                    ids: vec![],
                    filters,
                    search: Some(data),
                    truncated: false,
                    created: now,
                    expires: now.checked_add(runtime::TTL).ok_or(Error::Unavailable)?,
                };
                (runtime.insert(s, now)?, 0)
            };
            let data = snapshot.search.as_ref().ok_or(Error::Restart)?;
            fence_projects(&runtime, &data.projects).await?;
            // Loaded definitions have a namespace-specific inspection owner in addition
            // to the dashboard principal partition. Recheck that owner on replay.
            if data
                .hits
                .iter()
                .any(|h| h.r#ref.kind == SearchType::Playbook)
            {
                driver
                    .as_ref()
                    .ok_or(Error::AuthorizationChanged)?
                    .inspect_loaded_definitions(
                        user.id,
                        DefinitionsQuery {
                            project_id: scope.project(),
                        },
                    )
                    .await
                    .map_err(|_| Error::AuthorizationChanged)?;
            }
            let mut groups = vec![];
            for kind in TYPES {
                let hits: Vec<_> = data.hits.iter().filter(|h| h.r#ref.kind == kind).collect();
                let end = (offset + usize::from(limit)).min(hits.len());
                let next_cursor = if end < hits.len() {
                    Some(runtime.next(&snapshot, end, limit)?)
                } else {
                    None
                };
                groups.push(SearchGroup {
                    r#type: kind,
                    hits: hits
                        .into_iter()
                        .skip(offset)
                        .take(usize::from(limit))
                        .cloned()
                        .collect(),
                    has_more: next_cursor.is_some(),
                    next_cursor,
                });
            }
            for group in &groups {
                for hit in &group.hits {
                    if hit.r#ref.kind == SearchType::Session {
                        let id = hit.r#ref.id.parse().map_err(|_| Error::Restart)?;
                        service::session(&state, &runtime, &user, id, &scope)
                            .await
                            .map_err(|_| Error::AuthorizationChanged)?;
                    }
                }
            }
            service::fence(&state, &runtime, &user, &a).await?;
            fence_projects(&runtime, &data.projects).await?;
            if service::stamp(&state)? >= snapshot.expires {
                return Err(Error::Restart);
            }
            let partial = data
                .sources
                .iter()
                .any(|s| !matches!(s.coverage.state, Completeness::Complete));
            service::envelope(
                &state,
                scope,
                Search {
                    groups,
                    sources: data.sources.clone(),
                    snapshot_id: snapshot.id,
                },
                partial.then_some("See per-source search coverage"),
            )
        })
        .await
        .map_err(|_| Error::Unavailable)??,
    )
}
async fn projects(runtime: &Runtime, scope: &Scope) -> Result<Vec<ProjectPin>> {
    let mut tx = service::tx(runtime).await?;
    let rows = sqlx::query("SELECT project_id,revision,binding FROM project_execution_bindings WHERE ($1::uuid IS NULL OR project_id=$1) ORDER BY project_id LIMIT 9").bind(scope.project()).fetch_all(&mut *tx).await?;
    let result = rows
        .into_iter()
        .map(|r| {
            let id = r.get("project_id");
            let binding: Value = r.get("binding");
            ProjectPin {
                project: ProjectRef {
                    id,
                    label: display(binding["name"].as_str().unwrap_or("Project"), 256),
                },
                revision: r.get("revision"),
                binding,
            }
        })
        .collect();
    tx.commit().await?;
    Ok(result)
}
async fn fence_projects(runtime: &Runtime, expected: &[ProjectPin]) -> Result<()> {
    let mut tx = service::tx(runtime).await?;
    let ids: Vec<_> = expected.iter().map(|p| p.project.id).collect();
    let rows = sqlx::query("SELECT project_id,revision,binding FROM project_execution_bindings WHERE project_id=ANY($1) ORDER BY project_id").bind(ids).fetch_all(&mut *tx).await?;
    if rows.len() != expected.len()
        || rows.iter().any(|r| {
            expected
                .iter()
                .find(|p| p.project.id == r.get::<Uuid, _>("project_id"))
                .is_none_or(|p| {
                    p.revision != r.get::<i64, _>("revision")
                        || p.binding != r.get::<Value, _>("binding")
                })
        })
    {
        return Err(Error::AuthorizationChanged);
    }
    tx.commit().await?;
    Ok(())
}
// One bounded observation per owner; unknown coverage never becomes empty success.
#[allow(clippy::too_many_arguments)]
async fn collect(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    auth: &service::Auth,
    scope: &Scope,
    projects: &[ProjectPin],
    mounted: &MountedOwners,
    driver: Option<&Driver>,
    kind: SearchType,
    q: &str,
) -> Result<(Vec<SearchHit>, Option<&'static str>)> {
    let mut hits = vec![];
    let mut partial = projects.len() > 8;
    let needle = q.to_lowercase();
    match kind {
        SearchType::Task | SearchType::Knowledge => {
            if !auth.owner || !mounted.graph || state.agent_access.is_none() {
                return Err(Error::Unavailable);
            }
            let graph = human::ReadRuntime::with_pool(runtime.pool.clone());
            for p in projects.iter().take(8) {
                let data =
                    human::search_display(state, &graph, user, p.project.id, q, name(kind)).await?;
                partial |= data["next_cursor"].is_string();
                for item in data["items"].as_array().ok_or(Error::Unavailable)? {
                    // Enforce the display-field search boundary even if an
                    // upstream owner returns a broader metadata-only match.
                    let title = item["title"].as_str().ok_or(Error::Unavailable)?;
                    let body = item["body"].as_str().unwrap_or("");
                    let summary = if item["summary_of_revision"] == item["content_revision"] {
                        item["summary"].as_str().unwrap_or("")
                    } else {
                        ""
                    };
                    if !matches(&needle, title, body) && !matches(&needle, "", summary) {
                        continue;
                    }
                    let revision = item["content_revision"]
                        .as_str()
                        .ok_or(Error::Unavailable)?;
                    hits.push(hit(
                        kind,
                        item["id"].as_str().ok_or(Error::Unavailable)?.into(),
                        Revision(revision.into()).into(),
                        item["title"].as_str().ok_or(Error::Unavailable)?,
                        if summary.is_empty() { body } else { summary },
                        p.project.clone(),
                    ));
                }
            }
        }
        SearchType::Session => {
            let (ids, more) =
                service::membership(state, runtime, auth, scope, Resource::Sessions).await?;
            partial |= more || ids.len() > 32;
            for id in ids.into_iter().take(32) {
                let s = service::session(state, runtime, user, id, scope)
                    .await?
                    .summary;
                let excerpt = format!("{} {}", s.authority, s.execution_authority);
                if matches(&needle, &s.label, &excerpt) {
                    let revision = s.record_revision.ok_or(Error::Unavailable)?;
                    hits.push(hit(
                        kind,
                        id.to_string(),
                        revision.into(),
                        &s.label,
                        &excerpt,
                        s.project,
                    ));
                }
            }
        }
        SearchType::Result => {
            if !auth.owner || !mounted.results {
                return Err(Error::Unavailable);
            }
            let page = human_results::read_as(
                state,
                user,
                &human_results::Page {
                    profile: Some(human_results::ReadProfile::RecordedNativeV2),
                    project_id: scope.project(),
                    after: None,
                    limit: Some(25),
                },
                None,
            )
            .await?;
            // Owner coverage is retained native + legacy public metadata only.
            for r in page.items {
                if matches(&needle, &r.conclusion, &r.process_outcome) {
                    let project = projects
                        .iter()
                        .find(|p| p.project.id == r.project_id)
                        .map(|p| p.project.clone())
                        .unwrap_or(ProjectRef {
                            id: r.project_id,
                            label: r.project_id.to_string(),
                        });
                    hits.push(hit(
                        kind,
                        r.report_id.to_string(),
                        digest(&r.revision)?,
                        &r.conclusion,
                        &r.process_outcome,
                        project,
                    ));
                }
            }
            return Ok((
                hits,
                Some(if page.next_after.is_some() {
                    "retained_result_sample_truncated"
                } else {
                    "retained_native_and_legacy_metadata_only"
                }),
            ));
        }
        SearchType::Playbook => {
            if !auth.owner || !mounted.playbook_loaded_definitions {
                return Err(Error::Unavailable);
            }
            let page = driver
                .ok_or(Error::Unavailable)?
                .inspect_loaded_definitions(
                    user.id,
                    DefinitionsQuery {
                        project_id: scope.project(),
                    },
                )
                .await
                .map_err(|_| Error::Unavailable)?;
            let project = projects
                .iter()
                .find(|p| p.project.id == page.namespace.project_id)
                .map(|p| p.project.clone())
                .unwrap_or(ProjectRef {
                    id: page.namespace.project_id,
                    label: page.namespace.project_id.to_string(),
                });
            for d in page.items {
                if matches(&needle, &d.name, &d.purpose) {
                    hits.push(hit(
                        kind,
                        d.name.clone(),
                        digest(&d.source_digest)?,
                        &d.name,
                        &d.purpose,
                        project.clone(),
                    ));
                }
            }
            return Ok((hits, Some("loaded_startup_definitions_only")));
        }
    }
    Ok((hits, partial.then_some("bounded_owner_sample")))
}
