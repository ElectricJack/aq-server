use super::*;
#[test]
fn every_slot_holder_is_listed_under_its_own_node_with_phase_and_output_time() {
    let mut i = input(vec![
        node((1, "a"), WorkerHealth::Online, (Some(4), 3), vec![]),
        node((2, "b"), WorkerHealth::Online, (Some(4), 1), vec![]),
    ]);
    use AgentPhase::*;
    for (n, attempt, phase) in [
        (1, 5, Starting),
        (1, 6, Running),
        (1, 7, Finishing),
        (2, 8, Running),
    ] {
        i.agents
            .push((uid(n), agent(uid(attempt), "claude-sonnet-5", phase)));
    }
    let s = assemble(i).unwrap();
    let nodes: Vec<&SwarmNode> = s.machines.iter().flat_map(|m| &m.nodes).collect();
    for n in &nodes {
        assert_eq!(n.agents.len() as u32, n.slots.occupied, "{}", n.name);
    }
    let phases: Vec<_> = nodes[0].agents.iter().map(|a| a.phase.unwrap()).collect();
    assert_eq!(phases, [Starting, Running, Finishing]);
    assert_eq!(nodes[1].agents[0].attempt_id, uid(8));
    let row = serde_json::to_value(&nodes[0].agents[0]).unwrap();
    assert_eq!(
        [
            &row["phase"],
            &row["last_output_at"],
            &row["held_since"],
            &row["task"]["title"]
        ]
        .map(|v| v.as_str()),
        [
            "starting",
            "2026-11-07T00:00:03.000Z",
            "2026-11-06T23:59:50.000Z",
            "Fix the flaky test"
        ]
        .map(Some)
    );
    assert_eq!(row.get("ran_with"), Some(&serde_json::Value::Null));
}
/// The title reads below never wait, so one poll finishes them.
fn finish<F: std::future::Future>(future: F) -> F::Output {
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match std::pin::pin!(future).poll(&mut context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("title read waited"),
    }
}
#[test]
fn titles_are_read_once_per_task_and_then_served_from_the_cache() {
    use crate::agents::{Held, remember, title};
    let (known, fresh, slow) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    remember(known, Some("  Known title ".into()));
    let mut held = Held {
        agents: Vec::new(),
        running: 0,
        cut: false,
    };
    for task in [known, fresh, slow, fresh] {
        let mut a = agent(Uuid::new_v4(), "claude-sonnet-5", AgentPhase::Running);
        a.task.as_mut().unwrap().id = task;
        a.task.as_mut().unwrap().title = task.to_string();
        held.agents.push((uid(1), a));
    }
    let reads = std::cell::RefCell::new(Vec::new());
    let read = |_project: Uuid, task: Uuid| {
        reads.borrow_mut().push(task);
        // The third task's read runs out of time: no further reads this page.
        std::future::ready((task == fresh).then(|| Some("Fresh title".to_owned())))
    };
    finish(held.titles(read));
    let titles: Vec<&str> = (held.agents.iter())
        .map(|(_, a)| a.task.as_ref().unwrap().title.as_str())
        .collect();
    let id = slow.to_string();
    assert_eq!(
        titles,
        ["Known title", "Fresh title", id.as_str(), "Fresh title"]
    );
    assert_eq!(*reads.borrow(), [fresh, slow]);
    for (task, cached, displayed) in [
        (fresh, Some(Some("Fresh title".into())), "Fresh title"),
        (slow, None, id.as_str()),
    ] {
        assert_eq!(
            (title(task), crate::agents::title_or_id(task)),
            (cached, displayed.to_string())
        );
    }
    // A second page reads nothing it already has.
    reads.borrow_mut().clear();
    held.agents.truncate(2);
    finish(held.titles(read));
    assert!(reads.borrow().is_empty());
}
