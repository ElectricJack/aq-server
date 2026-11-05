mod common;
use aq_admission::*;
use common::*;
#[test]
fn bounded_project_round_robin_preserves_priority_then_advances_cursor() {
    let input = placement();
    let result = place(&input).unwrap();
    assert!(!result.authority_admission);
    assert_eq!(
        result
            .proposed
            .iter()
            .map(|p| p.project_id)
            .collect::<Vec<_>>(),
        vec![id(31), id(32)]
    );
    assert_eq!(result.next_after_project, Some(id(32)));
    let mut rotated = input;
    rotated.max_offers = 1;
    rotated.after_project = Some(id(31));
    assert_eq!(place(&rotated).unwrap().proposed[0].project_id, id(32));
}
#[test]
fn oversized_head_does_not_hide_later_fit_and_resources_are_reserved_in_proposal() {
    let mut input = placement();
    input.candidates[0].priority = 100;
    input.candidates[0].resources.memory_bytes = 1000;
    let result = place(&input).unwrap();
    assert_eq!(result.proposed.len(), 2);
    assert!(!result.proposed.iter().any(|p| p.task.task_id == id(51)));
    assert!(result.refused.iter().any(|r| r.task_id == id(51)));
    input.nodes[0].available.slots = 1;
    assert_eq!(place(&input).unwrap().proposed.len(), 1);
}
#[test]
fn warmth_is_only_after_eligibility_and_cold_nodes_can_prepare() {
    let mut input = placement();
    input.max_offers = 1;
    input.nodes.push(NodeCapacity {
        node_id: id(18),
        boot_id: id(19),
        ..input.nodes[0].clone()
    });
    input.candidates[0].nodes.push(NodeEligibility {
        node_id: id(18),
        boot_id: id(19),
        eligible: false,
        source_warm: true,
        reason_codes: vec![Label::new("project_denied").unwrap()],
    });
    let result = place(&input).unwrap();
    assert_eq!(result.proposed[0].node_id, id(11));
    assert!(!result.proposed[0].source_warm);
    input.candidates[0].nodes[1].eligible = true;
    assert_eq!(place(&input).unwrap().proposed[0].node_id, id(18));
}
#[test]
fn stale_boot_capacity_candidate_and_duplicate_subjects_never_become_authority() {
    let mut input = placement();
    input.nodes[0].boot_id = id(99);
    assert!(place(&input).unwrap().proposed.is_empty());
    input.nodes[0].boot_id = id(12);
    input.nodes[0].expires_at_ms = input.now_ms;
    assert!(place(&input).unwrap().proposed.is_empty());
    input = placement();
    input.candidates[0].expires_at_ms = input.now_ms;
    assert!(
        !place(&input)
            .unwrap()
            .proposed
            .iter()
            .any(|p| p.task.task_id == id(51))
    );
    input = placement();
    input.candidates.push(input.candidates[0].clone());
    assert!(place(&input).is_err());
}
#[test]
fn stable_ties_and_snapshot_bounds_are_explicit() {
    let input = placement();
    let first = place(&input).unwrap();
    let mut shuffled = input.clone();
    shuffled.candidates.reverse();
    shuffled.nodes.reverse();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(place(&shuffled).unwrap()).unwrap()
    );
    shuffled.max_offers = 129;
    assert!(place(&shuffled).is_err());
    shuffled = placement();
    shuffled.nodes = vec![shuffled.nodes[0].clone(); 129];
    assert!(place(&shuffled).is_err());
    let mut forged = place(&input).unwrap();
    forged.authority_admission = true;
    assert!(forged.validate().is_err());
}
