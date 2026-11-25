//! Fixed public graph observations for future scheduling. These reads never
//! claim a task, reserve capacity, mutate graph state or grant execution.
pub(super) mod parse;
use super::{Runtime, contract};
use aq_admission::TaskRevision;
pub use aq_profiled_state::native_offer::{GraphReadStamp, ReadyPage, ReadyTask, TaskContext};
use aq_profiled_state::native_offer::{bounded_cursor, graph_ready_page, graph_task_revision};
use serde_json::Value;
use uuid::Uuid;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchedulingReadError {
    #[error("invalid bounded scheduling read")]
    InvalidRequest,
    #[error("graph resource not found")]
    NotFound,
    #[error("graph access forbidden")]
    Forbidden,
    #[error("graph observation unavailable")]
    Unavailable,
    #[error("invalid graph response")]
    InvalidResponse,
    #[error("graph task revision changed")]
    StaleTask,
}
type Result<T> = std::result::Result<T, SchedulingReadError>;
fn now() -> Result<u64> {
    chrono::Utc::now()
        .timestamp_millis()
        .try_into()
        .map_err(|_| SchedulingReadError::Unavailable)
}
// Validate the wire schema before decoding its scheduling fields.
fn ready_page(
    workspace: Uuid,
    limit: u16,
    cursor: Option<&str>,
    value: Value,
    stamp: GraphReadStamp,
) -> Result<ReadyPage> {
    if !contract::upstream_valid("NodePage", &value) {
        return Err(SchedulingReadError::InvalidResponse);
    }
    graph_ready_page(workspace, limit, cursor, &value, stamp)
        .map_err(|_| SchedulingReadError::InvalidResponse)
}

