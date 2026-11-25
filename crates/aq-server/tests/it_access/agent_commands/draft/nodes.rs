use super::*;
pub(super) struct Nodes {
    pub(super) workspace: Value,
    pub(super) project: Value,
    pub(super) own: Value,
    pub(super) namespace: Uuid,
    pub(super) planner: Uuid,
    pub(super) draft: Uuid,
    pub(super) sibling: Uuid,
    pub(super) outside: Uuid,
    pub(super) active: Uuid,
    pub(super) foreign: Uuid,
    pub(super) foreign_blocker: Value,
    pub(super) unrelated_edge: Value,
}
impl Nodes {
    pub(super) fn new() -> Self {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let base = &fixture["context"]["binding"];
        let (workspace, project, own) = (
            base["workspace_id"].clone(),
            base["project_id"].clone(),
            base["task_id"].clone(),
        );
        let (namespace, planner, draft, sibling, outside, active, foreign) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let edge = |source, target, kind| {
            json!({"id":Uuid::new_v4(),"source":source,"target":target,
        "edge_type":kind,"source_revision":null,"target_revision":null,"description":null,
        "workspace_id":workspace,"version":1,"deleted":false,"created_at":"2026-11-07T00:00:00Z"})
        };
        let foreign_blocker = edge(outside, draft, "blocks");
        let unrelated_edge = edge(draft, sibling, "blocks");

        Self {
            workspace,
            project,
            own,
            namespace,
            planner,
            draft,
            sibling,
            outside,
            active,
            foreign,
            foreign_blocker,
            unrelated_edge,
        }
    }
    pub(super) fn fixture(&self, pause: Arc<Mutex<Option<ScopePause>>>) -> DraftFixture {
        let Self {
            workspace,
            project,
            own,
            namespace,
            planner,
            draft,
            sibling,
            outside,
            active,
            foreign,
            foreign_blocker,
            unrelated_edge,
        } = self;
        let meta = json!({"aq:project_id":project});
        // The host's retained motivation must take precedence over the Graph stamp.
        let own_meta = json!({"aq:project_id":project,"aq:playbook_namespace":namespace,
        "aq:logical_key":"test-review","aq:work_kind":"investigation",
        "aq:subject":{"id":outside,"revision":1}});
        let edge = |source, target, kind| {
            json!({"id":Uuid::new_v4(),"source":source,"target":target,
        "edge_type":kind,"source_revision":null,"target_revision":null,"description":null,
        "workspace_id":workspace,"version":1,"deleted":false,"created_at":"2026-11-07T00:00:00Z"})
        };
        DraftFixture {
            pause,
            nodes: vec![
                (own.clone(), "active", own_meta),
                (json!(planner), "active", meta.clone()),
                (json!(draft), "draft", meta.clone()),
                (json!(sibling), "draft", meta.clone()),
                (json!(outside), "draft", meta.clone()),
                (json!(active), "active", meta),
                (
                    json!(foreign),
                    "draft",
                    json!({"aq:project_id":Uuid::new_v4()}),
                ),
            ],
            edges: vec![
                edge(*planner, *draft, "parent-child"),
                edge(*planner, *sibling, "parent-child"),
                edge(*planner, *active, "parent-child"),
                edge(*planner, *foreign, "parent-child"),
                foreign_blocker.clone(),
                unrelated_edge.clone(),
            ],
        }
    }
}
