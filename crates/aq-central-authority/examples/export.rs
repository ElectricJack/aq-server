use aq_central_authority::*;
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

fn examples() -> Value {
    let original: Value = serde_json::from_str(include_str!("../artifacts/examples.json")).unwrap();
    let Document::Plan(plan) = decode(&serde_json::to_vec(&original["plan"]).unwrap()).unwrap()
    else {
        panic!()
    };
    let Document::Permit(permit) =
        decode(&serde_json::to_vec(&original["permit"]).unwrap()).unwrap()
    else {
        panic!()
    };
    let admitted = Clock {
        machine_boot_id: permit.machine_boot_id,
        boot_ms: permit.issued_boot_ms,
    };
    let state = Run::new(plan.clone(), admitted.clone()).unwrap();
    let dispatch = plan.dispatch(permit.clone()).unwrap();
    let event = Event::CommitStart {
        launch: HostLaunchRef {
            binding: plan.binding().unwrap(),
            launch_id: Uuid::from_u128(100),
            intent: plan.profiles.host_plan.clone(),
        },
        dispatch: dispatch.clone(),
    };
    let transition = state.transition(&event, admitted).unwrap();
    let mut successor = plan.clone();
    successor.generation = 2;
    successor.session = SessionIdentity::Central {
        authority_id: Uuid::from_u128(3),
        session_id: Uuid::from_u128(104),
        generation: 2,
    };
    successor.dispatch_id = Uuid::from_u128(107);
    successor.host_operation_id = Uuid::from_u128(108);
    successor.batch.messages = vec![MailRef {
        message_id: Uuid::from_u128(105),
        cursor: 3,
        payload_digest: Digest::of(b"new immutable mail"),
    }];
    successor.batch.through_cursor = 3;
    let history = MailHistoryFence {
        schema_version: 1,
        actor_id: successor.subject.actor_id(),
        conversation_id: successor.subject.conversation_id(),
        through_generation: 1,
        source_snapshot: ArtifactRef {
            id: Text::new("retained-history-query").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"authenticated owner query required"),
        },
        entries: successor
            .batch
            .messages
            .iter()
            .cloned()
            .map(|message| MessageHistory {
                message,
                disposition: HistoryDisposition::NoPriorDispatch,
            })
            .collect(),
        operations: [successor.dispatch_id, successor.host_operation_id]
            .into_iter()
            .map(|operation_id| OperationHistory {
                operation_id,
                disposition: OperationDisposition::Unused,
            })
            .collect(),
    };
    validate_successor_history(&successor, 1, &history).unwrap();
    let examples = BTreeMap::from([
        ("plan", Document::Plan(plan)),
        ("permit", Document::Permit(permit)),
        ("dispatch", Document::Dispatch(dispatch)),
        ("prepared_state", Document::State(state)),
        ("commit_start_event", Document::Event(event)),
        ("start_transition", Document::Transition(transition)),
        ("successor_plan", Document::Plan(successor)),
        ("successor_history", Document::History(history)),
    ]);
    for value in examples.values() {
        decode(&encode(value).unwrap()).unwrap();
    }
    serde_json::to_value(examples).unwrap()
}
fn main() {
    let path = std::path::PathBuf::from(std::env::args().nth(1).expect("artifact output path"));
    let value = if std::env::args().any(|s| s == "--examples") {
        examples()
    } else {
        schema()
    };
    let bytes = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
    if std::env::args().any(|s| s == "--check") {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes.as_bytes(),
            "central artifact drift"
        );
    } else {
        std::fs::write(path, bytes).unwrap();
    }
}
