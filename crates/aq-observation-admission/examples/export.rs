use aq_observation_admission::*;
use serde_json::json;
use uuid::Uuid;
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn fixture() -> serde_json::Value {
    let mut access = Access {
        node_id: id(1),
        boot_id: id(2),
        accepted_through: 0,
        max_pending_distance: 100,
        new_boot_previously_owned: false,
    };
    let p = Request {
        version: VERSION,
        operation_id: id(3),
        node_id: id(1),
        operation: Operation::Prepare {
            boot_id: id(2),
            store_id: id(4),
        },
    };
    let prepared = apply(Retained::new(None, None), &p, &access, Some(id(5)))
        .unwrap()
        .reply;
    let a = Request {
        version: VERSION,
        operation_id: id(6),
        node_id: id(1),
        operation: Operation::Activate {
            marker: prepared.current_owner.marker.clone(),
        },
    };
    let active = apply(
        Retained::new(Some(&prepared.current_owner), None),
        &a,
        &access,
        None,
    )
    .unwrap()
    .reply;
    access.boot_id = id(7);
    let b = Request {
        version: VERSION,
        operation_id: id(8),
        node_id: id(1),
        operation: Operation::AdvanceBoot {
            marker: active.current_owner.marker.clone(),
            expected_boot_id: id(2),
            expected_revision: 2,
            new_boot_id: id(7),
            next_sequence: 12,
        },
    };
    let boot = apply(
        Retained::new(Some(&active.current_owner), None),
        &b,
        &access,
        None,
    )
    .unwrap()
    .reply;
    let replay = apply(
        Retained::new(Some(&boot.current_owner), Some(&prepared.receipt)),
        &p,
        &access,
        None,
    )
    .unwrap()
    .reply;
    json!({"provenance":"synthetic contract fixture; no execution/source/harness authority","pairs":[{"request":p,"reply":prepared},{"request":a,"reply":active},{"request":b,"reply":boot},{"request":p,"reply":replay}],"status":Status{version:VERSION,node_id:id(1),admitted_node_boot_id:id(7),owner:Some(boot.current_owner),accepted_through:0}})
}
fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("artifacts");
    std::fs::write(
        root.join("transport.fixture.json"),
        format!("{}\n", serde_json::to_string_pretty(&fixture()).unwrap()),
    )
    .unwrap();
    std::fs::write(
        root.join("transport.schema.json"),
        format!("{}\n", serde_json::to_string_pretty(&schemas()).unwrap()),
    )
    .unwrap();
}
