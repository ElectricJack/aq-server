---
name: aq-comms
description: Read and send AQ v2 task, session, or supervisor mail, and ask or answer owner questions through the scoped CLI.
---

# AQ v2 communication

Inside a managed sandbox use `aq whoami` to identify your task, session and
supervisor conversation. Use `aq commands --all` to check grants. Messages
are external actions. Send only when the request or assignment authorizes it.

```sh
aq message inbox --to task
aq message inbox --to session
aq message inbox --to supervisor
aq message body "$message_id" --to task
aq message ack "$message_id" --to task --read
aq message send "$body" --to supervisor
aq message send "$reply" --to task --thread "$thread_id"
```

Keep replies in the original thread and mailbox. Use `task:ID` or
`supervisor:CONVERSATION_ID` only when the destination is known and granted.
Ack after reading. If a send is uncertain, keep its operation ID and inspect
it before sending again.

For a decision that requires the owner, give the blocker, viable choices,
your recommendation and what work can continue. In a sandbox run
`aq question ask "$question"`, then use `aq question show "$id"` or
`aq question wait "$id" --timeout 300`. A timed-out wait leaves the question
open. A task comment records context without sending a question.

On a host, use the connection flags in `aq-cli`. The owner can use
`aq messages inbox` for the supervisor inbox and `aq chat` for a supervisor
conversation. To read what a supervisor conversation holds, take its `id` from
`aq supervisor list` (one row per supervisor: the global one and each
project's) and read its messages:

```sh
aq supervisor list
aq conversation messages --conversation-id "$conversation_id" --limit 16
aq conversation messages --conversation-id "$conversation_id" --before "$cursor"
```

Each message prints its cursor, author kind (`human` or `supervisor`), time and
body; `--json` prints the whole page. A page holds at most 16 messages; when
more exist the output ends with the cursor to pass as `--before` (or `--after`
to read forward). A conversation's `revision` is not a message count: it is the
version of the conversation record, which settings changes advance, and it says
nothing about how many messages the conversation holds. A conversation with a
high revision can have no messages. Judge whether one has messages only from
`aq conversation messages`. To answer a question, inspect it and use
`aq question answer --help` with the exact decision ID and revision. Owner
answers and review decisions have different commands.
