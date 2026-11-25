use super::*;

impl OverviewRead<'_> {
    pub(super) async fn ready(&self, mounted: bool) -> Result<Contribution> {
        let state = &self.request.state;
        let projects = &self.projects;
        let truncated = self.truncated;
        if !mounted {
            return Err(Error::Unavailable);
        }
        let graph = state.agent_access.as_ref().ok_or(Error::Unavailable)?;
        let mut c = Contribution::default();
        let mut n = 0;
        let mut more = truncated;
        for p in projects {
            if p.binding["graph_adapter_reference"] != "aq.graph.configured" {
                return Err(Error::Unavailable);
            }
            let workspace = p.binding["graph_workspace_id"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or(Error::Unavailable)?;
            let page = graph
                .scheduling_ready_current(workspace, 32, None, state.clock.as_ref())
                .await
                .map_err(|_| Error::Unavailable)?;
            let ids: Vec<_> = page
                .items
                .iter()
                .map(|item| item.revision.task_id)
                .collect();
            let eligible = aq_machine_control::holds::unclaimed_count(&state.store.pool, &ids)
                .await
                .map_err(|_| Error::Unavailable)?;
            n += eligible;
            more |= page.next_cursor.is_some();
            let mut s = source(
                "graph_ready",
                Some(p.label.clone()),
                SourceSample {
                    at: page.stamp.received_at_ms,
                    count: eligible,
                    truncated: page.next_cursor.is_some() || truncated,
                },
                None,
            )?;
            if let Some(o) = &mut s.observation {
                o.fresh_until = timestamp(page.stamp.expires_at_ms)?;
            }
            c.sources.push(s);
        }
        self.empty_source(&mut c, "graph_ready", None)?;
        c.ready = Some(total(n, more));
        Ok(c)
    }
}
