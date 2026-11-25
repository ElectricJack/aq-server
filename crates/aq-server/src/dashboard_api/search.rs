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
fn hit(reference: SearchRef, title: &str, excerpt: &str, project: ProjectRef) -> SearchHit {
    SearchHit {
        r#ref: reference,
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
    let lowercase_hex = value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if value.len() != 64 || !lowercase_hex {
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

mod owners;

struct SearchQuery {
    text: String,
    types: Vec<SearchType>,
    scope: Scope,
    limit: u16,
    cursor: Option<String>,
}

impl QueryParams {
    fn parse(self) -> Result<SearchQuery> {
        let text = self.q.trim();
        let has_search_text = !text.is_empty() && !self.q.chars().any(char::is_control);
        if self.q.len() > 256 || !has_search_text {
            return Err(Error::Invalid);
        }
        let mut types = match self.types {
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
            scope: self.scope,
            limit: self.limit,
            cursor: self.cursor,
        };
        Ok(SearchQuery {
            text: text.to_owned(),
            types,
            scope: page.scope()?,
            limit: page.limit()?,
            cursor: page.cursor,
        })
    }
}

struct SearchRead<'a> {
    request: &'a DashboardRequest,
    query: &'a SearchQuery,
    auth: service::Auth,
    mounted: MountedOwners,
    driver: Option<&'a Driver>,
    now: u64,
}

pub(super) async fn read(
    request: DashboardRequest,
    owners: Option<Extension<MountedOwners>>,
    driver: Option<Extension<Option<Arc<Driver>>>>,
    query: std::result::Result<Query<QueryParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope<Search>>> {
    let Query(params) = query.map_err(|_| Error::Invalid)?;
    let query = params.parse()?;
    request
        .reply(4, || {
            Ok(async {
                let (state, runtime, user) = request.parts();
                let auth = service::auth(state, runtime, user).await?;
                let read = SearchRead {
                    request: &request,
                    query: &query,
                    auth,
                    now: service::stamp(state)?,
                    mounted: owners.map(|v| v.0).unwrap_or_default(),
                    driver: driver.as_ref().and_then(|v| v.0.as_deref()),
                };
                read.page().await
            })
        })
        .await
}

impl SearchRead<'_> {
    async fn snapshot(&self) -> Result<(Snapshot, usize)> {
        let (_, runtime, user) = self.request.parts();
        let query = self.query;
        let filters = json!({"q":query.text,"types":query.types});
        if let Some(cursor) = &query.cursor {
            return runtime.resume(
                cursor,
                runtime::PageKey {
                    actor: user.id,
                    partition: &self.auth.partition,
                    scope: &query.scope,
                    resource: Resource::Search,
                    limit: query.limit,
                    filters: &filters,
                },
                self.now,
            );
        }
        let projects = if self.auth.owner {
            projects(runtime, &query.scope).await?
        } else {
            vec![]
        };
        let mut data = SnapshotData {
            hits: vec![],
            sources: vec![],
            projects,
        };
        for kind in TYPES {
            self.sample(kind, &mut data).await?;
        }
        data.hits.sort_by(|a, b| {
            (a.r#ref.kind, a.project.id, &a.r#ref.id).cmp(&(
                b.r#ref.kind,
                b.project.id,
                &b.r#ref.id,
            ))
        });
        let snapshot = Snapshot {
            id: Uuid::nil(),
            actor: user.id,
            partition: self.auth.partition.clone(),
            scope: query.scope.clone(),
            resource: Resource::Search,
            ids: vec![],
            filters,
            search: Some(data),
            truncated: false,
            created: self.now,
            expires: self
                .now
                .checked_add(runtime::TTL)
                .ok_or(Error::Unavailable)?,
        };
        Ok((runtime.insert(snapshot, self.now)?, 0))
    }

    async fn sample(&self, kind: SearchType, data: &mut SnapshotData) -> Result<()> {
        if !self.query.types.contains(&kind) {
            data.sources
                .push(source(kind, 0, Some("group_not_requested"), self.now)?);
            return Ok(());
        }
        let result = tokio::time::timeout(BUDGET, self.collect(kind, &data.projects)).await;
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
        data.sources
            .push(source(kind, hits.len(), reason, self.now)?);
        data.hits.extend(hits);
        Ok(())
    }

    async fn page(&self) -> Result<Envelope<Search>> {
        let (state, runtime, user) = self.request.parts();
        let (snapshot, offset) = self.snapshot().await?;
        let data = snapshot.search.as_ref().ok_or(Error::Restart)?;
        fence_projects(runtime, &data.projects).await?;
        self.fence_definitions(data).await?;
        let groups = self.groups(&snapshot, data, offset)?;
        self.fence_sessions(&groups).await?;
        service::fence(state, runtime, user, &self.auth).await?;
        fence_projects(runtime, &data.projects).await?;
        if service::stamp(state)? >= snapshot.expires {
            return Err(Error::Restart);
        }
        let partial = data
            .sources
            .iter()
            .any(|s| !matches!(s.coverage.state, Completeness::Complete));
        service::envelope(
            state,
            self.query.scope.clone(),
            Search {
                groups,
                sources: data.sources.clone(),
                snapshot_id: snapshot.id,
            },
            partial.then_some("See per-source search coverage"),
        )
    }

    // Loaded definitions also have a namespace-specific inspection owner. Check it on replay.
    async fn fence_definitions(&self, data: &SnapshotData) -> Result<()> {
        if data
            .hits
            .iter()
            .any(|h| h.r#ref.kind == SearchType::Playbook)
        {
            self.driver
                .ok_or(Error::AuthorizationChanged)?
                .inspect_loaded_definitions(
                    self.request.user.id,
                    DefinitionsQuery {
                        project_id: self.query.scope.project(),
                    },
                )
                .await
                .map_err(|_| Error::AuthorizationChanged)?;
        }
        Ok(())
    }
    fn groups(
        &self,
        snapshot: &Snapshot,
        data: &SnapshotData,
        offset: usize,
    ) -> Result<Vec<SearchGroup>> {
        let limit = self.query.limit;
        let mut groups = vec![];
        for kind in TYPES {
            let hits: Vec<_> = data.hits.iter().filter(|h| h.r#ref.kind == kind).collect();
            let end = (offset + usize::from(limit)).min(hits.len());
            let next_cursor = if end < hits.len() {
                Some(self.request.runtime.next(snapshot, end, limit)?)
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
        Ok(groups)
    }
    async fn fence_sessions(&self, groups: &[SearchGroup]) -> Result<()> {
        for hit in groups
            .iter()
            .flat_map(|g| &g.hits)
            .filter(|h| h.r#ref.kind == SearchType::Session)
        {
            let id = hit.r#ref.id.parse().map_err(|_| Error::Restart)?;
            service::session(self.request, id, &self.query.scope, None)
                .await
                .map_err(|_| Error::AuthorizationChanged)?;
        }
        Ok(())
    }
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
