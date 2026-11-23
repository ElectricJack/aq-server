//! Questions, waits and conversation messages.
use super::*;

pub(super) const COMMANDS: &[CommandSpec] = &[
    agent(
        mutation(
            "question.ask",
            op::QUESTION_ASK,
            TargetRule::Own,
            "Ask the owner a question; wait on it with question.show",
        ),
        Route::post(
            "/v1/agent/questions",
            Body::Versioned,
            Params::Inline(
                r#"{"type":"object","additionalProperties":false,"required":["operation_id","question"],"properties":{"operation_id":{"type":"string","format":"uuid"},"question":{"type":"string","minLength":1,"maxLength":4096}}}"#,
            ),
        ),
    ),
    agent(
        read(
            "question.show",
            op::QUESTION_READ,
            TargetRule::Own,
            "One question, with the answer once recorded",
        )
        .with_owner(Some(Route::get(
            "/api/questions/decisions/{id}",
            &[("id", "decision_id")],
        ))),
        Route::post(
            "/v1/agent/questions/show",
            Body::Versioned,
            Params::Inline(
                r#"{"type":"object","additionalProperties":false,"required":["decision_id"],"properties":{"decision_id":{"type":"string","format":"uuid"}}}"#,
            ),
        ),
    ),
    owner(
        "question.list",
        op::QUESTION_READ,
        "Open and answered agent questions",
        "/api/questions/decisions",
    ),
    CommandSpec {
        agent: Some(Route::post(
            "/v1/agent/waits/register",
            Body::Params,
            Params::Ref("crates/aq-lifecycle-api/artifacts/request.schema.json"),
        )),
        ..mutation(
            "wait.register",
            op::WAIT_REGISTER,
            TargetRule::Own,
            "Register an H4 wait for this attempt (a sealed lifecycle request)",
        )
    },
    CommandSpec {
        agent: Some(Route::post(
            "/v1/agent/waits/inspect",
            Body::Params,
            Params::Ref("crates/aq-lifecycle-api/artifacts/request.schema.json"),
        )),
        ..read(
            "wait.inspect",
            op::WAIT_READ,
            TargetRule::Own,
            "Inspect this attempt's wait (a sealed lifecycle request)",
        )
    },
    CommandSpec {
        agent: Some(Route::post(
            "/v1/agent/waits/ack",
            Body::Params,
            Params::Ref("crates/aq-lifecycle-api/artifacts/request.schema.json"),
        )),
        ..mutation(
            "wait.ack",
            op::WAIT_ACK,
            TargetRule::Own,
            "Acknowledge a continuation (a sealed lifecycle request)",
        )
    },
    agent(
        owner(
            "wait.list",
            op::WAIT_READ,
            "Registered waits (an agent's own attempt's)",
            "/api/waits",
        ),
        Route::post(
            "/v1/agent/waits/list",
            Body::Versioned,
            Params::Inline(r#"{"type":"object","additionalProperties":false}"#),
        ),
    ),
    agent(
        read(
            "wait.show",
            op::WAIT_READ,
            TargetRule::Own,
            "One registered wait (an agent's own attempt's)",
        )
        .with_owner(Some(Route::get("/api/waits/{wait_id}", &[]))),
        Route::post(
            "/v1/agent/waits/show",
            Body::Versioned,
            Params::Inline(
                r#"{"type":"object","additionalProperties":false,"required":["wait_id"],"properties":{"wait_id":{"type":"string","format":"uuid"}}}"#,
            ),
        ),
    ),
    CommandSpec {
        owner: Some(Route::post(
            "/api/messages/send",
            Body::Params,
            Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageSend"),
        )),
        agent: Some(Route::post(
            "/v1/agent/messages/send",
            Body::Params,
            Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageSend"),
        )),
        ..mutation(
            "message.send",
            op::MESSAGE_SEND,
            TargetRule::Mailbox("recipient"),
            "Send mail to a task, session or supervisor mailbox",
        )
    },
    CommandSpec {
        owner: Some(Route::post(
            "/api/messages/inbox",
            Body::Params,
            Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageInbox"),
        )),
        agent: Some(Route::post(
            "/v1/agent/messages/inbox",
            Body::Params,
            Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageInbox"),
        )),
        ..read(
            "message.inbox",
            op::MESSAGE_READ,
            TargetRule::Mailbox("recipient"),
            "A mailbox page",
        )
    },
    CommandSpec {
        agent: Some(Route::post(
            "/v1/agent/messages/body",
            Body::Params,
            Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageBodyRequest"),
        )),
        ..read(
            "message.body",
            op::MESSAGE_READ,
            TargetRule::Mailbox("recipient"),
            "One message body, in bounded chunks",
        )
    },
    CommandSpec {
        agent: Some(Route::post(
            "/v1/agent/messages/ack",
            Body::Params,
            Params::Ref("protocol/agent-access/v1/schema.json#/$defs/MessageAck"),
        )),
        ..mutation(
            "message.ack",
            op::MESSAGE_ACK,
            TargetRule::Mailbox("recipient"),
            "Acknowledge a message as received or read",
        )
    },
    read(
        "conversation.messages",
        op::MESSAGE_READ,
        TargetRule::Own,
        "A conversation's messages",
    )
    .with_owner(Some(Route::get(
        "/api/human/conversations/{conversation}/messages",
        &[("conversation", "conversation_id")],
    ))),
    read(
        "conversation.read_marker",
        op::MESSAGE_READ,
        TargetRule::Own,
        "A conversation's read marker",
    )
    .with_owner(Some(Route::get(
        "/api/human/conversations/{conversation}/read-marker",
        &[("conversation", "conversation_id")],
    ))),
    read(
        "conversation.operation",
        op::MESSAGE_READ,
        TargetRule::Own,
        "A conversation send operation",
    )
    .with_owner(Some(Route::get(
        "/api/human/conversations/{conversation}/operations/{operation}",
        &[
            ("conversation", "conversation_id"),
            ("operation", "operation_id"),
        ],
    ))),
];
