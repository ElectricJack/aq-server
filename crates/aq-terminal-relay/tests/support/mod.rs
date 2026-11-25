use aq_session_protocol::*;
use aq_terminal_relay::draft::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn text(s: &str) -> Text {
    Text::new(s).unwrap()
}
fn pretty(v: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(v).unwrap() + "\n"
}
type Artifacts = BTreeMap<String, String>;

pub fn artifacts() -> Artifacts {
    let mut out = BTreeMap::new();
    out.insert(
        "schema.json".into(),
        pretty(&schemars::schema_for!(DraftDocument)),
    );
    out.insert(
        "peer-schema.json".into(),
        pretty(&schemars::schema_for!(WireDocument)),
    );
    out.insert(
        "human-runtime.schema.json".into(),
        pretty(&schemars::schema_for!(
            aq_terminal_relay::api::RuntimeDocument
        )),
    );

    for central in [false, true] {
        let fixture = PeerFixture::new(central);
        fixture.publish(&mut out, central);
    }
    epoch_artifacts(&mut out);
    // Schemars output is compared as bytes by drift tests; runtime validators also
    // enforce relational identity/capability/budget rules schema cannot express.
    let _: Value = serde_json::from_str(&out["schema.json"]).unwrap();
    out
}

struct PeerFixture {
    peer: Peer,
    lease: ControllerLease,
    input: InputCommand,
    resize: ResizeCommand,
}

impl PeerFixture {
    fn new(central: bool) -> Self {
        let peer = if central {
            Peer::Central {
                authority_id: id(1),
            }
        } else {
            Peer::Remote {
                node_id: id(1),
                boot_id: id(2),
            }
        };
        let terminal = TerminalIdentity {
            session: if central {
                SessionIdentity::Central {
                    authority_id: id(1),
                    session_id: id(2),
                    generation: 1,
                }
            } else {
                SessionIdentity::Remote {
                    node_id: id(1),
                    boot_id: id(2),
                    attempt_id: id(3),
                    assignment_generation: 1,
                    session_id: id(4),
                }
            },
            terminal_id: id(5),
            epoch: 1,
        };
        let lease = ControllerLease {
            terminal: terminal.clone(),
            principal_id: text("fixture-principal"),
            lease_id: id(6),
            generation: 1,
            issued_at_ms: 100,
            expires_at_ms: 10100,
            execution_expires_at_ms: 20100,
        };
        let input = InputCommand {
            operation_id: id(7),
            lease: lease.clone(),
            issued_at_ms: 200,
            deadline_ms: 1000,
            bytes: b"literal\n".to_vec(),
            escape_key: false,
        };
        let resize = ResizeCommand {
            operation_id: id(8),
            lease: lease.clone(),
            sequence: 1,
            issued_at_ms: 200,
            deadline_ms: 1000,
            columns: 80,
            rows: 24,
        };
        Self {
            peer,
            lease,
            input,
            resize,
        }
    }

    fn downstream(&self) -> DraftDocument {
        let terminal = &self.lease.terminal;
        let envelope = Envelope {
            version: VERSION,
            capability: text(CAPABILITY),
            peer: self.peer.clone(),
            exchange_id: id(11),
            cursor: 1,
            effects: vec![
                Effect::Fence(Fence {
                    terminal: terminal.clone(),
                    fence_id: id(12),
                    generation: 1,
                    controller: Some(self.lease.clone()),
                }),
                Effect::Input(self.input.clone()),
                Effect::Resize(self.resize.clone()),
                Effect::Watch(WatchSubscription {
                    terminal: terminal.clone(),
                    subscription_id: id(13),
                    principal_id: text("viewer"),
                    expires_at_ms: 5000,
                }),
                Effect::Capture(Capture {
                    terminal: terminal.clone(),
                    capture_request_id: id(10),
                    uncertain_operation_id: id(7),
                    lease_id: id(6),
                    controller_generation: 1,
                    after_ledger_revision: 3,
                    expires_at_ms: 1000,
                }),
                Effect::Fence(Fence {
                    terminal: terminal.clone(),
                    fence_id: id(14),
                    generation: 2,
                    controller: None,
                }),
            ],
        };
        DraftDocument::Downstream(envelope)
    }

