#[path = "../tests/common.rs"]
mod common;
use aq_admission::transport::*;
use common::*;
fn main() {
    let value = if std::env::args().nth(1).as_deref() == Some("schema") {
        transport_schema()
    } else {
        serde_json::json!([
            TransportDocument::Poll(Poll {
                schema_version: 1,
                operation_id: id(91),
                identity: WorkerIdentity {
                    node_id: id(11),
                    boot_id: id(12)
                },
                inventory_sequence: 1,
                available: aq_admission::Resources {
                    cpu_millis: 1000,
                    memory_bytes: 1048576,
                    disk_bytes: 1048576,
                    slots: 1
                },
                limit: 2,
                after_offer: None,
            }),
            TransportDocument::StopEvidence(WorkerStopEvidence {
                schema_version: 1,
                binding: binding(),
                guardian_id: id(70),
                launch_id: id(71),
                stop_operation_id: id(72),
                journal: JournalEvidence::Recorded {
                    revision: 2,
                    digest: aq_admission::Digest::of(b"journal-2")
                },
                disposition: StopDisposition::Stopped {
                    grant_id: id(73),
                    stop_receipt_digest: aq_admission::Digest::of(b"stop")
                },
                reason: StopReason::ExecutionExpiry,
            }),
            TransportDocument::MaterialDelivery(MaterialDelivery {
                lease: lease(),
                clock: DeliveryClock::new(1000, 2000).unwrap()
            }),
        ])
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
