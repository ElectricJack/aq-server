use super::*;

#[derive(Default)]
pub(in crate::playbook_host9) struct FakeGraph {
    pub nodes: Mutex<BTreeMap<Uuid, Value>>,
}
#[async_trait::async_trait]
impl WorkGraph for FakeGraph {
    async fn node(&self, _: Uuid, id: Uuid) -> Result<Option<Value>> {
        Ok(self.nodes.lock().unwrap().get(&id).cloned())
    }
    async fn mutate(&self, _: Uuid, _: Value) -> Result<(u16, Value)> {
        Err(Error::Invalid)
    }
}
impl FakeGraph {
    pub fn task(&self, title: &str, metadata: Value) -> Uuid {
        let id = Uuid::new_v4();
        let node = json!({"id": id, "kind": "task", "version": 3, "title": title,
            "body": format!("{title} body"), "metadata": metadata});
        self.nodes.lock().unwrap().insert(id, node);
        id
    }
    pub fn node_of(&self, id: Uuid) -> Value {
        self.nodes.lock().unwrap()[&id].clone()
    }
    pub fn stored(&self, id: Uuid) -> Option<CompactRoute> {
        CompactRoute::from_metadata(&self.node_of(id)["metadata"]).unwrap()
    }
}

/// The server side of routing: where tasks live, which projects are on
/// the router, attempt counts, Settings offers, and the route writes
/// (applied to the fake graph with the version check and owner rule of
/// `routing::write`).
pub(in crate::playbook_host9) struct FakeRouting {
    pub graph: Arc<FakeGraph>,
    pub located: Mutex<BTreeMap<Uuid, (Uuid, Uuid)>>,
    pub on_router: Mutex<BTreeSet<Uuid>>,
    pub attempts: Mutex<(u32, u32)>,
    pub options: Mutex<Options>,
    pub writes: Mutex<Vec<(Uuid, Option<Route>)>>,
}
#[async_trait::async_trait]
impl RoutingPort for FakeRouting {
    async fn located(&self, task: Uuid) -> Result<Option<(Uuid, Uuid)>> {
        Ok(self.located.lock().unwrap().get(&task).copied())
    }
    async fn on_router(&self, project: Uuid) -> Result<bool> {
        Ok(self.on_router.lock().unwrap().contains(&project))
    }
    async fn attempts(&self, _: Uuid) -> Result<(u32, u32)> {
        Ok(*self.attempts.lock().unwrap())
    }
    async fn options(&self) -> Result<Options> {
        Ok(self.options.lock().unwrap().clone())
    }
    async fn write(&self, at: (Uuid, Uuid, u64), route: Option<&Route>) -> Result<Written> {
        let (_, task, version) = at;
        let mut nodes = self.graph.nodes.lock().unwrap();
        let node = nodes.get_mut(&task).ok_or(Error::Unknown)?;
        let stored = CompactRoute::from_metadata(&node["metadata"]).unwrap_or(None);
        if node["version"] != json!(version)
            || !aq_routing::may_replace(stored.as_ref(), Source::Router)
        {
            return Ok(Written::Stale);
        }
        let mut writes = self.writes.lock().unwrap();
        let sequence = writes.len() as u64 + 1;
        let compact = route.map(|r| r.compact(sequence).unwrap());
        node["metadata"] = aq_routing::with_route(&node["metadata"], compact.as_ref()).unwrap();
        node["version"] = json!(version + 1);
        writes.push((task, route.cloned()));
        Ok(Written::Routed { sequence })
    }
}
