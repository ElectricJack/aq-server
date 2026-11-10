use super::*;
/// Distinct live task targets of live `blocks` edges leaving `task`.
pub fn blocked_by(raw: &Value, w: Uuid, task: Uuid) -> Option<u32> {
    let mut live = std::collections::BTreeSet::new();
    for n in raw["nodes"].as_array()? {
        if n["workspace_id"] != json!(w) {
            return None;
        }
        if n["kind"] == "task" && n["deleted"] == false {
            live.insert(n["id"].as_str()?.to_owned());
        }
    }
    let source = task.to_string();
    let mut targets = std::collections::BTreeSet::new();
    for e in raw["edges"].as_array()? {
        if e["workspace_id"] != json!(w) {
            return None;
        }
        let target = e["target"].as_str()?;
        if e["edge_type"] == "blocks"
            && e["deleted"] == false
            && e["source"] == source.as_str()
            && target != source
            && live.contains(target)
        {
            targets.insert(target.to_owned());
        }
    }
    u32::try_from(targets.len()).ok()
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