    fn register(&self) -> DraftDocument {
        let registration = Register {
            version: 1,
            capability: text(CAPABILITY),
            operation_id: id(22),
            peer: self.peer.clone(),
            terminal: self.lease.terminal.clone(),
            expected_authority_revision: 1,
            input_mode: InputMode::FiniteV1,
            advertised_capabilities: vec![Capability {
                id: text(CAPABILITY),
                version: 1,
            }],
        };
        registration.validate().unwrap();
        DraftDocument::Register(registration)
    }

    fn upstream(&self) -> DraftDocument {
        let terminal = &self.lease.terminal;
        let frame = OutputFrame {
            terminal: terminal.clone(),
            sequence: 2,
            bytes: b"echo".to_vec(),
            byte_length: 4,
            digest: Digest::of(b"echo"),
        };
        let snapshot = Snapshot {
            snapshot_id: id(9),
            capture_request_id: id(10),
            terminal: terminal.clone(),
            through_sequence: 1,
            columns: 80,
            rows: 24,
            renderer: ArtifactRef {
                id: text("synthetic"),
                version: text("1"),
                digest: Digest::of(b"renderer"),
            },
            state: b"snapshot".to_vec(),
            digest: Digest::of(b"snapshot"),
        };
        DraftDocument::Upstream(PeerUpload {
            version: VERSION,
            capability: text(CAPABILITY),
            peer: self.peer.clone(),
            exchange_id: id(15),
            cursor: 1,
            epoch_receipts: vec![],
            receipts: vec![InputReceipt {
                operation_id: id(7),
                terminal: terminal.clone(),
                command_digest: canonical_digest(&self.input).unwrap(),
                disposition: InputDisposition::DeliveryUnknown,
            }],
            resize_receipts: vec![ResizeReceipt {
                terminal: terminal.clone(),
                operation_id: id(8),
                command_digest: canonical_digest(&self.resize).unwrap(),
                sequence: 1,
                applied: true,
            }],
            fence_acks: vec![FenceAck {
                terminal: terminal.clone(),
                fence_id: id(12),
                generation: 1,
            }],
            frames: vec![frame],
            snapshots: vec![snapshot],
            gaps: vec![OutputGap {
                terminal: terminal.clone(),
                first_missing: 3,
                last_missing: 4,
            }],
        })
    }

    fn publish(self, out: &mut Artifacts, central: bool) {
        let name = if central { "central" } else { "remote" };
        let down = self.downstream();
        let up = self.upstream();
        down.validate().unwrap();
        up.validate().unwrap();
        out.insert(format!("valid-{name}-downstream.json"), pretty(&down));
        out.insert(format!("valid-{name}-upstream.json"), pretty(&up));
        out.insert(
            format!("valid-{name}-registration.json"),
            pretty(&self.register()),
        );
        if central {
            human_artifacts(out, self.lease);
            invalid_downstream_artifacts(out, &down);
        }
    }
}
fn human_artifacts(out: &mut Artifacts, lease: ControllerLease) {
    let terminal = &lease.terminal;
    for (name, doc) in [
        (
            "renew",
            HumanLifecycle::Renew(RenewControl {
                terminal: terminal.clone(),
                operation_id: id(16),
                expected_session_revision: 1,
                expected_controller_generation: 1,
                lease_id: id(6),
                expires_at_ms: 12000,
            }),
        ),
        (
            "release",
            HumanLifecycle::Release(ReleaseControl {
                terminal: terminal.clone(),
                operation_id: id(17),
                expected_session_revision: 1,
                expected_controller_generation: 1,
                lease_id: id(6),
            }),
        ),
        (
            "capture",
            HumanLifecycle::Capture(HumanCapture {
                terminal: terminal.clone(),
                operation_id: id(18),
                expected_session_revision: 1,
                uncertain_operation_id: id(7),
                lease_id: id(6),
                expected_controller_generation: 1,
                expires_at_ms: 1000,
            }),
        ),
        (
            "open-epoch",
            HumanLifecycle::OpenEpoch(aq_terminal_relay::epoch::Open {
                terminal: terminal.clone(),
                operation_id: id(19),
                expected_session_revision: 1,
                expected_controller_generation: 1,
                lease_id: id(6),
                expires_at_ms: 1000,
            }),
        ),
        (
            "status",
            HumanLifecycle::Status(ControlStatus {
                terminal: terminal.clone(),
                session_revision: 1,
                generation: 1,
                lease: Some(lease),
                fence_acknowledged: false,
                input_paused: true,
            }),
        ),
    ] {
        let doc = DraftDocument::Human(doc);
        doc.validate().unwrap();
        out.insert(format!("valid-human-{name}.json"), pretty(&doc));
    }
}

