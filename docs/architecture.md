# Behavior, protocol, and safety

## One hub, separate spaces

A Rust process owns the socket, SQLite state, watcher jobs, scheduler, and optional Ratatui dashboard for one herdr server instance. Each live herdr space gets a tab and its own roster, messages, tasks, and automation switches. There is no hub model or central shared conversation.

herdr supplies topology, not agent reasoning. mustr uses only `pane.current` and `workspace.list`: no terminal scraping, simulated user input, launching participants, or killing participants. Live pane identity takes precedence over inherited workspace environment variables. Socket identity includes filesystem instance information so a replacement herdr server cannot inherit stale coordination accidentally.

Each explicitly joined Pi session keeps its ordinary history, policies, selected model, and tools. A thin adapter provides current scoped state and addressed messages. The dashboard opens on the launching pane's live herdr space (not an inherited workspace environment value), and users can still select any other space. The startup preference survives the initial empty topology snapshot; subsequent refreshes preserve manual selection, falling back only if the selected space disappears. Models receive only their space's roster/task summaries and their own incoming messages, not neighbors' transcripts.

## Participation and human control

Select a role with `/role NAME [PARTICIPANT-NAME]`. Role selection may precede hub startup: the adapter waits and reconnects with bounded backoff, without invoking a model. Connectivity, role selection, current activity, and task completion are separate facts. The dashboard's participant list counts and displays only connected agents (including paused ones). Offline identities remain stored for task/message name resolution and reconciliation; disconnecting does not erase unfinished tasks. A reconnect makes the participant visible again. Orderly leave/socket closure updates presence immediately; a lost heartbeat is detected after the 15-second stale threshold on a scheduler tick.

Assignments and automated prompts wait until Pi is settled. Live peer comments may enter an existing run at Pi's safe boundary, but queued input, editor drafts, dialogs, pauses, and human-initiated runs still gate delivery. Informational messages are inserted without triggering a turn. Actionable messages trigger or steer an ordinary Pi turn through the extension API.

- `/mustr pause` prevents incoming delivery; `/mustr resume` releases the local pause.
- Escape/abort or an assistant error pauses automated delivery.
- `mustr_pause` lets a model stop a blocked TODO/PR cycle and request a human decision; only the user resumes it.
- `/mustr leave` ends participation and disables mustr tools in that session.
- Session replacement, reload, fork/tree navigation, or a detected move to another herdr space ends participation. Rejoin explicitly after reconciliation. Even a subsequently cancelled navigation attempt may require rejoining.
- Turning a mode off cancels **pending** prompts of that kind. A prompt already offered to Pi or work already started cannot be recalled reliably.

Neither cancellation, leaving, losing a connection, nor closing the hub proves a remote tool/job stopped. Existing project policies continue to govern all work.

## Addressed work and ownership

Model-facing tools are `mustr_roster`, `mustr_send`, `mustr_assign`, `mustr_report`, `mustr_guide`, `mustr_accept`, `mustr_cancel`, and `mustr_pause`. Communication tools must be called alone, not alongside sibling tool calls. They are model-only, not exposed through nested tool orchestration.

A request message asks a question or requests attention; it does **not** assign ownership. An assignment records one requester, one owner, a title, scope, and revision. An owner can have only one unfinished assignment. New assignments require an idle, unpaused recipient; a running TODO driver cannot simultaneously accept an assignment.

Typical lifecycle:

```text
assigned → receipt → active → done report → review → requester accepts
                         ↘ blocked / waiting / paused → explicit guidance
```

A done report is not acceptance. Only the owner reports on its task. Only the requester guides, accepts, or cancels it. Guidance increments the revision; stale revisions fail. Cancellation closes hub bookkeeping, not external processes.

The adapter stages `blocked`, `waiting`, and `done` reports, terminates its current tool run, and publishes only after `agent_settled`. Further ordinary tool work is held until guidance or human direction. If interrupted before publication, the report is not replayed blindly. `status` is observational; `progress` explicitly resets the no-progress nudge budget but still requires evidence and independent review.

## Three independent modes

All modes are off after startup/recovery. Toggle them in the selected dashboard space with `t`, `c`, or `p`, or from Pi with `/mustr todo on|off`, `/mustr checkin on|off`, and `/mustr pr on|off`.

### TODO and unfinished-task continuation

