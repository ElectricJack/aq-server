use super::*;
use crate::{FallbackReason, UnknownCostPolicy};
use aq_session_api::Permission;
use uuid::Uuid;

const ROLE: &str = "---
id: developer
name: Developer
description: Makes the change a task asks for and reports evidence.
execution: sandbox
operations: [task.read, knowledge.read, source.write]
enabled: true
---
## Role
You implement the change your task describes.

```md
## Not a heading
```

## Rules
- Change only what the task asks for.
";
const INTELLIGENCE: &str = "---
id: standard-high
name: Standard, high
---
## Candidates
| provider | model | reasoning | harnesses |
|---|---|---|---|
| anthropic | claude-sonnet-5-5 | high | claude |
| openai | gpt-6 | medium | codex |
| openai | gpt-6 | default | codex, opencode |

## Selection
- max_attempts: 2
- fallback_on: [unavailable, unsupported]
- observation_max_age_ms: 30000
- allow_unknown_availability: false
- unknown_cost: deny
- require_complete_accounting: true
- require_hard_cost_enforcement: true
- max_cost_microunits: 5000000
- currency: USD
- max_uncharged_continuations: 1
";
const EXECUTION: &str = "---
id: sandbox
name: Sandbox
description: A container.
harnesses: [claude, codex]
environment: aq-ms1-docker-local-claude
confinement: container
---
## Credentials
- claude: owner-claude-subscription
- codex: owner-codex

## Limits
- wall_time_ms: 900000
- memory_bytes: 4294967296
- cpu_millis: 2000
- process_limit: 512
- startup_context_bytes: 65536
- max_output_tokens: 32000
";

#[cfg(not(feature = "project_environments"))]
fn env(environment: &str) -> String {
    environment.into()
}
#[cfg(feature = "project_environments")]
fn env(environment: &str) -> Option<String> {
    Some(environment.into())
}
/// Spec 0017 §5.3: a profile may leave the environment out; it then pins none.
#[cfg(feature = "project_environments")]
#[test]
fn an_execution_profile_may_name_no_environment() {
    let source = EXECUTION.replace("environment: aq-ms1-docker-local-claude\n", "");
    let profile = parse_execution(&source).unwrap();
    assert_eq!(profile.environment, None);
    assert_eq!(profile.to_markdown(), source);
    let mut s = set();
    s.execution[0].environment = None;
    assert!(validate(s, &defaults(), &limits()).is_ok());
}

fn role_error(source: &str) -> String {
    parse_role(source).unwrap_err().to_string()
}
fn limits() -> DocumentLimits {
    DocumentLimits::default()
}
fn defaults() -> SelectionDefaults {
    SelectionDefaults::default()
}
/// The shipped defaults, parsed.
fn set() -> DocumentSet {
    let mut set = DocumentSet::default();
    for d in default_documents() {
        match d.kind {
            Kind::Role => set.roles.push(parse_role(&d.text).unwrap()),
            Kind::Intelligence => set.intelligence.push(parse_intelligence(&d.text).unwrap()),
            Kind::Execution => set.execution.push(parse_execution(&d.text).unwrap()),
        }
    }
    set
}
fn problems(set: DocumentSet) -> Vec<String> {
    validate(set, &defaults(), &limits())
        .unwrap_err()
        .iter()
        .map(Problem::to_string)
        .collect()
}
#[track_caller]
fn assert_has(problems: &[String], expected: &str) {
    assert!(
        problems.iter().any(|p| p == expected),
        "missing `{expected}` in {problems:#?}"
    );
}

mod documents;
mod launch;
mod permissions;
mod snapshots;
mod startup;
mod validation;
