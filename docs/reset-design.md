# Start-fresh implementation contract

Status: implementation contract for the reset backend, adapter and dashboard. See [Start fresh](start-fresh.md) for user-facing operation and rollout.

## Human-visible behavior

- A dashboard-only `r` action previews a reset of the instance's bound herdr space. It must not be an agent tool or an unrestricted socket reset command.
- Confirmation defaults to Cancel. Show the space, exact connected participant names/count, task/message counts, and TODO/PR detachment. Confirm explicitly (not a bare Enter on initial opening); Escape cancels.
- Reset means NEW active Pi sessions for the confirmed connected participants, restoring only approved role/name and normal policy/configuration. Old session files remain saved. It clears all hub tasks/messages for the space and detaches TODO/PR bindings, disabling all automation; it does not delete/change TODO files, repository files, roles, or other spaces.
- Disconnected historical sessions are NOT remotely cleared. Their old identities are retired so they cannot automatically reintroduce old context. Dialog must say so. External tools/jobs are NOT stopped; user must reconcile these before resetting.
- Fail preflight for working, human-paused, unsupported, or offered-delivery participants; connected participants must be idle. An adapter additionally rejects resetting with human input/drafts/dialogs/queued prompts/staged reports or unsettled insertion. Never interrupt a running model/tool to force reset.

## Durable hub boundary

The single-space Rust actor owns one persisted reset plan and all authoritative transitions. Capture exact space/inventory and revalidate at confirmation. Once prepared, freeze target-space messages, scheduling and all ordinary mutations (including cached mutations, Mode and reconfiguration); disable modes and advance watcher generation monotonically. No new participant may silently join the plan. Instances in other spaces continue normally.

The adapter must strip control fields before storing model-visible snapshots or returning any sync/roster tool result; no reset token may enter model context, notices, or general snapshots. A control directive exposed only to each bound participant through sync carries that participant's single-use reset token and immutable approved role/name/pane/old session identity. It is not an LLM message. The adapter needs negotiated reset capability. Existing clients remain compatible for ordinary use, but cannot participate in reset.

Before invoking Pi replacement, the adapter must atomically claim its token at the hub. The claim is persisted before success is returned. Each claim permits at most ONE newSession attempt. Repeated polling/reconnects, timeout, or restart must not cause automatic retries of an attempted reset. Report blocked/cancelled/failed/uncertain progress truthfully. Starting fresh Pi sessions is not atomic across agents; retain old hub records until all expected fresh-session receipts are validated.

A replacement rejoins with a fresh, different session ID plus its original token/plan identity. Validate space/pane/role/name and token against the persisted plan; reject unrelated/moved identities and reused tokens. Repeated acknowledgement of the SAME already-confirmed new identity may be idempotent, never authorize a second session. Only that verified fresh join is a completion receipt, not transport delivery or old-session acknowledgement.

After every expected participant has rejoined freshly, atomically clear target history/bindings/cache, retain replacement participants, and finalize. Zero connected participants may finish with hub-only clearing after the same explicit confirmation; do not describe offline contexts as cleared. Keep durable fencing of retired identities/connection incarnations so old messages/reports/requests cannot recreate cleared work or disconnect a replacement. Preserve monotonic watcher generations; never reset them to reusable defaults.

If any participant vetoes, disconnects, or becomes ambiguous, show an incomplete reset with per-agent progress and keep the space frozen and old hub data retained. No automatic second context-reset attempt. Do not claim rollback of already-created Pi sessions. Recovery requires explicit human review. A separately confirmed human-only `reset_abandon(plan_id)` retains hub history/bindings, leaves modes off, marks unresolved pending/offered deliveries uncertain and active assignments paused, and invalidates plan tickets/claimed old identities while retaining verified fresh identities. It releases the reset freeze so the human can reconcile affected agents with manual `/new` then `/role NAME`. It does not roll back contexts: an already-issued replacement may still finish, but its late receipt is rejected and that agent remains unjoined until manual recovery. There is no force-clear or automatic claimed-token retry. Restart preserves an incomplete plan/fence.

## Pi bridge

Use a registered adapter command dispatched with public `sendUserMessage(..., {expandPromptTemplates:true})` outside lifecycle callbacks to obtain a fresh ExtensionCommandContext. Raw `/new` sent as a user message is NOT a reliable command execution API. Gate both before dispatch and inside the command, using an adapter-held approval token (never arbitrary peer text).

Use `ctx.newSession` without copying prior branch, parent, summaries or custom entries. Perform any post-replacement work through `withSession`'s fresh context only. Restore approval data using a dedicated fresh-instance command; hub token verification is authoritative. Never retain/use stale Pi contexts. Cancellation must not claim success. Preserve policy tools and the base system prompt.

## Verification

- Real pinned SDK proof: fresh session ID, no old sentinel entries, real command dispatch/runtime replacement, no model turn, cancellation, fresh-context restore, unchanged policy resources.
- Rust: scoped clearing of ALL messages (not preview only), tasks, bindings and cache; persistent plan progress/recovery; stale sockets/IDs/dedup/deliveries/watcher ABA rejection; foreign-space rejection and independent-instance preservation; failed persistence does not publish success.
- Adapter: one-attempt claim, all safety gates, veto, duplicate/late token/receipt, topology changes, disconnect/partial completion and replacement role restore. No live agent operations.
- TUI: Cancel default, exact-space/inventory confirmation invalidation, clear warnings, status/progress, keyboard isolation from automation/navigation while modal is open, responsive native-color/no-color rendering.
