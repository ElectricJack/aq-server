---
id: sandbox
name: Sandbox
description: A node-owned container running the Claude harness, 15 minutes per attempt.
harnesses: [claude]
environment: aq-ms1-docker-local-claude
confinement: container
---
## Credentials
- claude: owner-claude-subscription

## Limits
- wall_time_ms: 900000
- memory_bytes: 4294967296
- cpu_millis: 2000
- process_limit: 512
- startup_context_bytes: 65536
- max_output_tokens: 32000
