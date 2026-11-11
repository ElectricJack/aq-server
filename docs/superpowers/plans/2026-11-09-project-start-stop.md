# Clear project Start/Stop controls

Owner feedback: project start/stop controls are unclear. Implement the controls and locally qualify them; no further approval gate is needed for this reversible UI/API work.

- Settings → Projects shows compact rows with Started/Stopped/Setup needed and Start project or Stop project beside the name. Keep raw identities, profiles and connection timestamps in a disclosure. Link Manage projects from the worker-launch form.
- Reuse existing owner binding POST and disable routes with exact operation identity, expected revision and unchanged settings. Retain uncertain operations per principal/authorization/project and retry the same request. Expose refusal text and prevent accidental duplicate clicks.
- Report the configured worker-policy ceiling in project reads so enabling a binding is not misrepresented as completed execution setup. Projects without setup stay visible with an explanation; do not fabricate source policies or enable repositories as a side effect of UI delivery.
- Stop uses the existing disable semantics: no new launches and current permission checks may interrupt running agents. It is not a guaranteed drain-to-completion. State this visibly. Exclude stopped bindings from launch choices and refuse new manual launches; exact old receipts stay replayable.
- Reuse existing Rust integration binaries and private worktree target; move pure validation into aq-profiles to keep the server line ratchet falling. Test control/retry/conflict flows, project lifecycle, and the actual Graph launch boundary. Inspect live desktop/390/320 screenshots, verify the served bundle, update only the isolated AQ server/dashboard, and preserve owner services/v1/Supabase.
