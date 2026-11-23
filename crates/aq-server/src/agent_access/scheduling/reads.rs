use super::*;
pub(super) struct ReadyRead<'a> {
    pub(super) workspace: Uuid,
    pub(super) limit: u16,
    pub(super) cursor: Option<&'a str>,
}
impl ReadyRead<'_> {
    pub(super) async fn load(self, runtime: &Runtime, clock: &Clock<'_>) -> Result<ReadyPage> {
        let Self {
            workspace,
            limit,
            cursor,
        } = self;
        let valid_page = (1..=64).contains(&limit) && cursor.is_none_or(bounded_cursor);
        if workspace.is_nil() || !valid_page {
            return Err(SchedulingReadError::InvalidRequest);
        }
        let mut query = vec![("limit".into(), limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor".into(), cursor.into()));
        }
        let request = ReadRequest {
            path: format!("/v1/workspaces/{workspace}/ready"),
            query,
            max_bytes: 1_048_576,
        };
        let (value, stamp, _bytes) = request.get(runtime, clock).await?;
        if !contract::upstream_valid("NodePage", &value) {
            return Err(SchedulingReadError::InvalidResponse);
        }
        let items = scheduling::ready_tasks(&value, workspace, limit)?;
        let next_cursor = value["next_cursor"].as_str().map(str::to_owned);
        if next_cursor
            .as_ref()
            .is_some_and(|c| !bounded_cursor(c) || Some(c.as_str()) == cursor)
        {
            return Err(SchedulingReadError::InvalidResponse);
        }
        Ok(ReadyPage {
            workspace_id: workspace,
            items,
            next_cursor,
            stamp,
        })
    }
}
pub(super) struct ContextRead<'a> {
    expected: &'a TaskRevision,
    depth: u8,
    pub(super) limit: u16,
    max_bytes: u32,
}
impl<'a> ContextRead<'a> {
    pub(super) fn new(expected: &'a TaskRevision, depth: u8, limit: u16, max_bytes: u32) -> Self {
        Self {
            expected,
            depth,
            limit,
            max_bytes,
        }
    }
    pub(super) async fn load(self, runtime: &Runtime, clock: &Clock<'_>) -> Result<TaskContext> {
        let Self {
            expected,
            depth,
            limit,
            max_bytes,
        } = self;
        expected
            .validate()
            .map_err(|_| SchedulingReadError::InvalidRequest)?;
        let bounded_size = (1..=128).contains(&limit) && (1024..=1_048_576).contains(&max_bytes);
        if depth > 5 || !bounded_size {
            return Err(SchedulingReadError::InvalidRequest);
        }
        let query = vec![
            ("depth".into(), depth.to_string()),
            ("limit".into(), limit.to_string()),
            ("max_bytes".into(), max_bytes.to_string()),
            ("direction".into(), "both".into()),
        ];
        let request = ReadRequest {
            path: format!(
                "/v1/workspaces/{}/nodes/{}/context",
                expected.workspace_id, expected.task_id
            ),
            query,
            max_bytes: max_bytes as usize,
        };
        let (value, stamp, bytes) = request.get(runtime, clock).await?;
        if !contract::upstream_valid("Context", &value) {
            return Err(SchedulingReadError::InvalidResponse);
        }
        let revision = scheduling::context_revision(&value, expected, limit)?;
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
