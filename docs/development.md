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
```

Adapter tests require Node 22.19+ (Pi's engine requirement and native TypeScript stripping). The macOS/Linux PTY smoke test additionally uses `uv`, creating its Python environment in a temporary project with no third-party dependencies. It runs only the test-owned hub; it never attaches to user terminals.

To verify the minimum Rust compiler, Node engine requirement, and Linux PTY lifecycle in an isolated Linux environment:

```sh
docker build --progress=plain -f tests/linux.Dockerfile -t mustr-validation .
```

This runs Rust tests, TypeScript checking, and the full Node integration suite with Rust 1.89, Node 22.19, Python, and `uv`. No host sockets, credentials, Pi configuration, or hub state are mounted. Docker's build cache may reuse successful test layers; use `--no-cache` to force a complete rerun.

`npm test` builds the debug Rust binary, then exercises the real socket service against an in-process fake herdr server, temporary SQLite/configuration directories, fake participant clients, and a mocked Pi extension host. Fixtures cover cross-space rejection, task exchange, rollback, uncertain restart recovery, singleton locking, pane movement, TODO delegation waits, human input/abort, delayed terminal reports, preserved policy tools, transport framing, and terminal restoration. Rust tests also cover PR surfaces/fingerprints, stale generations, baselines, and watcher errors. The subprocess integration test supplies fake `git`/`gh` executables and checks the exact read-only commands and feedback delivery; no test contacts GitHub.

`pi/test/pi-receipts.test.mjs` calls the pinned Pi dependency's real `AgentSession.sendCustomMessage`/flush/event methods with an in-memory `SessionManager` and no resource discovery or model. It verifies that context-only insertion persists a `custom_message` but does not emit extension `message_end`; an attempt marker and abandoned branches are not receipts. The adapter harness mirrors that distinction. Integration regressions cover information-before-assignment, long busy insertion, input/offer races, marker-only attempts, and late insertion after restart. Push tests stretch the fallback poll to ten seconds and verify delivery before it can fire, concurrent wakeup serialization, opt-in/recipient scoping, and preservation of a partial request frame across socket notifications. Rust tests cover typed delivery transitions, one outstanding offer, safe deferral, disabled/superseded prompts, and explicit recovery of paused work. TUI tests cover recovery hints, cancellation semantics, truncated previews, bounded scrolling, and startup/manual space selection; the macOS/Linux PTY smoke test checks live space selection despite a stale inherited workspace variable.

Tests do not call paid models, install an adapter into live Pi, operate neighboring agents, or mutate Git repositories. Running `cargo install --path . --locked` and `mustr install pi` is a separate, explicit user action. No legacy extensions are removed by this repository.

Composable-role regressions cover ordered/shared graph expansion, invalid headers and names, cycle/duplicate diagnostics, resource bounds, and non-destructive initialization. `tests/roles.test.mjs` checks targeted CLI resolution and isolated invalid-role listing; socket tests reject invalid initial/reconnect/fresh-session graphs before admission, including retaining an authentic reset receipt for a valid retry. `pi/test/adapter-roles.test.mjs` exercises inactive pending selections, late admission rejection, explicit retry, and reconnect prompt refresh with the production CLI/socket service. Tests bind binary/config overrides at adapter factory creation so fixture setup cannot accidentally select an earlier imported environment.

Fresh-start regressions span three boundaries: Rust reset/actor/store tests check scoped persistence, fences and failure recovery; `pi/test/adapter-reset.test.mjs` exercises actual adapter handlers against controlled race/transport fixtures; `pi/test/adapter-runtime.test.mjs` uses the production adapter with real pinned Pi command/session replacement APIs and an isolated Unix-socket peer, proving fresh context and receipt-gated role activation without a model. `tests/reset-ui.test.mjs` plus its uv-owned PTY helper exercise actual human confirmation/cancellation and scoped hub-only clearing. These do not claim a live installed multi-agent reset or termination of external jobs.

## Applying dashboard and adapter changes

Source changes do not update a running hub or Pi extension. Fresh start requires both the new hub and adapter; its database migration to version 2 intentionally prevents old binaries from ignoring durable reset fences. Back up the state directory before upgrade if rollback is needed, and never restore a backup over a running hub. See [Start fresh](start-fresh.md) for scope and recovery. After reconciling active tasks, rebuild/install the binary (`cargo install --path . --locked`), update the adapter with `mustr install pi`, then restart the hub and use `/reload` in participating Pi sessions. Reload ends participation; explicitly `/role NAME` again. Hub restart disables automation and marks ambiguous deliveries/work for reconciliation; do not blindly reassign them. Do not interrupt active agents just to refresh presentation. The receipt/push fix requires the new hub's `delivery_policy: 1` and `inbox_notifications: 1` capabilities and the updated adapter; rebuilding source alone changes neither running component. Already-uncertain deliveries stay uncertain, and paused tasks still require explicit requester guidance after reconciliation.

The installer writes immutable `transport.<hash>.mjs` modules and rewrites the installed entry point's import URL, so native ESM caching cannot retain a different transport after `/reload`. Old transport files remain untouched; changed entry points receive content-addressed `.bak` backups. A corrupt file at an immutable transport URL fails installation instead of being overwritten. No Pi settings or policies are changed. See the [validation session checklist](validation.md) before updating participating sessions.

## Source map

- `cli`, `config`, `paths`, `roles`: configuration, validation, and editable roles.
- `protocol`, `hub`: typed envelopes and deterministic state transitions.
- `delivery`: Rust insertion policy, consuming pending/offered guards, and receipt-only task/PR effects; external observations still require runtime validation.
- `store`, `service`: SQLite and transactional single-writer actor.
- `herdr`, `server`: topology, private sockets, leases, and watcher supervision.
- `watchers`: bounded TODO parsing and read-only GitHub observations.
- `tui`: terminal-native dashboard; no model invocation. See [dashboard design](dashboard-design.md) for visual direction and review criteria.
- `install`: bundled, explicit Pi adapter installation with backups.
- `pi/index.ts`, `pi/transport.mjs`: host lifecycle integration and addressed transport.

The declared minimum Rust version is 1.89 (standard-library file locking). An earlier baseline passed Rust and Node/PTY tests on Linux ARM64 with that compiler; the current composition changes were verified on macOS with the installed stable toolchain, not revalidated on Linux/Rust 1.89. Actual herdr/Pi interaction, real-model behavior, native Pi message-delivery races, and production GitHub responses still warrant live, explicitly authorized smoke tests; isolated coverage is not a claim of production validation.
