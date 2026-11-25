use super::*;
fn native(profile: &mut Value, context: &mut Value) {
    profile["read_scope"]["edge_types"] = json!(["references"]);
    context["edges"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["edge_type"] == "references");
}
pub(super) async fn outgoing() {
    let (url, graph) = fake_graph().await;
    let e = setup_graph_with(
        Some(json!([{"operation":"task.read","scope":"project:self"}])),
        &url,
        None,
        native,
    )
    .await;
    let (status, children) = send(
        &e.app,
        "/api/v1/commands/task.children",
        Some(&e.token),
        Some(json!({"params":{}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{children}");
    let edges = children["result"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "{children}");
    assert_eq!(edges[0]["edge_type"], "parent-child");
    assert_eq!(edges[0]["target"], TASK_3);
    // Without task.read the edge type stays outside the profile.
    let e = setup_graph_with(
        Some(json!([{"operation":"knowledge.read","scope":"project:self"}])),
        &url,
        None,
        native,
    )
    .await;
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/graph.read",
        Some(&e.token),
        Some(
            json!({"params":{"query":{"kind":"relationships","node_id":e.binding["task_id"],
            "direction":"outgoing","edge_types":["parent-child"],"limit":64,"cursor":null}}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    graph.abort();
}
pub(super) async fn incoming() {
    // Which parent-child edges come back follows the task.read scope: here
    // the attempt's own task is the child of task 3. Its parent edge
    // (source task 3) is readable at project:self, not at task:self, even
    // though knowledge.read reads every record of the project.
    let (url, graph) = fake_graph_with(|data| {
        for e in data["context"]["edges"].as_array_mut().unwrap() {
            if e["edge_type"] == "parent-child" {
                let source = e["source"].clone();
                e["source"] = e["target"].clone();
                e["target"] = source;
            }
        }
    })
    .await;
    let parents = |scope: &'static str| {
        let url = url.clone();
        async move {
            let e = setup_graph_with(
                Some(json!([
                    {"operation":"task.read","scope":scope},
                    {"operation":"knowledge.read","scope":"project:self"}
                ])),
                &url,
                None,
                native,
            )
            .await;
            let (status, body) = send(
                &e.app,
                "/api/v1/commands/graph.read",
                Some(&e.token),
                Some(json!({"params":{"query":{"kind":"relationships","node_id":e.binding["task_id"],
                    "direction":"incoming","edge_types":["parent-child"],"limit":64,"cursor":null}}})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["result"]["edges"].as_array().unwrap().len()
        }
    };
    assert_eq!(parents("project:self").await, 1);
    assert_eq!(parents("task:self").await, 0);
    assert_eq!(parents("task:subtree").await, 0);
    graph.abort();
}
