//! Graph action adapter for typed task creation.
pub(crate) mod supervisor;
use super::graph_actions::{Current, GraphPayload};
use super::*;
use crate::dashboard_api::errors::ApiError;
use aq_human_work::task_graph::Author;
pub(crate) use aq_human_work::task_graph::{GraphCreate, Task, hints_valid, metadata};
#[cfg(feature = "project_environments")]
pub(crate) use aq_human_work::task_graph::{Needs, needs};

impl GraphPayload for GraphCreate {
    const KIND: &'static str = "task.graph.create";
    const GRANT: &'static str = aq_agent_grants::op::TASK_CREATE_SUBTASK;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::ParentTask {
        field: "parent_id",
        unparented: aq_agent_grants::op::TASK_CREATE_PROJECT,
    };
    const ENTITY: &'static str = "project";
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
        GraphCreate::valid(self)
    }
    fn roles(&self) -> Vec<&str> {
        GraphCreate::roles(self)
    }
    fn tasks(&self) -> Vec<Uuid> {
        self.external_tasks()
    }
    fn operations(&self, current: &Current<'_>) -> std::result::Result<Vec<Value>, ApiError> {
        let author = match (current.agent, current.supervisor_actor) {
            (Some((attempt, task)), _) => Author::Agent { attempt, task },
            (None, Some(actor)) => Author::Supervisor {
                actor,
                session: current.actor,
            },
            (None, None) => Author::Owner(current.actor),
        };
        Ok(self.plan_operations(current.project, author))
    }
}
