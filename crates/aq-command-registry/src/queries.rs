//! The query params each GET command route accepts, keyed by route path.
//!
//! A read's published schema and the params `forward` lets through both come
//! from this table, so it must say what the handler's `Query<T>` accepts.
//! Each entry names its handler (paths under `crates/aq-server/src`). A
//! param the handler accepts but ignores is left out. Add an entry when a
//! GET command is added (`every_get_route_declares_its_query...` requires
//! one), and change it with the handler's query type.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryParam {
    pub name: &'static str,
    /// A JSON Schema for the value.
    pub schema: &'static str,
    pub required: bool,
}
const fn opt(name: &'static str, schema: &'static str) -> QueryParam {
    QueryParam {
        name,
        schema,
        required: false,
    }
}
const fn req(name: &'static str, schema: &'static str) -> QueryParam {
    QueryParam {
        name,
        schema,
        required: true,
    }
}

const UUID: &str = r#"{"type":"string","format":"uuid"}"#;
const CURSOR: &str = r#"{"type":"string","minLength":1,"description":"Opaque: the cursor a previous page returned"}"#;
const SCOPE: QueryParam = opt(
    "scope",
    r#"{"type":"string","pattern":"^(all_authorized|project:[0-9a-f-]{36})$","default":"all_authorized"}"#,
);
const PROJECT: QueryParam = opt("project_id", UUID);
const TEXT: &str = r#"{"type":"string","minLength":1,"maxLength":256,"description":"Text to match; not blank, no control characters"}"#;
const SEQUENCE: &str = r#"{"type":"integer","minimum":0,"maximum":9223372036854775807}"#;
const MILLIS: &str = r#"{"type":"integer","minimum":0,"description":"Unix time in milliseconds"}"#;
const DECIMAL: &str = r#"{"type":"string","pattern":"^[1-9][0-9]{0,18}$"}"#;
const DIGEST: &str = r#"{"type":"string","pattern":"^sha256:[0-9a-f]{64}$"}"#;
const STATE: QueryParam = opt("state", r#"{"enum":["open","closed"],"default":"open"}"#);

const NONE: &[QueryParam] = &[];
/// dashboard_api.rs `ReadQuery`, for the reads that use only its scope.
const SCOPED: &[QueryParam] = &[SCOPE];
/// dashboard_api.rs `ReadQuery`.
const DASHBOARD_PAGE: &[QueryParam] = &[
    SCOPE,
    opt(
        "limit",
        r#"{"type":"integer","minimum":1,"maximum":100,"default":25}"#,
    ),
    opt("cursor", CURSOR),
];
/// agent_access/human.rs `Page`.
const HUMAN_PAGE: &[QueryParam] = &[
    opt(
        "limit",
        r#"{"type":"integer","minimum":1,"maximum":64,"default":25}"#,
    ),
    opt("cursor", CURSOR),
    opt("q", TEXT),
];
const PLAYBOOK_LIMIT: QueryParam = opt(
    "limit",
    r#"{"type":"integer","minimum":1,"maximum":25,"default":25}"#,
);
const PLAYBOOK_AFTER: QueryParam = opt(
    "after",
    r#"{"type":"string","minLength":1,"maxLength":256,"description":"Opaque: where the previous page ended"}"#,
);
/// playbooks/inspection.rs `PageQuery`.
const PLAYBOOK_PAGE: &[QueryParam] = &[PLAYBOOK_AFTER, PLAYBOOK_LIMIT, PROJECT];
/// playbooks/inspection/catalog.rs `CatalogQuery`.
const PLAYBOOK_CATALOG: &[QueryParam] = &[
    PROJECT,
    PLAYBOOK_LIMIT,
    opt("exact", DIGEST),
    opt("after", DIGEST),
    opt("incarnation", DECIMAL),
    opt("metadata_revision", DECIMAL),
];
/// aq-terminal-relay api.rs `ViewQuery`.
const TERMINAL_VIEW: &[QueryParam] = &[
    req("subscription_id", UUID),
    req("after_sequence", SEQUENCE),
];
const RESULT_LIMIT: QueryParam = opt(
    "limit",
    r#"{"type":"integer","minimum":1,"maximum":100,"default":20}"#,
);
const RESULT_PROFILE: QueryParam = opt("profile", r#"{"enum":["recorded_native_v2"]}"#);

/// The declared query of the GET route at `path`. A supervisor read
/// (`/v1/supervisor/read/...`) is the owner's handler behind a gate that
/// passes the query string through, so it takes the owner route's params.
pub fn declared(path: &str) -> Option<&'static [QueryParam]> {
    let path = path.strip_prefix("/v1/supervisor/read").unwrap_or(path);
    Some(match path {
        // agent_access/human.rs::tasks, ::knowledge
        "/api/human/projects/{project}/tasks" | "/api/human/projects/{project}/knowledge" => {
            HUMAN_PAGE
        }
        // agent_access/human.rs::task; human/work.rs::summary, ::work
        "/api/human/projects/{project}/tasks/{node}"
        | "/api/human/projects/{project}/tasks/{node}/summary"
        | "/api/human/projects/{project}/tasks/{node}/work" => NONE,
        // agent_access/human.rs::graph (`GraphPage`)
        "/api/human/projects/{project}/tasks/{node}/graph" => {
            const {
                &[opt(
                    "limit",
                    r#"{"type":"integer","minimum":1,"maximum":63,"default":63}"#,
                )]
            }
        }
        // agent_access/human/work.rs::history (`HistoryPage`)
        "/api/human/projects/{project}/tasks/{node}/history" => const { &[opt("cursor", CURSOR)] },
        // agent_access/human/comments.rs::read (`Page`)
        "/api/human/projects/{project}/tasks/{node}/comments" => {
            const {
                &[
                    opt("cursor", CURSOR),
                    opt(
                        "limit",
                        r#"{"type":"integer","minimum":1,"maximum":100,"default":50}"#,
                    ),
                ]
            }
        }
        // agent_access/human.rs::document (`Revision`)
        "/api/human/projects/{project}/knowledge/{node}" => {
            const {
                &[opt(
                    "revision",
                    r#"{"type":"integer","minimum":1,"description":"An earlier revision; the current one when omitted"}"#,
                )]
            }
        }
        // agent_access/human/overview.rs::read (`QueryArgs`)
        "/api/human/overview/tasks" => {
            const {
                &[
                    PROJECT,
                    opt(
                        "project_limit",
                        r#"{"type":"integer","minimum":1,"maximum":8,"default":4}"#,
                    ),
                    opt(
                        "task_limit",
                        r#"{"type":"integer","minimum":1,"maximum":32,"default":16}"#,
                    ),
                    opt(
                        "after",
                        r#"{"type":"string","format":"uuid","description":"The last project of the previous page; not with project_id"}"#,
                    ),
                ]
            }
        }
        // agent_access/messaging/global/reads.rs::discovery; writes.rs::lookup; marker.rs::read
        "/api/human/conversations/global"
        | "/api/human/conversations/{conversation}/operations/{operation}"
        | "/api/human/conversations/{conversation}/read-marker" => NONE,
        // agent_access/messaging/global/reads.rs::history (`Page`)
        "/api/human/conversations/{conversation}/messages" => {
            const {
                &[
                    opt(
                        "before",
                        r#"{"type":"string","pattern":"^[1-9][0-9]*(\\.[1-9][0-9]*)?$","description":"A message position; not with after"}"#,
                    ),
                    opt(
                        "after",
                        r#"{"type":"string","pattern":"^(0|[1-9][0-9]*(\\.[1-9][0-9]*)?)$","description":"A message position; not with before"}"#,
                    ),
                    opt(
                        "limit",
                        r#"{"type":"integer","minimum":1,"maximum":16,"default":16}"#,
                    ),
                ]
            }
        }
        // dashboard_api.rs::bootstrap
        "/api/v1/bootstrap" => NONE,
        // dashboard_api.rs::capacity, ::overview, ::provider_usage, ::swarm,
        // ::operations, ::detail (`ReadQuery`; limit and cursor are refused or ignored)
        "/api/v1/capacity"
        | "/api/v1/overview"
        | "/api/v1/provider-usage"
        | "/api/v1/swarm"
        | "/api/v1/sessions/{id}/operations"
        | "/api/v1/sessions/{id}" => SCOPED,
        // dashboard_api.rs::projects, ::sessions (`ReadQuery`)
        "/api/v1/projects" | "/api/v1/sessions" => DASHBOARD_PAGE,
        // dashboard_api/search.rs::read (`QueryParams`)
        "/api/v1/search" => {
            const {
                &[
                    req("q", TEXT),
                    SCOPE,
                    opt(
                        "limit",
                        r#"{"type":"integer","minimum":1,"maximum":100,"default":25}"#,
                    ),
                    opt("cursor", CURSOR),
                    opt(
                        "types",
                        r#"{"type":"string","pattern":"^(task|session|result|playbook|knowledge)(,(task|session|result|playbook|knowledge))*$","description":"Comma-separated; every type when omitted"}"#,
                    ),
                ]
            }
        }
        // dashboard_api/actions/mod.rs::get_operation; the machine listener's
        // agent_access/human/enqueue/supervisor.rs::lookup; agent_access/mutations.rs::agent_get
        "/api/v1/operations/{id}" | "/v1/agent/operations/{operation_id}" => NONE,
        // aq-supervisor-control read.rs (headers only)
        "/v1/supervisor/projects" => NONE,
        // results_feedback/api.rs::draft_status, ::detail
        "/api/v1/feedback-tasks/{id}" | "/api/v1/results/{id}/revisions/{revision}" => NONE,
        // results_feedback/api.rs::comments (`Page`; only these two are not refused)
        "/api/v1/results/{id}/revisions/{revision}/comments" => {
            const { &[RESULT_LIMIT, opt("cursor", CURSOR)] }
        }
        // results_feedback/api.rs::feed (`Page`)
        "/api/v1/results" => {
            const {
                &[
                    opt(
                        "mode",
                        r#"{"enum":["latest","needs_review","following"],"default":"latest"}"#,
                    ),
                    opt(
                        "scope",
                        r#"{"enum":["all_authorized","project"],"description":"project needs project_id"}"#,
                    ),
                    PROJECT,
                    opt("task_id", UUID),
                    opt(
                        "media",
                        r#"{"enum":["any","image","video","none"],"default":"any"}"#,
                    ),
                    opt("since_ms", MILLIS),
                    opt("until_ms", MILLIS),
                    RESULT_LIMIT,
                    opt("cursor", CURSOR),
                ]
            }
        }
        // profiled_execution/human_results.rs::list (`Page`)
        "/api/human/results" => {
            const {
                &[
                    RESULT_PROFILE,
                    PROJECT,
                    opt(
                        "after",
                        r#"{"type":"string","format":"uuid","description":"The last result of the previous page"}"#,
                    ),
                    opt(
                        "limit",
                        r#"{"type":"integer","minimum":1,"maximum":25,"default":10}"#,
                    ),
                ]
            }
        }
        // profiled_execution/human_results.rs::detail (`Page`; after and limit do nothing here)
        "/api/human/results/{id}/revisions/{revision}" => const { &[RESULT_PROFILE, PROJECT] },
        // result_media/api.rs::list (refuses any query string)
        "/api/human/results/{report}/revisions/{revision}/media" => NONE,
        // notifications/api.rs::inbox (`Page`), ::inspect
        "/api/notifications" => {
            const {
                &[
                    opt(
                        "recipient",
                        r#"{"type":"string","format":"uuid","description":"The caller when omitted"}"#,
                    ),
                    opt("after", r#"{"type":"integer","minimum":0,"default":0}"#),
                    opt(
                        "limit",
                        r#"{"type":"integer","minimum":1,"maximum":64,"default":32}"#,
                    ),
                ]
            }
        }
        "/api/notifications/{message}" => NONE,
        // playbooks/inspection/catalog.rs::activations, ::definitions
        "/api/playbooks/inspection/catalog/activations"
        | "/api/playbooks/inspection/catalog/definitions" => PLAYBOOK_CATALOG,
        // playbooks/inspection.rs::admissions, ::detail (`PageQuery`)
        "/api/playbooks/inspection/admissions" | "/api/playbooks/inspection/runs/{id}" => {
            PLAYBOOK_PAGE
        }
        // playbooks/inspection.rs::runs (`RunsQuery`)
        "/api/playbooks/inspection/runs" => {
            const {
                &[
                    PLAYBOOK_AFTER,
                    PLAYBOOK_LIMIT,
                    PROJECT,
                    opt("since_ms", MILLIS),
                    opt(
                        "until_ms",
                        r#"{"type":"integer","minimum":1,"description":"Unix time in milliseconds; after since_ms"}"#,
                    ),
                ]
            }
        }
        // playbooks/inspection/definitions.rs::loaded_definitions (`DefinitionsQuery`)
        "/api/playbooks/inspection/loaded-definitions" => const { &[PROJECT] },
        // provider_accounts.rs::read
        "/api/human/provider-accounts" => NONE,
        // agent_access/questions/inbox.rs::list_http (`ListQuery`), ::detail_http
        "/api/questions/decisions" => {
            const {
                &[
                    STATE,
                    opt(
                        "limit",
                        r#"{"type":"integer","minimum":1,"maximum":50,"default":25}"#,
                    ),
                    PROJECT,
                    opt("cursor", CURSOR),
                ]
            }
        }
        "/api/questions/decisions/{id}" => NONE,
        // reviews/inbox.rs::inbox_http (`InboxQuery`); reviews.rs::inspect_http
        "/api/reviews/decisions" => {
            const {
                &[
                    PROJECT,
                    STATE,
                    opt(
                        "order",
                        r#"{"enum":["decision","impact"],"default":"decision","description":"impact needs state open"}"#,
                    ),
                    opt(
                        "limit",
                        r#"{"type":"integer","minimum":1,"maximum":50,"default":25}"#,
                    ),
                    opt(
                        "cursor",
                        r#"{"type":"string","minLength":1,"maxLength":4096,"description":"Opaque: the cursor a previous page returned"}"#,
                    ),
                ]
            }
        }
        "/api/reviews/decisions/{id}" => NONE,
        // observations/api.rs::events (`Page`), ::usage
        "/api/observations/sessions/{attempt}/events" => {
            const {
                &[
                    opt("after_sequence", SEQUENCE),
                    opt("through_sequence", SEQUENCE),
                    opt(
                        "limit",
                        r#"{"type":"integer","minimum":1,"maximum":16,"default":16}"#,
                    ),
                ]
            }
        }
        "/api/observations/sessions/{attempt}/usage" => NONE,
        // terminal/api.rs::output, ::tail (`ViewQuery`); ::inspect, ::receipt
        "/api/v1/terminals/{terminal}/output" | "/api/v1/terminals/{terminal}/tail" => {
            TERMINAL_VIEW
        }
        "/api/v1/terminals/{terminal}" | "/api/v1/terminals/{terminal}/receipts/{operation}" => {
            NONE
        }
        // agent_access/waits/routes.rs::owner_list (`ListQuery`), ::owner_show
        "/api/waits" => const { &[PROJECT] },
        "/api/waits/{wait_id}" => NONE,
        // project_bindings.rs::inspect, ::history
        "/api/projects/{id}" | "/api/projects/{id}/revisions/{revision}" => NONE,
        // agent_access/messaging/global/reads.rs::project_discovery
        "/api/human/conversations/projects/{project}" => NONE,
        // admin_api.rs::workers, ::status; execution/api.rs::inspect
        "/api/workers" | "/api/status" | "/api/execution/attempts/{id}" => NONE,
        // aq-supervisor-control api.rs::list
        "/api/supervisors" => NONE,
        // aq-manual-sessions api.rs::list (`Scope`)
        "/api/manual-agents" => const { &[PROJECT] },
        // profile_catalog/settings.rs::read; intelligence.rs::inspect
        "/api/settings/intelligence" | "/api/intelligence/registry" => NONE,
        // profile_catalog.rs::inspect, ::artifact
        "/api/profiles/catalogs" | "/api/profiles/catalogs/{digest}" => NONE,
        // repository_onboarding.rs::show; profiled_execution/disposition.rs::read;
        // aq-supervisor-control api.rs::operation; profile_catalog/settings.rs::operation
        "/api/repositories/onboard/{operation_id}"
        | "/api/execution/attempts/{id}/disposition"
        | "/api/supervisors/{id}/operations/{operation}"
        | "/api/settings/intelligence/operations/{id}" => NONE,
        _ => return None,
    })
}
