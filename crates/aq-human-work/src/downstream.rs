use super::*;
use std::collections::BTreeSet;

/// Distinct live task targets of live `blocks` edges leaving `task`.
pub fn blocked_by(raw: &Value, workspace: Uuid, task: Uuid) -> Option<u32> {
    let workspace = json!(workspace);
    let live = live_tasks(raw["nodes"].as_array()?, &workspace)?;
    let source = task.to_string();
    let mut targets = BTreeSet::new();
    for edge in raw["edges"].as_array()? {
        if edge["workspace_id"] != workspace {
            return None;
        }
        let target = edge["target"].as_str()?;
        if blocks_task(edge, &source, target) && live.contains(target) {
            targets.insert(target);
        }
    }
    targets.len().try_into().ok()
}

fn live_tasks<'a>(nodes: &'a [Value], workspace: &Value) -> Option<BTreeSet<&'a str>> {
    let mut tasks = BTreeSet::new();
    for node in nodes {
        if &node["workspace_id"] != workspace {
            return None;
        }
        if node["kind"] == "task" && node["deleted"] == false {
            tasks.insert(node["id"].as_str()?);
        }
    }
    Some(tasks)
}

fn blocks_task(edge: &Value, source: &str, target: &str) -> bool {
    edge["edge_type"] == "blocks"
        && edge["deleted"] == false
        && edge["source"] == source
        && target != source
}

#[cfg(test)]
mod brief_tests {
    use super::*;
    #[test]
    fn downstream_counts_distinct_live_blocked_tasks_only() {
        let w = Uuid::new_v4();
        let t = Uuid::new_v4();
        let (a, b, gone, doc) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let n = |id: Uuid, kind: &str, deleted: bool| json!({"id":id,"workspace_id":w,"kind":kind,"deleted":deleted});
        let e = |s: Uuid, t: Uuid, ty: &str| json!({"workspace_id":w,"source":s,"target":t,"edge_type":ty,"deleted":false});
        let raw = json!({
            "nodes":[n(a,"task",false),n(b,"task",false),n(gone,"task",true),n(doc,"knowledge",false)],
            "edges":[e(t,a,"blocks"),e(t,a,"blocks"),e(b,t,"blocks"),e(t,gone,"blocks"),e(t,doc,"blocks"),e(t,b,"relates_to")]
        });
        assert_eq!(blocked_by(&raw, w, t).unwrap(), 1);
        let mut foreign = raw.clone();
        foreign["edges"][0]["workspace_id"] = json!(Uuid::new_v4());
        assert!(blocked_by(&foreign, w, t).is_none());
    }
}
