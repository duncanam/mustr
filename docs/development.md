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

Adapter tests require Node 22.18+ (native TypeScript stripping). The macOS PTY smoke test additionally uses `uv`, creating its Python environment in a temporary project with no third-party dependencies. It runs only the test-owned hub; it never attaches to user terminals. Other platforms still exercise Ratatui's `TestBackend` but currently skip this PTY check.

`npm test` builds the debug Rust binary, then exercises the real socket service against an in-process fake herdr server, temporary SQLite/configuration directories, fake participant clients, and a mocked Pi extension host. Fixtures cover cross-space rejection, task exchange, rollback, uncertain restart recovery, singleton locking, pane movement, TODO delegation waits, human input/abort, delayed terminal reports, preserved policy tools, transport framing, and terminal restoration. Rust tests also cover PR surfaces/fingerprints, stale generations, baselines, and watcher errors. The subprocess integration test supplies fake `git`/`gh` executables and checks the exact read-only commands and feedback delivery; no test contacts GitHub.

Tests do not call paid models, install an adapter into live Pi, operate neighboring agents, or mutate Git repositories. Running `cargo install --path . --locked` and `mustr install pi` is a separate, explicit user action. No legacy extensions are removed by this repository.

## Source map

- `cli`, `config`, `paths`, `roles`: configuration, validation, and editable roles.
- `protocol`, `hub`: typed envelopes and deterministic state transitions.
- `store`, `service`: SQLite and transactional single-writer actor.
- `herdr`, `server`: topology, private sockets, leases, and watcher supervision.
- `watchers`: bounded TODO parsing and read-only GitHub observations.
- `tui`: terminal-native dashboard; no model invocation.
- `install`: bundled, explicit Pi adapter installation with backups.
- `pi/index.ts`, `pi/transport.mjs`: host lifecycle integration and addressed transport.

The declared minimum Rust version is 1.89 (standard-library file locking); current validation uses the installed stable toolchain. Test on that minimum compiler before publishing a release that promises MSRV support. Real-model behavior, native Pi message-delivery races, production GitHub responses, and Linux terminal lifecycle still warrant live, explicitly authorized smoke tests; mocked-host coverage is not a claim of production validation.