fn invalid_downstream_artifacts(out: &mut Artifacts, down: &DraftDocument) {
    let mut value = serde_json::to_value(down).unwrap();
    value["body"]["version"] = json!(2);
    out.insert("invalid-version.json".into(), pretty(&value));
    let mut value = serde_json::to_value(down).unwrap();
    value["body"]["effects"][0]["body"]["unknown_mutation"] = json!(true);
    out.insert("invalid-extra-mutation.json".into(), pretty(&value));
    let mut value = serde_json::to_value(down).unwrap();
    value["body"]["peer"] = json!({"kind":"remote","node_id":id(1),"boot_id":id(2)});
    out.insert("invalid-cross-peer.json".into(), pretty(&value));
    let mut value = serde_json::to_value(down).unwrap();
    value["body"]["effects"][0]["body"]["generation"] = json!(0);
    out.insert("invalid-zero-fence.json".into(), pretty(&value));
    let mut value = serde_json::to_value(down).unwrap();
    value["body"]["effects"][4]["body"]["capture_request_id"] = json!(Uuid::nil());
    out.insert("invalid-nil-capture.json".into(), pretty(&value));
}

fn epoch_artifacts(out: &mut Artifacts) {
    let permit = aq_terminal_relay::epoch::Permit {
        terminal: TerminalIdentity {
            session: SessionIdentity::Central {
                authority_id: id(1),
                session_id: id(2),
                generation: 3,
            },
            terminal_id: id(4),
            epoch: 5,
        },
        epoch_id: id(6),
        lease_id: id(7),
        controller_generation: 8,
        expires_at_ms: 9,
        derivation_version: 1,
    };
    let operation = aq_terminal_relay::epoch::operation_id(&permit, 1).unwrap();
    epoch_request_artifacts(out, &permit, operation);
    let command = epoch_command(&permit, operation);
    epoch_downstream_artifacts(out, &command);
    poll_artifacts(out);
    epoch_upstream_artifacts(out, &command);
    upload_ack_artifact(out, &permit);
    out.insert("uuid-vectors.json".into(),pretty(&json!({"algorithm":"sha256-domain-terminal-epoch-lease-generation-sequence-uuidv8","permit":permit,"vectors":[{"sequence":1,"operation_id":"8da947e6-212b-8e9c-b631-f670ca25ba73"},{"sequence":2,"operation_id":"2bb83041-7173-835c-9847-9c25f8d014b8"},{"sequence":u64::MAX,"operation_id":"79e034f4-981d-8eb3-953f-4be646dc30df"}]})));
}

fn epoch_request_artifacts(
    out: &mut Artifacts,
    permit: &aq_terminal_relay::epoch::Permit,
    operation: Uuid,
) {
    let request = aq_terminal_relay::epoch::Request {
        version: 1,
        capability: text(aq_terminal_relay::epoch::CAPABILITY),
        epoch_id: permit.epoch_id,
        sequence: 1,
        input: aq_session_api::InputRequest {
            terminal: permit.terminal.clone(),
            operation_id: operation,
            lease_id: permit.lease_id,
            expected_controller_generation: 8,
            expected_session_revision: 1,
            expires_at_ms: 5,
            bytes: b"x".to_vec(),
            escape_key: false,
        },
    };
    let document = DraftDocument::EpochInput(request.clone());
    document.validate().unwrap();
    out.insert("valid-epoch-input.json".into(), pretty(&document));
    let mut bad = request;
    bad.version = 2;
    out.insert(
        "invalid-epoch-version.json".into(),
        pretty(&DraftDocument::EpochInput(bad)),
    );
}

fn epoch_command(
    permit: &aq_terminal_relay::epoch::Permit,
    operation: Uuid,
) -> aq_terminal_relay::epoch::Command {
    aq_terminal_relay::epoch::Command {
        permit: permit.clone(),
        sequence: 1,
        input: InputCommand {
            operation_id: operation,
            lease: ControllerLease {
                terminal: permit.terminal.clone(),
                principal_id: text("fixture"),
                lease_id: permit.lease_id,
                generation: 8,
                issued_at_ms: 1,
                expires_at_ms: 9,
                execution_expires_at_ms: 10,
            },
            issued_at_ms: 2,
            deadline_ms: 5,
            bytes: b"x".to_vec(),
            escape_key: false,
        },
    }
}