Without an attached file, TODO mode nudges an idle owner only for an active assignment after the configured grace period. It does not resume blocked, waiting, review, paused, or disconnected work. Pending requests coalesce, and repeated nudges without progress pause the task for reconciliation.

`/mustr-todo [path/to/TODO.md]` attaches one explicit file and driver to the space, initially disabled. Nested Markdown checkboxes count; fenced examples do not. The hub reads but never edits the file. A reminder tells the driver to read the complete plan and independently verify criteria before checking boxes.

The driver is not nudged while it owns **or has delegated** unfinished tasks, preventing a second competing continuation. Unreadable/completed files and repeated cycles without checkbox progress disable the file cycle. New attachments replace the previous driver/file and cancel undelivered TODO prompts.

### Periodic check-ins

Check-in mode requests concise evidence, progress, remaining work, and blockers for active tasks at the configured interval. Delivery waits for an idle, unpaused owner; it never interrupts a running tool. A long-running busy session is therefore observable in the dashboard but is not forcibly interrupted by a check-in. Check-ins and continuation share the task's bounded nudge budget. Transport heartbeats do not invoke models or count as progress.

### PR watching

`/mustr-pr https://github.com/OWNER/REPO/pull/NUMBER` attaches an explicit PR, checkout, and recipient, initially disabled. Only GitHub.com canonical URLs are supported. The hub runs bounded, read-only `git rev-parse`, `gh pr view`, and paginated `gh api .../pulls/.../comments` commands; it cannot commit, push, merge, comment, rerun CI, or change branches.

Observations cover conversation comments, review decisions, inline comments/replies, and failed check/status results. Fingerprints include edited content. Existing feedback is baselined on first observation; existing failed checks are actionable. Checks are tied to the remote head. The local branch must match the PR branch, and both local and remote identity are rechecked during collection.

A baseline advances only with a complete observation and, for actionable deltas, confirmed message receipt. Disabling/re-enabling the same watch preserves its baseline. Replacing an in-flight offered PR notification is rejected until receipt/disconnection resolves it. New watcher generations discard late results. Old pending feedback is refreshed before delivery rather than acted on after a long pause.

Closure/merge disables watching. A failed or inconsistent poll, including branch changes or authentication errors, pauses the watcher and cancels pending feedback; inspect the dashboard error and explicitly re-enable or reattach it. GitHub content is untrusted evidence. Notifications are bounded previews, not a replacement for reading full linked feedback. `gh pr view` governs availability of conversation/review data; mustr does not claim a complete archival export of arbitrary-sized PRs.

## Durability and wire protocol

Protocol v1 is UTF-8 newline-delimited JSON over a same-user Unix socket. Each connection first sends `join` with protocol version, session ID, role/name, and pane/space IDs. The server checks the live pane and its Pi agent designation. Subsequent operations bind to that sender; recipients and task IDs cannot cross spaces.

```json
{"id":"unique-request-id","op":"send","to":"recipient-session-id","text":"Please review task X","wake":true}
```

Responses carry the request ID, `ok`, and either `result` or `error`. `sync` reports activity and may offer one inbox message; `ack` confirms its receipt. The Rust `Command` enum in `src/protocol.rs` is the complete versioned command schema. Unknown command fields are rejected.

SQLite is the single writer's transaction boundary. Each operation applies to a cloned state. Durable mutations and message offers persist atomically before replies; presence heartbeats remain ephemeral and no-op ticks do not rewrite the database. Failed transitions cannot partially commit. Corrupt or incompatible state fails closed, rather than resetting silently.

Addressed peer requests, information, and status/blocked/done reports use push-driven receipt, without waiting for the whole run to end or a polling tick. After explicit `subscribe`, Rust sends a content-free `{"event":"inbox_ready"}` socket hint when the bound recipient has eligible queued work. Hints are recipient-scoped and coalesced; unsubscribed clients receive no extra frames. Readiness uses the complete queue, not the dashboard's 200-message preview. The adapter wakes its serialized pump immediately; the normal validated `sync` transaction still owns offering. One-second polling remains a fallback and heartbeat, not the primary delivery mechanism. Local process/socket scheduling and Pi's safe insertion boundary still impose latency; this is not a real-time execution guarantee. Actionable messages use Pi's steering queue at its next safe boundary; informational messages are appended after the current turn's tool results without forcing a new model turn. This is not cancellation of a running tool. Assignments, task resumptions, and automated TODO/check-in/PR prompts remain idle-only; eligible live comments can bypass those queued messages. Human input, editor drafts, pending Pi messages, dialogs, pauses, and staged terminal reports still gate delivery. Already-inserted/queued messages are not recalled by a later pause.

