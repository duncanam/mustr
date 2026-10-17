# Start fresh in one space

> This requires both the updated hub and Pi adapter. Source changes do not update installed or running components.

The dashboard's `r` action opens **Start fresh** for its selected herdr space. It is a human dashboard action, not an agent tool. Merely opening the dialog does nothing.

Read the space name, participant list, counts and warnings. Scroll if necessary, then type `RESET` and press Enter to confirm. Empty Enter or Escape cancels the confirmation. Space navigation and automation shortcuts do not act behind the dialog.

## What changes

After every confirmed connected participant acknowledges a genuinely fresh Pi session:

- All hub tasks and messages in that space are cleared, including history outside the message preview.
- TODO and PR bindings are detached, and TODO continuation, check-ins and PR watching are disabled.
- Connected, supported Pi agents have fresh active conversations and restored approved roles/names. Normal Pi policies, tools and configuration remain in place; prior conversation/summary content is not copied.

Other spaces are untouched. TODO files, project files, roles/configuration and saved Pi session files are **not deleted or changed**. This is a fresh active workspace, not secure erasure of historical files or backups.

Disconnected historical agents cannot be remotely cleared. Their old identities are retired so they cannot automatically rejoin with stale context; use a fresh Pi session and explicitly select a role when bringing them back.

## Safety and incomplete resets

Reconcile agents and external jobs first. Reset does **not** stop external jobs, kill tools, or undo their effects. Working, human-paused or unsupported agents and unresolved offered deliveries block the initial reset; reconcile and resume participants until they are idle first. Adapters also refuse to discard human drafts, queued prompts or other unsettled activity.

While resetting, the space is frozen against new coordination work. `r` shows per-agent progress. Closing the status dialog does not stop a reset. A transport receipt or idle participant is not proof of fresh context: completion requires a validated new Pi session identity.

Multi-agent session replacement cannot be atomic. If an agent vetoes, disconnects or becomes uncertain, the hub retains history and displays an incomplete reset. Already-created sessions remain fresh; attempted replacements are not automatically repeated. Restarting the hub does not silently retry or clear the incomplete state.

From an incomplete reset's status dialog, `a` opens a separate **Abandon attempt** confirmation. Read it and type `ABANDON` to release the reset freeze while retaining hub history and bindings. Automation stays off; unresolved deliveries become uncertain and active assignments remain paused for reconciliation. This is **not rollback**: it cannot restore old contexts or recall a replacement already issued to Pi. Late receipts for revoked tickets are rejected. Affected agents may need manual `/new` followed by `/role NAME`.

After abandoning, inspect each agent, task and external job before choosing another fresh-start attempt or explicitly resuming work. Do not blindly replay uncertain messages or assignments.

## Updating existing installations

The reset handshake requires both updated hub and Pi adapter code. Rebuilding source alone updates neither running component. Install the new hub/adapter and reload/rejoin Pi only after reconciling current work; follow [development rollout instructions](development.md#applying-dashboard-and-adapter-changes). A hub with older adapters should reject reset as unsupported rather than falsely report that agent context was cleared.

The updated hub migrates the state database to schema version 2, even if you have not used reset. Older binaries must refuse this database: they cannot honor its durable reset fences and retired identities. Back up the state directory before upgrading if you need a rollback path; do not bypass the version check or restore a backup over a running hub. A backup also restores old coordination history, not the live state of Pi sessions or external jobs.
