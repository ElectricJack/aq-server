---
id: deep-high
name: Deep, high
description: The hardest work, such as open-ended investigation and cross-cutting design.
---
## Candidates
| provider | model | reasoning | harnesses |
|---|---|---|---|
| anthropic | claude-fable-5-2 | high | claude |

## Selection
- max_attempts: 2
- fallback_on: [missing_credentials, unavailable, unsupported, stale_unknown, policy_denied]
- max_lineage_elapsed_ms: 3600000
- observation_max_age_ms: 30000
- allow_unknown_availability: false
- max_output_tokens: 32000
- required_capabilities: [aq.profiled-launch, aq.terminal, aq.harness.startup-context, aq.harness.container-confinement, aq.harness.stop]
- unknown_cost: allow_bounded
- require_complete_accounting: false
- require_hard_cost_enforcement: false