The adapter keeps heartbeating while Pi waits to insert a live message. Rust enforces at most one offered message per recipient, even if a client repeatedly requests another. `sync` supplies the Rust-selected `insertion_policy` (`live` or `idle_only`); the adapter rechecks local human/activity gates before insertion. If input or work races with the offer, `defer` returns it without disconnecting, but only before any insertion attempt. Rust discards deferred prompts whose task/revision or automation mode has been superseded. Once insertion is attempted, deferral/replay is forbidden. A long model/tool call is not treated as an insertion timeout; an initially idle offer that becomes working loses its short deadline. Idle delivery retains its bounded confirmation timeout.

Delivery states are **pending**, **offered**, **received**, **uncertain**, and **cancelled**. The adapter records an **attempt marker**, not a receipt, before injecting a message. Only a matching `custom_message` entry on its current session branch proves insertion and permits acknowledgement. Pi 1.2's informational (`triggerTurn:false`) path persists messages and emits public SDK events without calling extension `message_end`; waiting for that extension callback caused false timeouts/disconnects. Polling the actual entry covers both informational and actionable delivery without forcing a model turn. A marker alone prevents reinjection but never authorizes acknowledgement. Receipt is neither execution nor completion; there is no exactly-once model-execution guarantee.

Rust validates persisted/wire states before constructing private, non-Clone `Pending`/`Offered` transition capabilities. Consuming an offer produces receipt effects that can activate the matching assignment revision and advance a PR baseline once. Only offered messages can be deferred; uncertain/received/cancelled messages cannot. These are compile-time constraints inside the delivery module, not a claim that Rust can prove an external Pi insertion or eliminate runtime validation at JSON/SQLite boundaries. The v1 join reply advertises `delivery_policy: 1` and `inbox_notifications: 1`; the updated adapter requires both, so update/restart the hub before rejoining with the updated adapter. Incoming hints and lifecycle events coalesce into one pump; they never overlap a running pump or bypass human gates.

A disconnect/restart marks pending/offered deliveries uncertain and pauses active/assigned work. Reconnection does not replay those messages. TODO/check-in modes intentionally skip paused work, even with an idle, reconnected owner. The dashboard shows the pause reason and requester responsible for recovery: inspect files, job status, and task notes, then have that requester explicitly guide or cancel. `/mustr resume` releases a participant's local pause; it does not resume a paused assignment. Do not blindly reassign uncertain work. A cancelled completion **notification** does not mean the task was cancelled: acceptance can supersede a queued done notification while leaving the task accepted. The adapter never retries a timed-out mutation automatically. Request deduplication retains the most recent 1024 mutations; it is not an indefinite replay guarantee.

## Trust, storage, and current limits

This is a cooperative **same-user** system, not authentication against hostile processes running as your UID. Private directories, socket permissions, instance checks, and live-pane checks limit accidental routing and other-user access; another same-user program can impersonate a participant. Roles do not sandbox filesystem/tool access.

SQLite stores participant identities, task descriptions/notes, addressed message text, delivery state, TODO paths, PR URLs, and feedback fingerprints. Pi stores delivered messages and receipt markers in its own session. No private neighbor transcripts are read. State is not encrypted; protect the user account and backups.

Current bounds include 256 KiB frames, 6000-byte user message text, 256 remembered participants, 1000 tasks, 1000 retained messages, 1024 deduplication records, a 16 MiB state document, and a 1 MiB TODO file. Completed/cancelled messages can be pruned at capacity; unresolved deliveries are not silently dropped. Roster replies prioritize unfinished/owned tasks, return at most 64 task summaries, mark truncation, and bound note previews. The dashboard displays the latest 200 messages. Message bodies are 240-character previews with an explicit truncation marker; scrolling clamps to the wrapped content bounds.

A full state/history limit requires stopping the hub and intentionally archiving its per-server SQLite files before starting fresh; reconcile outstanding work first. There is no live garbage-collection/reset command. Local Unix sockets only: no Windows support, cross-machine routing, distributed consensus, or unattended manufacture/production approval. herdr API compatibility was inspected against 0.9.5; Pi typechecking uses 1.2.0. End-to-end tests use fake topology/agents, not paid models.
