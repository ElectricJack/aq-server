# Full workspace results

Command: `cargo test --locked --workspace --no-fail-fast -- --test-threads=2`

Exit: 101. Source: `43212ff9371125b6f089b329757370eb972e6390` (code unchanged from prerequisite merge `46fb7a19`).

| Test target | Result | Passed | Failed | Ignored | Duration |
| --- | --- | ---: | ---: | ---: | --- |
| `Running unittests src/lib.rs (target/debug/deps/aq_accounting-3d790cca4dbc3b80)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/accounting.rs (target/debug/deps/accounting-5df40e937765c451)` | ok | 20 | 0 | 0 | 0.04s |
| `Running unittests src/lib.rs (target/debug/deps/aq_admission-d4aa2cbb50b51595)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/common.rs (target/debug/deps/common-b9b93eec2e15811e)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/contracts.rs (target/debug/deps/contracts-16be2adb65edac71)` | ok | 7 | 0 | 0 | 0.01s |
| `Running tests/control.rs (target/debug/deps/control-a9b14b731c3f39f2)` | ok | 10 | 0 | 0 | 0.06s |
| `Running tests/control_common.rs (target/debug/deps/control_common-2d2b70a0670d4544)` | ok | 0 | 0 | 0 | 0.01s |
| `Running tests/execution_spec.rs (target/debug/deps/execution_spec-f946237ae354bb52)` | ok | 3 | 0 | 0 | 0.02s |
| `Running tests/granted_closure.rs (target/debug/deps/granted_closure-2a47349ad1d15565)` | ok | 5 | 0 | 0 | 0.10s |
| `Running tests/granted_closure_common.rs (target/debug/deps/granted_closure_common-bc3da037efa3208c)` | ok | 0 | 0 | 0 | 0.01s |
| `Running tests/outcome.rs (target/debug/deps/outcome-995eddc4c737533e)` | ok | 6 | 0 | 0 | 0.02s |
| `Running tests/placement.rs (target/debug/deps/placement-c8776362156732ed)` | ok | 5 | 0 | 0 | 0.03s |
| `Running tests/preparation_refresh.rs (target/debug/deps/preparation_refresh-fb219bd29232a980)` | ok | 15 | 0 | 0 | 1.46s |
| `Running tests/preparation_refresh_common.rs (target/debug/deps/preparation_refresh_common-8614b103e5ce6014)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/preparation_v2.rs (target/debug/deps/preparation_v2-f462b4712dabf842)` | ok | 16 | 0 | 0 | 1.37s |
| `Running tests/preparation_v2_baseline.rs (target/debug/deps/preparation_v2_baseline-137557b9485c18ab)` | ok | 1 | 0 | 0 | 0.69s |
| `Running tests/preparation_v2_common.rs (target/debug/deps/preparation_v2_common-24df94e834b32cba)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/result_record.rs (target/debug/deps/result_record-0d3e1bc7757a64ce)` | ok | 3 | 0 | 0 | 0.01s |
| `Running tests/scoped_presence.rs (target/debug/deps/scoped_presence-6396911953b04ff2)` | ok | 9 | 0 | 0 | 0.03s |
| `Running tests/scoped_presence_common.rs (target/debug/deps/scoped_presence_common-30cc7d7c04b90033)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/spec_common.rs (target/debug/deps/spec_common-b315da2e2d9f91ba)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/started.rs (target/debug/deps/started-60051cbd92c0067a)` | ok | 3 | 0 | 0 | 0.01s |
| `Running tests/started_common.rs (target/debug/deps/started_common-d995d19dec99f39a)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/startup.rs (target/debug/deps/startup-3f3dfa015c0ce7c9)` | ok | 6 | 0 | 0 | 0.03s |
| `Running tests/startup_common.rs (target/debug/deps/startup_common-a0a4a6d62d0e8b4c)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/stopped.rs (target/debug/deps/stopped-0c3f8e6e24c38893)` | ok | 4 | 0 | 0 | 0.01s |
| `Running tests/task_lineage.rs (target/debug/deps/task_lineage-341a6c8f68f22548)` | ok | 2 | 0 | 0 | 0.00s |
| `Running tests/transport.rs (target/debug/deps/transport-e1c861ce5a5e65ae)` | ok | 4 | 0 | 0 | 0.01s |
| `Running unittests src/lib.rs (target/debug/deps/aq_agent_api-6eb5a745399c02ef)` | ok | 1 | 0 | 0 | 0.00s |
| `Running tests/contract.rs (target/debug/deps/contract-3a4902ca23e67c57)` | ok | 4 | 0 | 0 | 0.03s |
| `Running unittests src/lib.rs (target/debug/deps/aq_central_authority-84e557e42caa509b)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/authority.rs (target/debug/deps/authority-f95d32d0654d0798)` | ok | 16 | 0 | 0 | 0.23s |
| `Running unittests src/lib.rs (target/debug/deps/aq_human_http-76d4d9cd772c83dc)` | ok | 5 | 0 | 0 | 0.15s |
| `Running unittests src/lib.rs (target/debug/deps/aq_lifecycle_api-5188afc62cd85929)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/conformance.rs (target/debug/deps/conformance-601387772ee48d90)` | ok | 12 | 0 | 0 | 0.02s |
| `Running unittests src/lib.rs (target/debug/deps/aq_notifications-698146ced7464593)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/delivery.rs (target/debug/deps/delivery-5b6b2525f027514e)` | ok | 5 | 0 | 0 | 0.01s |
| `Running tests/discord.rs (target/debug/deps/discord-231cf28f37ece98c)` | ok | 3 | 0 | 0 | 0.01s |
| `Running unittests src/main.rs (target/debug/deps/aq_notify-8a00788e8e63bd88)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/cli.rs (target/debug/deps/cli-15b978adaa2e42c6)` | ok | 0 | 0 | 0 | 0.00s |
| `Running unittests src/lib.rs (target/debug/deps/aq_observation_admission-07478bff413f0521)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/ownership.rs (target/debug/deps/ownership-074bea472701207e)` | ok | 10 | 0 | 0 | 0.02s |
| `Running unittests src/lib.rs (target/debug/deps/aq_observation_api-af63e3a89bb5ae6a)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/views.rs (target/debug/deps/views-1b6d6ba3e01c7369)` | ok | 4 | 0 | 0 | 0.01s |
| `Running unittests src/lib.rs (target/debug/deps/aq_phase_terminal_protocol-bd95d13c6a941d1a)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/common.rs (target/debug/deps/common-c4d695dcb1223088)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/contracts.rs (target/debug/deps/contracts-69b9dbfa64c4f055)` | ok | 12 | 0 | 0 | 1.16s |
| `Running tests/frozen_artifact.rs (target/debug/deps/frozen_artifact-765709ac04f71f4e)` | ok | 1 | 0 | 0 | 0.01s |
| `Running tests/refresh_common.rs (target/debug/deps/refresh_common-1fad1cff1adfcc0d)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/refresh_contracts.rs (target/debug/deps/refresh_contracts-2538f1fa308b3a0e)` | ok | 14 | 0 | 0 | 1.05s |
| `Running unittests src/lib.rs (target/debug/deps/aq_playbook_host-1b6b08dac699ee22)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/catalog.rs (target/debug/deps/catalog-f7c6a93c457cc0b2)` | ok | 3 | 0 | 0 | 0.01s |
| `Running tests/owner.rs (target/debug/deps/owner-10268fa204a4b991)` | ok | 5 | 0 | 0 | 0.03s |
| `Running tests/postgres.rs (target/debug/deps/postgres-73b4a672fa7ceae0)` | ok | 0 | 0 | 1 | 0.00s |
| `Running tests/protection.rs (target/debug/deps/protection-e47d5c761b0d6635)` | ok | 1 | 0 | 0 | 0.01s |
| `Running tests/registered.rs (target/debug/deps/registered-8b51e91bc7a2c171)` | ok | 2 | 0 | 0 | 0.02s |
| `Running unittests src/lib.rs (target/debug/deps/aq_profiles-16fd515fd4dc6af4)` | ok | 0 | 0 | 0 | 0.00s |
| `Running unittests src/main.rs (target/debug/deps/aq-d0701a54c6ebd66d)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/central.rs (target/debug/deps/central-af0fdecdc3a19abf)` | ok | 14 | 0 | 0 | 0.19s |
| `Running tests/cli.rs (target/debug/deps/cli-5f71801941946027)` | ok | 3 | 0 | 0 | 0.07s |
| `Running tests/compiler.rs (target/debug/deps/compiler-fe7f29a0524fd6a9)` | ok | 11 | 0 | 0 | 0.08s |
| `Running tests/registry.rs (target/debug/deps/registry-c3aba97909b0164f)` | ok | 8 | 0 | 0 | 0.06s |
| `Running tests/task_baseline.rs (target/debug/deps/task_baseline-452626fe49c5b6f0)` | ok | 1 | 0 | 0 | 0.08s |
| `Running unittests src/lib.rs (target/debug/deps/aq_release-30560748eb674adb)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/manifest.rs (target/debug/deps/manifest-c4f68234201c8e06)` | ok | 17 | 0 | 0 | 0.01s |
| `Running tests/spool.rs (target/debug/deps/spool-8f128de716eaf831)` | ok | 5 | 0 | 0 | 0.00s |
| `Running unittests src/lib.rs (target/debug/deps/aq_result_media-1e880d515e7d9f3b)` | ok | 1 | 0 | 0 | 0.00s |
| `Running tests/codec.rs (target/debug/deps/codec-f9934e0a95cc28d3)` | ok | 13 | 0 | 0 | 1.45s |
| `Running tests/domain.rs (target/debug/deps/domain-88e8679fed122530)` | ok | 10 | 0 | 0 | 0.01s |
| `Running unittests src/lib.rs (target/debug/deps/aq_review_protocol-2d0dc4218a55046a)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/conformance.rs (target/debug/deps/conformance-7c20feb90e6d4a78)` | ok | 11 | 0 | 0 | 0.03s |
| `Running unittests src/lib.rs (target/debug/deps/aq_selection-4d2414c503aaef2b)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/central.rs (target/debug/deps/central-c139bbc8182f1f68)` | ok | 14 | 0 | 0 | 0.90s |
| `Running tests/selection.rs (target/debug/deps/selection-1cbe9501471c1bff)` | ok | 23 | 0 | 0 | 0.15s |
| `Running tests/task_baseline.rs (target/debug/deps/task_baseline-42ae55c88ffb5155)` | ok | 1 | 0 | 0 | 0.22s |
| `Running unittests src/lib.rs (target/debug/deps/aq_server-a22abeff305308c3)` | ok | 181 | 0 | 1 | 75.23s |
| `Running unittests src/main.rs (target/debug/deps/aq_server-60c146a3849727b1)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/actual_codex_task.rs (target/debug/deps/actual_codex_task-bc7634858eb9f2cb)` | ok | 17 | 0 | 1 | 0.14s |
| `Running tests/actual_owner_setup.rs (target/debug/deps/actual_owner_setup-d25f345634679f94)` | ok | 3 | 0 | 0 | 0.02s |
| `Running tests/agent_access.rs (target/debug/deps/agent_access-f3c3d5e8e1228d16)` | ok | 1 | 0 | 0 | 2.17s |
| `Running tests/authz.rs (target/debug/deps/authz-30c1c9fd93eefb5d)` | ok | 13 | 0 | 0 | 10.08s |
| `Running tests/cli.rs (target/debug/deps/cli-3ca1d550d42f8bfa)` | ok | 4 | 0 | 0 | 1.27s |
| `Running tests/codex_finite_provider.rs (target/debug/deps/codex_finite_provider-97bdaaedf402659c)` | ok | 2 | 0 | 0 | 0.10s |
| `Running tests/codex_runtime_recipe.rs (target/debug/deps/codex_runtime_recipe-13db303f5c6a8874)` | ok | 6 | 0 | 0 | 0.00s |
| `Running tests/config.rs (target/debug/deps/config-0fa576b326e95d1a)` | ok | 13 | 0 | 0 | 0.02s |
| `Running tests/dashboard_actions.rs (target/debug/deps/dashboard_actions-b9817fbd19e5abc6)` | ok | 8 | 0 | 0 | 6.24s |
| `Running tests/dashboard_assets.rs (target/debug/deps/dashboard_assets-3d75913764df9153)` | ok | 3 | 0 | 0 | 2.91s |
| `Running tests/dashboard_mounts.rs (target/debug/deps/dashboard_mounts-7f9c2e86d9485c39)` | ok | 3 | 0 | 0 | 3.47s |
| `Running tests/dashboard_notifications.rs (target/debug/deps/dashboard_notifications-393096796a43628f)` | ok | 1 | 0 | 0 | 2.36s |
| `Running tests/dashboard_read.rs (target/debug/deps/dashboard_read-8e5b4124fb0bb9e7)` | ok | 6 | 0 | 0 | 5.32s |
| `Running tests/dashboard_usage.rs (target/debug/deps/dashboard_usage-f68e1b586d53d096)` | ok | 2 | 0 | 0 | 2.41s |
| `Running tests/enrollment.rs (target/debug/deps/enrollment-b832d6956af1d4f1)` | ok | 15 | 0 | 0 | 13.60s |
| `Running tests/execution.rs (target/debug/deps/execution-4612f5d4fe44ec98)` | ok | 27 | 0 | 0 | 18.45s |
| `Running tests/granted_closure.rs (target/debug/deps/granted_closure-083b8f36ccb7d06d)` | ok | 10 | 0 | 0 | 29.50s |
| `Running tests/graph_mutations.rs (target/debug/deps/graph_mutations-ac9f5dd4a99222f8)` | ok | 12 | 0 | 1 | 10.61s |
| `Running tests/health.rs (target/debug/deps/health-000f4566abe4de8c)` | ok | 1 | 0 | 0 | 0.00s |
| `Running tests/human_graph.rs (target/debug/deps/human_graph-98678462875fb138)` | ok | 1 | 0 | 1 | 1.09s |
| `Running tests/human_task_create.rs (target/debug/deps/human_task_create-ffcbe9b7a14bc6be)` | ok | 2 | 0 | 1 | 1.66s |
| `Running tests/identity.rs (target/debug/deps/identity-d247b46471e5f2f0)` | ok | 13 | 0 | 0 | 3.91s |
| `Running tests/intelligence.rs (target/debug/deps/intelligence-06157b854a2b02da)` | ok | 4 | 0 | 0 | 1.06s |
| `Running tests/messaging.rs (target/debug/deps/messaging-a8592d40888c7343)` | ok | 11 | 0 | 0 | 10.61s |
| `Running tests/native_accepted_preparation.rs (target/debug/deps/native_accepted_preparation-05fe5ad61a94428e)` | FAILED | 15 | 1 | 0 | 54.79s |
| `Running tests/native_cancellation.rs (target/debug/deps/native_cancellation-43a1a4692e3f5743)` | ok | 5 | 0 | 0 | 13.11s |
| `Running tests/native_definitions.rs (target/debug/deps/native_definitions-14e6babd14f34a99)` | ok | 3 | 0 | 0 | 1.60s |
| `Running tests/native_expiry.rs (target/debug/deps/native_expiry-f9e337b8569e213e)` | ok | 6 | 0 | 0 | 5.09s |
| `Running tests/native_initial_grant.rs (target/debug/deps/native_initial_grant-7e3e955f5826c45a)` | FAILED | 35 | 6 | 13 | 142.40s |
| `Running tests/native_inventory.rs (target/debug/deps/native_inventory-cb4a189589c22b38)` | ok | 1 | 0 | 0 | 1.86s |
| `Running tests/native_material_issuance.rs (target/debug/deps/native_material_issuance-a5c4329f280d8bd3)` | ok | 1 | 0 | 0 | 1.29s |
| `Running tests/native_offers.rs (target/debug/deps/native_offers-f39d4c59a94c07d3)` | ok | 4 | 0 | 0 | 7.64s |
| `Running tests/native_preparation.rs (target/debug/deps/native_preparation-5331e5c616150710)` | ok | 11 | 0 | 0 | 26.43s |
| `Running tests/native_proposal_scope.rs (target/debug/deps/native_proposal_scope-de32c12c6debec42)` | ok | 1 | 0 | 0 | 2.71s |
| `Running tests/native_reconciliation.rs (target/debug/deps/native_reconciliation-2869092776361e5b)` | ok | 7 | 0 | 0 | 17.52s |
| `Running tests/native_registration.rs (target/debug/deps/native_registration-1210df7162502a15)` | ok | 4 | 0 | 0 | 3.03s |
| `Running tests/native_result_startup.rs (target/debug/deps/native_result_startup-2765c943805c296c)` | ok | 3 | 0 | 0 | 8.85s |
| `Running tests/native_runner_transport.rs (target/debug/deps/native_runner_transport-4b7ae5534e4f25f3)` | ok | 4 | 0 | 0 | 0.00s |
| `Running tests/native_source.rs (target/debug/deps/native_source-b892fe95cfc8e7ee)` | ok | 6 | 0 | 0 | 6.31s |
| `Running tests/never_granted.rs (target/debug/deps/never_granted-fd3ee5085ef5bc18)` | ok | 7 | 0 | 0 | 11.76s |
| `Running tests/notification_discord_live.rs (target/debug/deps/notification_discord_live-27663f70f7936616)` | ok | 0 | 0 | 1 | 0.00s |
| `Running tests/notification_store.rs (target/debug/deps/notification_store-dcc9cc64efb2bac2)` | ok | 15 | 0 | 0 | 15.45s |
| `Running tests/observation_owner_peer.rs (target/debug/deps/observation_owner_peer-210e34749891812a)` | ok | 0 | 0 | 2 | 0.00s |
| `Running tests/observation_transport.rs (target/debug/deps/observation_transport-31bc6b85aced0b97)` | ok | 7 | 0 | 0 | 6.43s |
| `Running tests/observations.rs (target/debug/deps/observations-96cf002139185552)` | ok | 11 | 0 | 0 | 9.50s |
| `Running tests/phase_execution.rs (target/debug/deps/phase_execution-f5da02ca1a3c3c58)` | FAILED | 13 | 1 | 0 | 56.88s |
| `Running tests/phase_execution_transport.rs (target/debug/deps/phase_execution_transport-287d78aa94c9b067)` | FAILED | 3 | 1 | 0 | 12.74s |
| `Running tests/phase_initial_lease.rs (target/debug/deps/phase_initial_lease-b5a2248fcae5c50b)` | ok | 1 | 0 | 0 | 7.57s |
| `Running tests/phase_never_granted.rs (target/debug/deps/phase_never_granted-b85b30d7c2e1b74a)` | ok | 3 | 0 | 0 | 9.64s |
| `Running tests/phase_refresh_execution.rs (target/debug/deps/phase_refresh_execution-9f3acbf3ee6cfaf8)` | ok | 9 | 0 | 0 | 58.64s |
| `Running tests/phase_refresh_terminal.rs (target/debug/deps/phase_refresh_terminal-e211a99c05489496)` | ok | 7 | 0 | 0 | 27.38s |
| `Running tests/phase_results.rs (target/debug/deps/phase_results-f84dd70510a4a7d6)` | FAILED | 25 | 3 | 0 | 174.13s |
| `Running tests/phase_source_retention.rs (target/debug/deps/phase_source_retention-9c69eaf7becacddb)` | ok | 0 | 0 | 2 | 0.00s |
| `Running tests/phase_terminal_closure.rs (target/debug/deps/phase_terminal_closure-ffdec4cedf453c5f)` | FAILED | 0 | 4 | 0 | 5.97s |
| `Running tests/phase_terminal_fixture.rs (target/debug/deps/phase_terminal_fixture-72aa2b2db86d6769)` | ok | 1 | 0 | 0 | 5.34s |
| `Running tests/phase_terminal_history.rs (target/debug/deps/phase_terminal_history-04c92a068e5f0956)` | ok | 4 | 0 | 0 | 16.49s |
| `Running tests/phase_terminal_started.rs (target/debug/deps/phase_terminal_started-31eb61b6e9fae3c6)` | ok | 10 | 0 | 0 | 48.92s |
| `Running tests/phase_terminal_stopped.rs (target/debug/deps/phase_terminal_stopped-61ed2ffd2eb22dfc)` | FAILED | 3 | 1 | 0 | 17.79s |
| `Running tests/phase_terminal_transport.rs (target/debug/deps/phase_terminal_transport-63c480a636c5c31d)` | ok | 3 | 0 | 0 | 2.23s |
| `Running tests/playbook_activation.rs (target/debug/deps/playbook_activation-4a4b3e92c46a0911)` | ok | 15 | 0 | 1 | 19.82s |
| `Running tests/playbook_bridge.rs (target/debug/deps/playbook_bridge-08d8632b4d65453a)` | ok | 11 | 0 | 0 | 13.24s |
| `Running tests/playbook_contract.rs (target/debug/deps/playbook_contract-d182724a850e6d11)` | ok | 2 | 0 | 0 | 0.00s |
| `Running tests/playbook_integration_startup.rs (target/debug/deps/playbook_integration_startup-293a8062eab1d717)` | ok | 2 | 0 | 0 | 1.66s |
| `Running tests/playbook_isolation.rs (target/debug/deps/playbook_isolation-cc8ce99bc7bd641e)` | ok | 1 | 0 | 0 | 1.41s |
| `Running tests/playbook_review_contract.rs (target/debug/deps/playbook_review_contract-7041581d1d2fb508)` | ok | 4 | 0 | 0 | 0.03s |
| `Running tests/profile_catalog.rs (target/debug/deps/profile_catalog-2ca582c87ea9931c)` | ok | 4 | 0 | 0 | 2.79s |
| `Running tests/profile_inspection.rs (target/debug/deps/profile_inspection-d4c6545412fb1277)` | ok | 2 | 0 | 0 | 1.39s |
| `Running tests/profiled_artifact_phases.rs (target/debug/deps/profiled_artifact_phases-2c0f1da6e9451804)` | ok | 4 | 0 | 0 | 4.01s |
| `Running tests/profiled_artifacts.rs (target/debug/deps/profiled_artifacts-2b2866e3b042c6f2)` | ok | 4 | 0 | 0 | 4.27s |
| `Running tests/profiled_authority.rs (target/debug/deps/profiled_authority-3dfffe0450805d48)` | ok | 7 | 0 | 0 | 5.70s |
| `Running tests/profiled_config.rs (target/debug/deps/profiled_config-3d0a1f0f4699bbe7)` | ok | 2 | 0 | 0 | 0.00s |
| `Running tests/profiled_context.rs (target/debug/deps/profiled_context-ec6670ffa39ac6c9)` | ok | 1 | 0 | 0 | 1.25s |
| `Running tests/profiled_hot.rs (target/debug/deps/profiled_hot-439a2027daceb896)` | FAILED | 4 | 1 | 0 | 7.36s |
| `Running tests/profiled_material.rs (target/debug/deps/profiled_material-9da3bb045719e7ae)` | ok | 6 | 0 | 0 | 7.95s |
| `Running tests/profiled_results.rs (target/debug/deps/profiled_results-034af8115d6d8c27)` | FAILED | 67 | 1 | 0 | 117.58s |
| `Running tests/profiled_source.rs (target/debug/deps/profiled_source-520101b077b3e8f9)` | ok | 1 | 0 | 0 | 2.08s |
| `Running tests/profiled_source_exports.rs (target/debug/deps/profiled_source_exports-40fcf9a6141f8426)` | ok | 9 | 0 | 0 | 11.70s |
| `Running tests/profiled_started.rs (target/debug/deps/profiled_started-1b9c3f69bc6d4738)` | ok | 6 | 0 | 0 | 9.01s |
| `Running tests/profiled_stopped.rs (target/debug/deps/profiled_stopped-f938de495a7152a3)` | ok | 8 | 0 | 0 | 8.47s |
| `Running tests/project_bindings.rs (target/debug/deps/project_bindings-8f4564835a7ff9c6)` | ok | 12 | 0 | 1 | 22.43s |
| `Running tests/readiness.rs (target/debug/deps/readiness-177f923e9af32d7f)` | ok | 5 | 0 | 0 | 4.47s |
| `Running tests/real_graph_fixture.rs (target/debug/deps/real_graph_fixture-5a7803403761a7e9)` | ok | 1 | 0 | 1 | 0.00s |
| `Running tests/result_origin_migration.rs (target/debug/deps/result_origin_migration-5a602147967068d9)` | ok | 0 | 0 | 1 | 0.00s |
| `Running tests/scheduling_reads.rs (target/debug/deps/scheduling_reads-73365cefffba6131)` | ok | 4 | 0 | 0 | 0.05s |
| `Running tests/schema26_27.rs (target/debug/deps/schema26_27-4eb450cdf7916bc9)` | ok | 6 | 0 | 0 | 5.34s |
| `Running tests/schema28_closure.rs (target/debug/deps/schema28_closure-d5014b34bd8e07b7)` | ok | 3 | 0 | 0 | 4.84s |
| `Running tests/schema29_presence.rs (target/debug/deps/schema29_presence-331febd3893345a3)` | ok | 2 | 0 | 0 | 1.42s |
| `Running tests/schema30_preparation.rs (target/debug/deps/schema30_preparation-d72e6beb4248957b)` | FAILED | 3 | 1 | 0 | 2.42s |
| `Running tests/schema31_presence_barrier.rs (target/debug/deps/schema31_presence_barrier-675100a1cea6aa2d)` | ok | 3 | 0 | 0 | 2.47s |
| `Running tests/schema32_phase_execution.rs (target/debug/deps/schema32_phase_execution-6998a29bfb9346ac)` | FAILED | 3 | 1 | 0 | 2.88s |
| `Running tests/schema33_phase_terminal.rs (target/debug/deps/schema33_phase_terminal-16b6bd5d0ff418d2)` | FAILED | 1 | 8 | 0 | 18.03s |
| `Running tests/schema41_compute.rs (target/debug/deps/schema41_compute-a53610d57604fbb2)` | ok | 2 | 0 | 0 | 4.19s |
| `Running tests/schema43_cancellation.rs (target/debug/deps/schema43_cancellation-b9b8f67484f84774)` | FAILED | 1 | 2 | 0 | 8.13s |
| `Running tests/scoped_presence_inventory.rs (target/debug/deps/scoped_presence_inventory-2099fa623ee8b9ad)` | ok | 6 | 0 | 0 | 16.09s |
| `Running tests/session_inspection.rs (target/debug/deps/session_inspection-60e90ba781fb2136)` | ok | 4 | 0 | 0 | 4.20s |
| `Running tests/settlement_migration.rs (target/debug/deps/settlement_migration-2c4eab7bca3269fa)` | ok | 0 | 0 | 1 | 0.00s |
| `Running tests/source_retention.rs (target/debug/deps/source_retention-ac041488706a5d20)` | ok | 10 | 0 | 1 | 12.68s |
| `Running tests/store.rs (target/debug/deps/store-2a4931066266be55)` | ok | 14 | 0 | 0 | 9.66s |
| `Running tests/supervisor_conversation.rs (target/debug/deps/supervisor_conversation-bd218cd2f5a1bf7d)` | ok | 6 | 0 | 0 | 6.55s |
| `Running tests/terminal_output_poll.rs (target/debug/deps/terminal_output_poll-c116e5ddc2f80c8c)` | ok | 3 | 0 | 0 | 6.03s |
| `Running tests/terminal_store.rs (target/debug/deps/terminal_store-7163a8edd474f49e)` | ok | 21 | 0 | 0 | 37.98s |
| `Running tests/terminal_tail.rs (target/debug/deps/terminal_tail-9d3e92e6ad59c164)` | FAILED | 4 | 1 | 0 | 33.32s |
| `Running tests/updates.rs (target/debug/deps/updates-c60bb490b8222087)` | ok | 22 | 0 | 0 | 20.36s |
| `Running tests/worker_api.rs (target/debug/deps/worker_api-be3d0a5e9dc8b752)` | ok | 15 | 0 | 0 | 11.92s |
| `Running tests/worker_auth_context.rs (target/debug/deps/worker_auth_context-c18570bc37d65d34)` | ok | 2 | 0 | 0 | 1.70s |
| `Running unittests src/lib.rs (target/debug/deps/aq_session_api-74bf56cadf5c8ff3)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/publication.rs (target/debug/deps/publication-f0b1469daaf8ce0f)` | ok | 12 | 0 | 0 | 0.03s |
| `Running unittests src/main.rs (target/debug/deps/aq_session-783664fa0983d74d)` | ok | 1 | 0 | 0 | 0.01s |
| `Running tests/http.rs (target/debug/deps/http-8ce17476c0b7b058)` | ok | 0 | 0 | 0 | 0.00s |
| `Running unittests src/lib.rs (target/debug/deps/aq_terminal_relay-d00ca5c238056a10)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/fake_peers.rs (target/debug/deps/fake_peers-6c6289a87924f063)` | ok | 24 | 0 | 0 | 2.78s |
| `Running tests/publication.rs (target/debug/deps/publication-f576009cd548acac)` | ok | 1 | 0 | 0 | 0.12s |
| `Running unittests src/lib.rs (target/debug/deps/aq_terminal_relay_protocol-160bde02100a61a6)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/artifacts.rs (target/debug/deps/artifacts-e5b1206e5699bfc1)` | ok | 1 | 0 | 0 | 0.01s |
| `Running unittests src/lib.rs (target/debug/deps/aq_updater-faa422d49c29add1)` | ok | 25 | 0 | 0 | 0.59s |
| `Running unittests src/main.rs (target/debug/deps/aq_updater-8ec7ebcc5871388f)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/cli.rs (target/debug/deps/cli-9a13668675ac2a1a)` | ok | 14 | 0 | 0 | 3.46s |
| `Running tests/config.rs (target/debug/deps/config-bf21465042061c81)` | ok | 8 | 0 | 0 | 0.04s |
| `Running tests/engine.rs (target/debug/deps/engine-b085c698c7237dd7)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/journal.rs (target/debug/deps/journal-f935fe9cb83a5952)` | ok | 22 | 0 | 0 | 1.80s |
| `Running tests/spool.rs (target/debug/deps/spool-f1fd080219234c4b)` | ok | 3 | 0 | 0 | 0.09s |
| `Running unittests src/lib.rs (target/debug/deps/aq_wait_engine-d41e13dd61096cec)` | ok | 17 | 0 | 0 | 0.24s |
| `Running unittests src/lib.rs (target/debug/deps/aqv2-2de51f2232d1a221)` | ok | 0 | 0 | 0 | 0.00s |
| `Running unittests src/main.rs (target/debug/deps/aqv2-ecc3124070d3b3c3)` | ok | 0 | 0 | 0 | 0.00s |
| `Running tests/cli.rs (target/debug/deps/cli-5a73de7abb8aa33c)` | ok | 16 | 0 | 0 | 0.52s |
| `Doc-tests aq_accounting` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_admission` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_agent_api` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_central_authority` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_human_http` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_lifecycle_api` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_notifications` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_observation_admission` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_observation_api` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_phase_terminal_protocol` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_playbook_host` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_profiles` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_release` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_result_media` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_review_protocol` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_selection` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_server` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_session_api` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_terminal_relay` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_terminal_relay_protocol` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_updater` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aq_wait_engine` | ok | 0 | 0 | 0 | 0.00s |
| `Doc-tests aqv2` | ok | 0 | 0 | 0 | 0.00s |
