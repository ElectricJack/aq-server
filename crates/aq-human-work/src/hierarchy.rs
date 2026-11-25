use serde_json::{Value, json};
use uuid::Uuid;
/// Longest list of live children a hierarchy reports.
pub const MAX_HIERARCHY_CHILDREN: usize = 16;

/// What the task's place in the Graph hierarchy says about running it.
pub struct Hierarchy {
    /// Relationships that keep the task out of the runnable queue, ancestors
    /// nearest first, then live children (at most [`MAX_HIERARCHY_CHILDREN`]).
    pub blocking: Vec<Value>,
    /// False when a Context was truncated, an ancestor could not be read, or
    /// the ancestry was needed but not supplied.
    pub complete: bool,
}

fn live_parent_edge(e: &Value) -> bool {
    e["edge_type"] == "parent-child" && e["deleted"] == false
}

/// Whether the hierarchy above `task` has to be read from a deeper incoming
/// Context: the depth-1 Context shows a live parent, or it was truncated.
pub fn needs_ancestry(context: &Value, task: Uuid) -> bool {
    context["truncated"] != false
        || context["edges"].as_array().is_some_and(|edges| {
            edges
                .iter()
                .any(|e| live_parent_edge(e) && e["target"] == json!(task))
        })
}

/// The hierarchy relationships that keep an active `task` from being runnable
/// (Graph's ready query excludes it): a draft or retired ancestor within
/// `max_levels` above it, or any live child (a container is not a leaf).
/// `context` is the task's depth-1 Context in both directions; `incoming` is
/// its incoming Context at `max_levels` depth, needed when
/// [`needs_ancestry`]. None when a Context lacks node or edge lists.
pub fn hierarchy(
    context: &Value,
    incoming: Option<&Value>,
    task: Uuid,
    max_levels: u32,
) -> Option<Hierarchy> {
    let complete = context["truncated"] == false
        && incoming.is_none_or(|c| c["truncated"] == false)
        && (incoming.is_some() || !needs_ancestry(context, task));
    let graph = Context::read(context, incoming)?;
    let mut hierarchy = Hierarchy {
        blocking: Vec::new(),
        complete,
    };
    graph.ancestors(&mut hierarchy, task, max_levels)?;
    hierarchy.blocking.extend(graph.children(task));
    Some(hierarchy)
}

struct Context<'a> {
    nodes: Vec<&'a Value>,
    edges: Vec<&'a Value>,
}
impl<'a> Context<'a> {
    fn read(context: &'a Value, incoming: Option<&'a Value>) -> Option<Self> {
        let mut graph = Self {
            nodes: Vec::new(),
            edges: Vec::new(),
        };
        for c in std::iter::once(context).chain(incoming) {
            graph.nodes.extend(c["nodes"].as_array()?);
            graph.edges.extend(c["edges"].as_array()?);
        }
        Some(graph)
    }

    fn node(&self, id: &Value) -> Option<&Value> {
        self.nodes.iter().copied().find(|n| &n["id"] == id)
    }

    fn ancestors(&self, out: &mut Hierarchy, task: Uuid, max_levels: u32) -> Option<()> {
        let mut seen = std::collections::BTreeSet::from([task]);
        let mut pending = std::collections::VecDeque::from([(task, 0)]);
        // Descendants in the queue are ordered by depth.
        while let Some((current, depth)) =
            pending.pop_front().filter(|(_, depth)| *depth < max_levels)
        {
            for edge in self
                .edges
                .iter()
                .copied()
                .filter(|e| live_parent_edge(e) && e["target"] == json!(current))
            {
                let id = edge["source"].as_str()?.parse().ok()?;
                let parent = (id, depth + 1);
                if seen.insert(id) && self.visit_ancestor(out, edge, parent) {
                    pending.push_back(parent);
                }
            }
        }
        Some(())
    }

    fn visit_ancestor(&self, out: &mut Hierarchy, edge: &Value, (id, depth): (Uuid, u32)) -> bool {
        let Some(node) = self.node(&edge["source"]).filter(|n| n["deleted"] == false) else {
            out.complete = false;
            return false;
        };
        if node["kind"] != "task" {
            return false;
        }
        let code = if node["retired"] == true {
            Some("ancestor_retired")
        } else if node["task_state"] == "draft" {
            Some("ancestor_draft")
        } else {
            None
        };
        if let Some(code) = code {
            out.blocking.push(json!({"relationship":"ancestor","level":depth,"id":id,"title":node["title"],"task_state":node["task_state"],"retired":node["retired"],"edge_id":edge["id"],"code":code}));
        }
        true
    }

    fn children(&self, task: Uuid) -> impl Iterator<Item = Value> + '_ {
        self.edges.iter().copied().filter(move |e|live_parent_edge(e) && e["source"]==json!(task))
            .filter_map(|e| {
                let node=self.node(&e["target"]);
                if node.is_some_and(|n| n["deleted"]==true || n["retired"]==true) {return None;}
                let field=|k:&str|node.map_or(Value::Null,|n|n[k].clone());
                Some(json!({"relationship":"child","level":1,"id":e["target"],"title":field("title"),"task_state":field("task_state"),"retired":field("retired"),"edge_id":e["id"],"code":"has_live_children"}))
            }).take(MAX_HIERARCHY_CHILDREN)
    }
}

/// [`hierarchy`] for the work view: reads the incoming ancestry through
/// `incoming` only when [`needs_ancestry`]. A read that returns None (it
/// failed) leaves the answer incomplete, not an error.
pub async fn hierarchy_via<F: Future<Output = Option<Value>>>(
    context: &Value,
    task: Uuid,
    max_levels: u32,
    incoming: impl FnOnce() -> F,
) -> Option<Hierarchy> {
    let deeper = match needs_ancestry(context, task) {
        true => incoming().await,
        false => None,
    };
    hierarchy(context, deeper.as_ref(), task, max_levels)
}

/// Parent of `task` from a present `parent-child` edge (the source is the
/// parent) in a Context; `Null` without one. `check` validates the parent's
/// node for its kind. None when the parent node fails it.
pub fn parent(
    context: &Value,
    task: Uuid,
    check: impl Fn(&Value, &str) -> Option<Value>,
) -> Option<Value> {
    let edges = context["edges"].as_array().into_iter().flatten();
    let Some(edge) = { edges }.find(|e| live_parent_edge(e) && e["target"] == json!(task)) else {
        return Some(Value::Null);
    };
    let pid = &edge["source"];
    let nodes = context["nodes"].as_array().into_iter().flatten();
    let Some(found) = { nodes }.find(|n| &n["id"] == pid && n["deleted"] == false) else {
        return Some(
            json!({"id":pid,"kind":null,"title":null,"task_state":null,"edge_id":edge["id"]}),
        );
    };
    let n = check(found, found["kind"].as_str()?)?;
    Some(
        json!({"id":pid,"kind":n["kind"],"title":n["title"],"task_state":n["task_state"],"edge_id":edge["id"]}),
    )
}