fn epoch_downstream_artifacts(out: &mut Artifacts, command: &aq_terminal_relay::epoch::Command) {
    let envelope = Envelope {
        version: 1,
        capability: text(aq_terminal_relay::epoch::CAPABILITY),
        peer: Peer::Central {
            authority_id: id(1),
        },
        exchange_id: id(20),
        cursor: 1,
        effects: vec![Effect::EpochInput(command.clone())],
    };
    envelope.validate().unwrap();
    out.insert(
        "valid-epoch-downstream.json".into(),
        pretty(&DraftDocument::Downstream(envelope.clone())),
    );
    let mut unsupported = envelope;
    unsupported.capability = text(CAPABILITY);
    out.insert(
        "invalid-epoch-without-capability.json".into(),
        pretty(&DraftDocument::Downstream(unsupported)),
    );
}

fn poll_artifacts(out: &mut Artifacts) {
    let capabilities = vec![
        Capability {
            id: text(CAPABILITY),
            version: 1,
        },
        Capability {
            id: text(aq_terminal_relay::epoch::CAPABILITY),
            version: 1,
        },
    ];
    let poll = RelayPoll {
        version: 1,
        capability: text(aq_terminal_relay::epoch::CAPABILITY),
        peer: Peer::Remote {
            node_id: id(1),
            boot_id: id(2),
        },
        cursor: 0,
        hold_ms: 0,
        active_subscriptions: 0,
        active_controllers: 0,
        advertised_capabilities: capabilities,
    };
    poll.validate().unwrap();
    out.insert(
        "valid-poll.json".into(),
        pretty(&DraftDocument::Poll(poll.clone())),
    );
    let mut unsupported = poll;
    unsupported.advertised_capabilities.pop();
    out.insert(
        "invalid-poll-missing-epoch-capability.json".into(),
        pretty(&DraftDocument::Poll(unsupported)),
    );
}

fn epoch_upstream_artifacts(out: &mut Artifacts, command: &aq_terminal_relay::epoch::Command) {
    let permit = &command.permit;
    let upload = PeerUpload {
        version: 1,
        capability: text(aq_terminal_relay::epoch::CAPABILITY),
        peer: Peer::Central {
            authority_id: id(1),
        },
        exchange_id: id(21),
        cursor: 1,
        receipts: vec![],
        epoch_receipts: vec![aq_terminal_relay::epoch::Receipt {
            epoch_id: permit.epoch_id,
            sequence: 1,
            receipt: InputReceipt {
                terminal: permit.terminal.clone(),
                operation_id: command.input.operation_id,
                command_digest: canonical_digest(&command.input).unwrap(),
                disposition: InputDisposition::Applied,
            },
        }],
        resize_receipts: vec![],
        fence_acks: vec![],
        frames: vec![],
        snapshots: vec![],
        gaps: vec![],
    };
    upload.validate().unwrap();
    out.insert(
        "valid-epoch-upstream.json".into(),
        pretty(&DraftDocument::Upstream(upload)),
    );
}

fn upload_ack_artifact(out: &mut Artifacts, permit: &aq_terminal_relay::epoch::Permit) {
    let ack = UploadAck {
        version: 1,
        capability: text(aq_terminal_relay::epoch::CAPABILITY),
        peer: Peer::Central {
            authority_id: id(1),
        },
        exchange_id: id(21),
        cursor: 1,
        dropped_output: vec![],
        retirements: vec![aq_terminal_relay::epoch::Retirement {
            permit: permit.clone(),
            retired_through: 1,
        }],
    };
    ack.validate().unwrap();
    out.insert(
        "valid-upload-ack.json".into(),
        pretty(&DraftDocument::UploadAck(ack)),
    );
}

pub fn peer_artifacts() -> BTreeMap<String, String> {
    artifacts()
        .into_iter()
        .filter(|(name, bytes)| {
            name == "peer-schema.json"
                || name == "uuid-vectors.json"
                || serde_json::from_str::<Value>(bytes).ok().is_some_and(|v| {
                    matches!(
                        v["contract"].as_str(),
                        Some("downstream" | "upstream" | "poll" | "upload_ack" | "register")
                    )
                })
        })
        .collect()
}
