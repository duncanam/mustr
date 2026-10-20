# Development

mustr is a private-module Rust application, not a published library API. Clap and Serde derive the CLI and typed configuration/wire contracts. TOML is for user settings and role front matter; JSON is for socket frames and versioned internal SQLite records.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
npm --prefix pi ci --ignore-scripts
npm --prefix pi run typecheck
npm --prefix pi test
node --test claude/test/*.test.mjs
```

Adapter tests require Node 22.19+ (Pi's engine requirement and native TypeScript stripping). The macOS/Linux PTY smoke test additionally uses `uv`, creating its Python environment in a temporary project with no third-party dependencies. It runs only the test-owned hub; it never attaches to user terminals.

To verify the minimum Rust compiler, Node engine requirement, and Linux PTY lifecycle in an isolated Linux environment:

```sh
docker build --progress=plain -f tests/linux.Dockerfile -t mustr-validation .
```

This runs Rust tests, TypeScript checking, and the full Node integration suite with Rust 1.89, Node 22.19, Python, and `uv`. No host sockets, credentials, Pi configuration, or hub state are mounted. Docker's build cache may reuse successful test layers; use `--no-cache` to force a complete rerun.

`npm test` builds the debug Rust binary, then exercises the real socket service against an in-process fake herdr server, temporary SQLite/configuration directories, fake participant clients, and a mocked Pi extension host. Fixtures cover concurrent per-space hubs, wrong-endpoint and cross-space rejection, task exchange, rollback, independent restart/shutdown, uncertain restart recovery, same-space singleton locking, pane movement, TODO delegation waits, human input/abort, delayed terminal reports, preserved policy tools, transport framing, and terminal restoration. Rust tests also cover PR surfaces/fingerprints, stale generations, baselines, and watcher errors. The subprocess integration test supplies fake `git`/`gh` executables and checks the exact read-only commands and feedback delivery; no test contacts GitHub.

`pi/test/pi-receipts.test.mjs` calls the pinned Pi dependency's real `AgentSession.sendCustomMessage`/flush/event methods with an in-memory `SessionManager` and no resource discovery or model. It verifies that context-only insertion persists a `custom_message` but does not emit extension `message_end`; an attempt marker and abandoned branches are not receipts. The adapter harness mirrors that distinction. Integration regressions cover information-before-assignment, long busy insertion, input/offer races, marker-only attempts, and late insertion after restart. Push tests stretch the fallback poll to ten seconds and verify delivery before it can fire, concurrent wakeup serialization, opt-in/recipient scoping, and preservation of a partial request frame across socket notifications. Rust tests cover typed delivery transitions, one outstanding offer, safe deferral, disabled/superseded prompts, and explicit recovery of paused work. TUI tests cover recovery hints, cancellation semantics, truncated previews, bounded scrolling, fixed space identity, renamed/unavailable topology, and inert former space-switching keys. The macOS/Linux PTY smoke test checks binding to the live launch space despite a stale inherited workspace variable.

Automatic PR discovery regressions cover no-target current-branch lookup, pinning without later rediscovery, absent/closed/malformed PRs, branch/HEAD races, and stale results after mode/reset/connection changes. Selection tests preserve the displayed participant identity, checkout and opening epoch, while allowing ordinary heartbeat changes. Picker tests cover modal isolation, no eligible participant, and reaching the final long-named checkout in a narrow/short terminal. Adapter fixtures assert checkout advertisement on ordinary and fresh-reset joins; persisted participant records and peer roster JSON omit that live metadata.

Input-boundary regressions distinguish ordinary accepted user work from explicit pause/abort/error, retain pending input across out-of-order human/extension-origin user messages, and preserve terminal-report/reset human-intent rules. Handled/rejected preflight input remains fail-closed until explicit idle resume.

`pi/test/adapter-cache.test.mjs` drives the production adapter through real pinned `AgentSession`/`Agent` runs with a no-network fake stream. It verifies exact model-facing SDK-context prefixes across roster-tool continuations and subsequent runs despite changing synthetic heartbeats, stable system/tool declarations, preserved host policies, and role/tool deactivation after leave. This checks SDK request context, not serialized provider HTTP payloads or actual cache hits.

Tests do not call paid models, install adapters into live Pi/Claude configurations, operate neighboring agents, or mutate Git repositories. Running `cargo install --path . --locked` and `mustr install pi` is a separate, explicit user action. No legacy extensions are removed by this repository.

Composable-role regressions cover ordered/shared graph expansion, invalid headers and names, cycle/duplicate diagnostics, resource bounds, and non-destructive initialization. `tests/roles.test.mjs` checks targeted CLI resolution and isolated invalid-role listing; socket tests reject invalid initial/reconnect/fresh-session graphs before admission, including retaining an authentic reset receipt for a valid retry. `pi/test/adapter-roles.test.mjs` exercises inactive pending selections, late admission rejection, explicit retry, and reconnect prompt refresh with the production CLI/socket service. Tests bind binary/config overrides at adapter factory creation so fixture setup cannot accidentally select an earlier imported environment.

Fresh-start regressions span three boundaries: Rust reset/actor/store tests check scoped persistence, fences and failure recovery; `pi/test/adapter-reset.test.mjs` exercises actual adapter handlers against controlled race/transport fixtures; `pi/test/adapter-runtime.test.mjs` uses the production adapter with real pinned Pi command/session replacement APIs and an isolated Unix-socket peer, proving fresh context and receipt-gated role activation without a model. `tests/reset-ui.test.mjs` plus its uv-owned PTY helper exercise actual human confirmation/cancellation and scoped hub-only clearing. These do not claim a live installed multi-agent reset or termination of external jobs.

## Claude adapter checks

`tests/harness.test.mjs` covers mixed Pi/Claude task exchange, exact supported herdr designations, and connection invalidation on harness replacement or mismatched pane identity. `tests/claude-install.test.mjs` launches the installed stable launcher and immutable adapter from a temporary directory (including spaces/quotes), performs an MCP handshake/tool listing, and checks isolated user profiles and permissions fixtures. A fake Claude CLI covers first registration, same-directory upgrades without a second add, default/overridden profile paths, unmanaged conflicts, malformed/symlinked profiles, missing/failing CLI recovery, and `--config-only` without invoking Claude. HOME, profile directory, and cwd are test-owned; Linux tests do not require a native Claude executable. These tests run in the existing Node integration suite, along with `tests/claude-protocol.test.mjs` for initialization gates, framing, metadata, optional arguments, and protocol/operation errors. `claude/test/` covers the bridge's hub behavior; run it after `cargo build --locked`. An independent temporary smoke with the official MCP TypeScript SDK 1.29.0 also verified initialization, listing all 12 tools, optional arguments, progress metadata, and unjoined operation errors; the SDK is not a runtime dependency.

Claude's adapter deliberately does not advertise reset capability or infer delivery receipt from stdout writes. It cannot observe native Claude session boundaries, editor input, or ordinary tool settlement; see [Claude Code limits](claude-code.md). No tests claim those unsupported capabilities or run paid models. A no-model health-check attempt with installed Claude Code 2.1.302, a temporary HOME/config/project, and a copied generated MCP config reported **Pending approval**; it was stopped without bypassing approval. That confirms config discovery, not a native Claude handshake or model tool execution. The direct `--mcp-config` host workflow still needs an explicitly approved live smoke test. Updating a running Claude instance requires explicitly restarting/reloading its MCP connection after reconciling active work.

## Applying dashboard and adapter changes

Source changes do not update a running hub or Pi extension. Each instance now persists one space in its own version-3 database; old shared multi-space history is not imported or modified. Reconcile outstanding work and incomplete resets before stopping the old hub, then start one new instance in each intended space. Never restore state over a running hub. See [Start fresh](start-fresh.md) for scope and recovery. After reconciling active tasks, rebuild/install the binary (`cargo install --path . --locked`), update the adapter with `mustr install pi`, then restart the hub and use `/reload` in participating Pi sessions. Reload ends participation; explicitly `/role NAME` again. Hub restart disables automation and marks ambiguous deliveries/work for reconciliation; do not blindly reassign them. Do not interrupt active agents just to refresh presentation. The receipt/push fix requires the new hub's `delivery_policy: 1` and `inbox_notifications: 1` capabilities and the updated adapter; rebuilding source alone changes neither running component. Already-uncertain deliveries stay uncertain, and paused tasks still require explicit requester guidance after reconciliation.

Automatic branch-PR discovery requires both the updated hub and an adapter that advertises its checkout on join. Update/restart the hub before rejoining updated adapters; older hubs reject the added `cwd` field. Discovery runs in the existing generation-fenced watcher, not in the actor or a blocking socket request. A successful enable starts discovery; only complete validated observation pins a PR and permits feedback. Missing participant checkout metadata in older version-3 records defaults safely to unavailable until rejoin; existing explicit watch bindings remain readable.

The human-input and prompt-context corrections are Pi-adapter changes: an already-compatible running hub does not need restarting for those fixes alone. Rebuild the binary to embed the updated adapter, explicitly run `mustr install pi`, and reload/rejoin participating Pi sessions only after reconciling active work. Source edits and passing tests do not patch already-running extensions.

The installer writes immutable `transport.<hash>.mjs` modules and rewrites the installed entry point's import URL, so native ESM caching cannot retain a different transport after `/reload`. Old transport files remain untouched; changed entry points receive content-addressed `.bak` backups. A corrupt file at an immutable transport URL fails installation instead of being overwritten. No Pi settings or policies are changed. See the [validation session checklist](validation.md) before updating participating sessions.

## Source map

- `cli`, `config`, `paths`, `roles`: configuration, validation, and editable roles.
- `protocol`, `hub`: typed envelopes and deterministic state transitions.
- `delivery`: Rust insertion policy, consuming pending/offered guards, and receipt-only task/PR effects; external observations still require runtime validation.
- `store`, `service`: SQLite and transactional single-writer actor.
- `herdr`, `server`: topology, private sockets, leases, and watcher supervision.
- `watchers`: bounded TODO parsing and read-only GitHub observations.
- `tui`: terminal-native dashboard; no model invocation. See [dashboard design](dashboard-design.md) for visual direction and review criteria.
- `install`: bundled, explicit adapter installation with backups; `install/claude` generates immutable assets, a stable managed launcher, optional standalone config, and persistent user-scoped registration through Claude's CLI.
- `pi/index.ts`, `pi/transport.mjs`: Pi host lifecycle integration and shared addressed transport.
- `claude/index.mjs`: dependency-free stdio MCP adapter with explicit participation, bounded waits, and observed-message acknowledgement.

The declared minimum Rust version is 1.89 (standard-library file locking). The Claude compatibility changes passed 122 Rust tests, 97 Pi/shared integration tests, and 8 Claude lifecycle tests on macOS, and the same suites plus TypeScript checking in the isolated Linux image with Rust 1.89 and Node 22.19. Local formatting, Clippy, TypeScript checking, and official MCP SDK interoperability checks also passed. Actual herdr/Pi/Claude interaction, real-model behavior, native message-delivery races, and production GitHub responses still warrant live, explicitly authorized smoke tests; isolated coverage is not a claim of production validation.