impl Runtime {
    async fn scheduling_get_clock(
        &self,
        path: &str,
        query: &[(String, String)],
        max_bytes: usize,
        clock: &(dyn Fn() -> Result<u64> + Sync),
    ) -> Result<(Value, GraphReadStamp, Vec<u8>)> {
        let requested = clock()?;
        let mut response = self
            .client
            .get(
                self.url
                    .join(path)
                    .map_err(|_| SchedulingReadError::InvalidRequest)?,
            )
            .bearer_auth(&self.token)
            .header("X-AQ-Graph-API-Version", "1")
            .query(query)
            .send()
            .await
            .map_err(|_| SchedulingReadError::Unavailable)?;
        if response
            .headers()
            .get("X-AQ-Graph-API-Version")
            .is_none_or(|v| v != "1")
        {
            return Err(SchedulingReadError::InvalidResponse);
        }
        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err(SchedulingReadError::Forbidden),
            404 => return Err(SchedulingReadError::NotFound),
            _ => return Err(SchedulingReadError::Unavailable),
        }
        if response
            .content_length()
            .is_some_and(|n| n > max_bytes as u64)
        {
            return Err(SchedulingReadError::InvalidResponse);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| SchedulingReadError::Unavailable)?
        {
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|n| n > max_bytes)
            {
                return Err(SchedulingReadError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        let received = clock()?;
        let expires = requested
            .checked_add(30_000)
            .ok_or(SchedulingReadError::Unavailable)?;
        if received < requested || received >= expires {
            return Err(SchedulingReadError::Unavailable);
        }
        let value = parse::parse(&bytes).map_err(|_| SchedulingReadError::InvalidResponse)?;
        use sha2::Digest as _;
        let response_digest = hex::encode(sha2::Sha256::digest(&bytes));
        Ok((
            value,
            GraphReadStamp {
                requested_at_ms: requested,
                received_at_ms: received,
                expires_at_ms: expires,
                response_digest,
            },
            bytes,
        ))
    }
    pub async fn scheduling_ready(
        &self,
        workspace: Uuid,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<ReadyPage> {
        self.scheduling_ready_clock(workspace, limit, cursor, &now)
            .await
    }
    /// Trusted daemon clock around a genuinely new ready request/body receive,
    /// parallel to the current-context adapter. No wire-supplied clock or saved
    /// response can mint an observation through this method.
    pub(crate) async fn scheduling_ready_current(
        &self,
        workspace: Uuid,
        limit: u16,
        cursor: Option<&str>,
        clock: &(dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync),
    ) -> Result<ReadyPage> {
        let observed_now = || {
            u64::try_from(clock().timestamp_millis())
                .ok()
                .filter(|n| *n > 0)
                .ok_or(SchedulingReadError::Unavailable)
        };
        self.scheduling_ready_clock(workspace, limit, cursor, &observed_now)
            .await
    }

    pub(crate) async fn scheduling_task_ready_current(
        &self,
        task: &TaskRevision,
        clock: &(dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync),
    ) -> Result<ReadyPage> {
        let workspace = task.workspace_id;
        let id = task.task_id;
        let observed_now = || {
            u64::try_from(clock().timestamp_millis())
                .ok()
                .filter(|n| *n > 0)
                .ok_or(SchedulingReadError::Unavailable)
        };
        let (value, stamp, _) = match self
            .scheduling_get_clock(
                &format!("/v1/workspaces/{workspace}/ready/{id}"),
                &[],
                1_048_576,
                &observed_now,
            )
            .await
        {
            // Older Graph deployments only serve the bounded list endpoint.
            Err(SchedulingReadError::NotFound) => {
                return self
                    .scheduling_ready_current(workspace, 8, None, clock)
                    .await;
            }
            answer => answer?,
        };
        let ready = ready_page(workspace, 1, None, value, stamp)?;
        if ready.next_cursor.is_some() || ready.items.iter().any(|r| r.revision.task_id != id) {
            return Err(SchedulingReadError::InvalidResponse);
        }
        Ok(ready)
    }
    async fn scheduling_ready_clock(
        &self,
        workspace: Uuid,
        limit: u16,
        cursor: Option<&str>,
        clock: &(dyn Fn() -> Result<u64> + Sync),
    ) -> Result<ReadyPage> {
        if workspace.is_nil()
            || !(1..=64).contains(&limit)
            || cursor.is_some_and(|c| !bounded_cursor(c))
        {
            return Err(SchedulingReadError::InvalidRequest);
        }
        let mut query = vec![("limit".into(), limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor".into(), cursor.into()));
        }
        let (value, stamp, _bytes) = self
            .scheduling_get_clock(
                &format!("/v1/workspaces/{workspace}/ready"),
                &query,
                1_048_576,
                clock,
            )
            .await?;
        ready_page(workspace, limit, cursor, value, stamp)
    }
    pub async fn scheduling_context(
        &self,
        expected: &TaskRevision,
        depth: u8,
        limit: u16,
        max_bytes: u32,
    ) -> Result<TaskContext> {
        self.scheduling_context_clock(expected, depth, limit, max_bytes, &now)
            .await
    }
    /// Trusted daemon clock around a genuinely new request/body receive. This
    /// does not retime a saved graph response or accept a clock from HTTP input.
    pub(crate) async fn scheduling_context_current(
        &self,
        expected: &TaskRevision,
        depth: u8,
        limit: u16,
        max_bytes: u32,
        clock: &(dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync),
    ) -> Result<TaskContext> {
        let observed_now = || {
            u64::try_from(clock().timestamp_millis())
                .ok()
                .filter(|n| *n > 0)
                .ok_or(SchedulingReadError::Unavailable)
        };
        self.scheduling_context_clock(expected, depth, limit, max_bytes, &observed_now)
            .await
    }
    async fn scheduling_context_clock(
        &self,
        expected: &TaskRevision,
        depth: u8,
        limit: u16,
        max_bytes: u32,
        clock: &(dyn Fn() -> Result<u64> + Sync),
    ) -> Result<TaskContext> {
        expected
            .validate()
            .map_err(|_| SchedulingReadError::InvalidRequest)?;
        if depth > 5 || !(1..=128).contains(&limit) || !(1024..=1_048_576).contains(&max_bytes) {
            return Err(SchedulingReadError::InvalidRequest);
        }
        let query = vec![
            ("depth".into(), depth.to_string()),
            ("limit".into(), limit.to_string()),
            ("max_bytes".into(), max_bytes.to_string()),
            ("direction".into(), "both".into()),
        ];
        let (value, stamp, bytes) = self
            .scheduling_get_clock(
                &format!(
                    "/v1/workspaces/{}/nodes/{}/context",
                    expected.workspace_id, expected.task_id
                ),
                &query,
                max_bytes as usize,
                clock,
            )
            .await?;
        if !contract::upstream_valid("Context", &value) {
            return Err(SchedulingReadError::InvalidResponse);
        }
        let revision = graph_task_revision(&value["node"], expected.workspace_id)
            .map_err(|_| SchedulingReadError::InvalidResponse)?;
        if revision != *expected
            || value["node"]["task_state"] != "active"
            || value["node"]["retired"] != false
            || value["node"]["deleted"] != false
        {
            return Err(SchedulingReadError::StaleTask);
        }
        let nodes = value["nodes"]
            .as_array()
            .ok_or(SchedulingReadError::InvalidResponse)?;
        let edges = value["edges"]
            .as_array()
            .ok_or(SchedulingReadError::InvalidResponse)?;
        let pins = value["pinned_revisions"]
            .as_array()
            .ok_or(SchedulingReadError::InvalidResponse)?;
        if nodes
            .len()
            .checked_add(1)
            .is_none_or(|n| n > limit as usize)
            || edges.len() > 4 * limit as usize
            || pins.len() > 2 * edges.len()
        {
            return Err(SchedulingReadError::InvalidResponse);
        }
        let mut node_ids = std::collections::BTreeSet::from([expected.task_id]);
        let mut actual_pins = std::collections::BTreeSet::new();
        for (key, records) in [("nodes", nodes), ("pinned_revisions", pins)] {
            for record in records {
                let record_id = record["id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .filter(|id| !id.is_nil())
                    .ok_or(SchedulingReadError::InvalidResponse)?;
                if record["workspace_id"] != expected.workspace_id.to_string()
                    || ["version", "content_revision"].iter().any(|key| {
                        record[key]
                            .as_u64()
                            .is_none_or(|v| v == 0 || v > i64::MAX as u64)
                    })
                {
                    return Err(SchedulingReadError::InvalidResponse);
                }
                if key == "nodes" {
                    if !node_ids.insert(record_id) {
                        return Err(SchedulingReadError::InvalidResponse);
                    }
                } else if !actual_pins.insert((
                    record_id,
                    record["content_revision"]
                        .as_u64()
                        .ok_or(SchedulingReadError::InvalidResponse)?,
                )) {
                    return Err(SchedulingReadError::InvalidResponse);
                }
            }
        }
        let mut edge_ids = std::collections::BTreeSet::new();
        let mut expected_pins = std::collections::BTreeSet::new();
        for edge in edges {
            let edge_id = edge["id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .filter(|id| !id.is_nil())
                .ok_or(SchedulingReadError::InvalidResponse)?;
            if !edge_ids.insert(edge_id)
                || edge["workspace_id"] != expected.workspace_id.to_string()
                || edge["deleted"] != false
                || edge["version"]
                    .as_u64()
                    .is_none_or(|v| v == 0 || v > i64::MAX as u64)
            {
                return Err(SchedulingReadError::InvalidResponse);
            }
            for (endpoint, pin) in [("source", "source_revision"), ("target", "target_revision")] {
                let endpoint = edge[endpoint]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .filter(|id| !id.is_nil())
                    .ok_or(SchedulingReadError::InvalidResponse)?;
                if !node_ids.contains(&endpoint) {
                    return Err(SchedulingReadError::InvalidResponse);
                }
                if !edge[pin].is_null() {
                    let revision = edge[pin]
                        .as_u64()
                        .filter(|v| *v > 0 && *v <= i64::MAX as u64)
                        .ok_or(SchedulingReadError::InvalidResponse)?;
                    expected_pins.insert((endpoint, revision));
                }
            }
        }
        if actual_pins != expected_pins {
            return Err(SchedulingReadError::InvalidResponse);
        }
        let digest = stamp.response_digest.clone();
        Ok(TaskContext {
            revision,
            truncated: value["truncated"]
                .as_bool()
                .ok_or(SchedulingReadError::InvalidResponse)?,
            context: value,
            raw_json: String::from_utf8(bytes).map_err(|_| SchedulingReadError::InvalidResponse)?,
            digest,
            stamp,
        })
    }
}
