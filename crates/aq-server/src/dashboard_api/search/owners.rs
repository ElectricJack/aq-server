use super::*;

type Sample = (Vec<SearchHit>, Option<&'static str>);

impl SearchRead<'_> {
    fn require_owner(&self, mounted: bool) -> Result<()> {
        if self.auth.owner && mounted {
            Ok(())
        } else {
            Err(Error::Unavailable)
        }
    }

    pub(super) async fn collect(
        &self,
        kind: SearchType,
        projects: &[ProjectPin],
    ) -> Result<Sample> {
        match kind {
            SearchType::Task | SearchType::Knowledge => self.graph(kind, projects).await,
            SearchType::Session => self.sessions(projects).await,
            SearchType::Result => self.results(projects).await,
            SearchType::Playbook => self.playbooks(projects).await,
        }
    }
    async fn graph(&self, kind: SearchType, projects: &[ProjectPin]) -> Result<Sample> {
        let (state, runtime, user) = self.request.parts();
        let mut hits = vec![];
        let mut partial = projects.len() > 8;
        let needle = self.query.text.to_lowercase();
        self.require_owner(self.mounted.graph && state.agent_access.is_some())?;
        let graph = human::ReadRuntime::with_pool(runtime.pool.clone());
        for p in projects.iter().take(8) {
            let data = human::search_display(
                state,
                &graph,
                user,
                p.project.id,
                &self.query.text,
                name(kind),
            )
            .await?;
            partial |= data["next_cursor"].is_string();
            for item in data["items"].as_array().ok_or(Error::Unavailable)? {
                if let Some(hit) = graph_hit(kind, &p.project, item, &needle)? {
                    hits.push(hit);
                }
            }
        }
        Ok((hits, partial.then_some("bounded_owner_sample")))
    }
    async fn sessions(&self, projects: &[ProjectPin]) -> Result<Sample> {
        let scope = &self.query.scope;
        let needle = self.query.text.to_lowercase();
        let mut hits = vec![];
        let (ids, more) =
            service::membership(self.request, &self.auth, scope, Resource::Sessions).await?;
        let partial = projects.len() > 8 || more || ids.len() > 32;
        for id in ids.into_iter().take(32) {
            let s = service::session(self.request, id, scope, None)
                .await?
                .summary;
            let excerpt = format!("{} {}", s.authority, s.execution_authority);
            if matches(&needle, &s.label, &excerpt) {
                let revision = s.record_revision.ok_or(Error::Unavailable)?;
                hits.push(hit(
                    SearchRef {
                        kind: SearchType::Session,
                        id: id.to_string(),
                        revision: revision.into(),
                    },
                    &s.label,
                    &excerpt,
                    s.project,
                ));
            }
        }
        Ok((hits, partial.then_some("bounded_owner_sample")))
    }
    async fn results(&self, projects: &[ProjectPin]) -> Result<Sample> {
        let (state, _, user) = self.request.parts();
        let needle = self.query.text.to_lowercase();
        let mut hits = vec![];
        self.require_owner(self.mounted.results)?;
        let page = human_results::read_as(
            state,
            user,
            &human_results::Page {
                profile: Some(human_results::ReadProfile::RecordedNativeV2),
                project_id: self.query.scope.project(),
                after: None,
                limit: Some(25),
            },
            None,
        )
        .await?;
        // Owner coverage is retained native + legacy public metadata only.
        for r in page.items {
            if matches(&needle, &r.conclusion, &r.process_outcome) {
                let project = project_ref(projects, r.project_id);
                hits.push(hit(
                    SearchRef {
                        kind: SearchType::Result,
                        id: r.report_id.to_string(),
                        revision: digest(&r.revision)?,
                    },
                    &r.conclusion,
                    &r.process_outcome,
                    project,
                ));
            }
        }
        Ok((
            hits,
            Some(if page.next_after.is_some() {
                "retained_result_sample_truncated"
            } else {
                "retained_native_and_legacy_metadata_only"
            }),
        ))
    }
    async fn playbooks(&self, projects: &[ProjectPin]) -> Result<Sample> {
        let needle = self.query.text.to_lowercase();
        let mut hits = vec![];
        self.require_owner(self.mounted.playbook_loaded_definitions)?;
        let page = self
            .driver
            .ok_or(Error::Unavailable)?
            .inspect_loaded_definitions(
                self.request.user.id,
                DefinitionsQuery {
                    project_id: self.query.scope.project(),
                },
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        let project = project_ref(projects, page.namespace.project_id);
        for d in page.items {
            if matches(&needle, &d.name, &d.purpose) {
                hits.push(hit(
                    SearchRef {
                        kind: SearchType::Playbook,
                        id: d.name.clone(),
                        revision: digest(&d.source_digest)?,
                    },
                    &d.name,
                    &d.purpose,
                    project.clone(),
                ));
            }
        }
        Ok((hits, Some("loaded_startup_definitions_only")))
    }
}

fn project_ref(projects: &[ProjectPin], id: Uuid) -> ProjectRef {
    projects
        .iter()
        .find(|p| p.project.id == id)
        .map(|p| p.project.clone())
        .unwrap_or_else(|| ProjectRef {
            id,
            label: id.to_string(),
        })
}

fn graph_hit(
    kind: SearchType,
    project: &ProjectRef,
    item: &Value,
    needle: &str,
) -> Result<Option<SearchHit>> {
    // Enforce the display-field search boundary even if an
    // upstream owner returns a broader metadata-only match.
    let title = item["title"].as_str().ok_or(Error::Unavailable)?;
    let body = item["body"].as_str().unwrap_or("");
    let summary = if item["summary_of_revision"] == item["content_revision"] {
        item["summary"].as_str().unwrap_or("")
    } else {
        ""
    };
    if !matches(needle, title, body) && !matches(needle, "", summary) {
        return Ok(None);
    }
    let revision = item["content_revision"]
        .as_str()
        .ok_or(Error::Unavailable)?;
    Ok(Some(hit(
        SearchRef {
            kind,
            id: item["id"].as_str().ok_or(Error::Unavailable)?.into(),
            revision: Revision(revision.into()).into(),
        },
        title,
        if summary.is_empty() { body } else { summary },
        project.clone(),
    )))
}
