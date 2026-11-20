use super::*;
use aq_dashboard_contract::action_contract::ActivationCounts;
use aq_human_work::{TaskActivateSubtree, subtree_activation};

impl GraphPayload for TaskActivateSubtree {
    const KIND: &'static str = "task.activate.subtree";
    const GRANT: &'static str = "task.activate.subtree";
    const ENTITY: &'static str = "task";
    const ACTIVATE_SUBTREE: bool = true;

    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn operations(&self, _: &Current<'_>) -> ApiResult<Vec<Value>> {
        Err(ApiError::invalid())
    }
}

pub(super) async fn plan(
    rt: &Runtime,
    workspace: Uuid,
    task: Uuid,
    expected: u64,
) -> Result<std::result::Result<Vec<Value>, Outcome>> {
    let tree = crate::agent_access::activation_tree(rt, workspace, task).await?;
    Ok(
        subtree_activation::operations(&tree, workspace, task, expected).map_err(|error| {
            if error == subtree_activation::PlanError::Stale {
                Outcome::conflict(BTreeMap::new())
            } else {
                Outcome::refused(error.reason())
            }
        }),
    )
}

pub(super) fn confirmed(raw: &Value, operations: &[Value]) -> Outcome {
    if !crate::agent_access::settlement_response_valid(raw) {
        return Outcome::unknown();
    }
    let Some(activated) = subtree_activation::activated(operations, raw) else {
        return Outcome::unknown();
    };
    let affected = activated
        .iter()
        .map(|node| {
            existing::entity(
                "task",
                node["id"].as_str().unwrap(),
                Some(node["version"].to_string()),
            )
        })
        .collect();
    let mut outcome = Outcome::confirmed(
        vec![existing::entity(
            "graph_change_set",
            raw["change_set_id"].as_str().unwrap(),
            None,
        )],
        affected,
    );
    outcome.activation = Some(ActivationCounts {
        activated: activated.len() as u32,
        skipped: (operations.len() - 1 - activated.len()) as u32,
    });
    outcome
}
