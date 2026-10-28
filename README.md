<p align="center"><img src="assets/mustr-logo.jpg" alt="mustr" width="640"></p>

A terminal-native coordination hub for Pi and Claude Code agents in herdr. Run one mustr instance in a tab of each space you want to coordinate; agents join with editable roles and exchange addressed messages. The hub handles task ownership and user-controlled scheduling—not model reasoning.

## Install

Requires Rust 1.89+, Node 22.19+, and herdr's socket API, plus Pi 1.2+ or Claude Code with stdio MCP support. macOS and Linux; `gh` is needed only for PR watching.

```sh
cargo install --path . --locked
mustr install pi
```

Restart Pi or `/reload`.

For Claude Code, run `mustr install claude` once to install and register the user-wide MCP server, then launch `claude` normally in a herdr pane. Ask Claude to join a mustr role and wait for work. Claude uses explicit MCP wait/ack tools; background wake and session reset are not supported. See [Claude Code setup and limits](docs/claude-code.md).

## Use

Run `mustr` in a herdr tab. That instance stays bound to the tab's live space at launch; it cannot switch spaces. For another space, open a tab there and run `mustr` again. Only one instance can run per space, and closing one leaves the others running.

Open separate Pi sessions in other tabs of the same space, then select **one role in each session**:

| Pi session | Command |
|---|---|
| Architect's tab | `/role architect` |
| Implementor's tab | `/role implementor` |
| Optional reviewer's tab | `/role reviewer` |

Roles can be selected before the hub starts. For a new interactive hub, an ordered Pi profile can start several roles in separate tabs: `mustr --profile team` or `mustr run --profile team`. Profiles are opt-in; plain startup and manual `/role` remain unchanged. Profile startup requires the Herdr 0.9.5 CLI, cannot run headless, and will not attach to a hub already running in that space. Its dashboard shows partial outcomes; `q` cancels unfinished startup but leaves launched agents open. See [profile startup](docs/configuration.md#profile-startup) for the roles-only file format, validation, and recovery guidance.

Agents keep their own normal conversations. Closing mustr disconnects coordination; it does **not** kill agents or external jobs.

| Dashboard key | Action |
|---|---|
| `t` | Toggle TODO/task continuation |
| `c` | Toggle periodic manager check-ins |
| `p` | Toggle PR watching; discover the branch's PR on first enable |
| `f` | Switch Messages / Tasks tab |
| `r` | Start fresh in this space / inspect reset status (confirmation required) |
| `↑` / `↓`, `k` / `j` | Scroll messages or browse tasks in the active tab |
| `[` / `]` | Scroll profile startup results when shown |
| `?`, `q` | Help, quit |

Messages and Tasks share a pane with independent scroll positions. The pane border shows total tasks and accepted completions for this space; awaiting-review and cancelled tasks are not counted as completed.

Manager check-ins default **on** for a fresh space; an explicitly saved off setting stays off. TODO and PR watching start **off**. Press `p` to start discovering the current branch's PR from a participant's checkout; if several participants are eligible, choose who should receive feedback. Watching begins after discovery and validation succeed. From Pi, `/mustr pr on` discovers for that session when no watch is attached. Discovery uses `gh` without a model and pins the resulting PR URL; errors appear in the dashboard. `/mustr-pr https://github.com/OWNER/REPO/pull/123` remains an explicit override, initially off.

For unattended coordination, join one role with `manager = true`; default-on check-ins wait for that manager to become available. `c` or `/mustr checkin off` opts out; `/mustr checkin on` re-enables it. The shipped architect is a manager; existing role files are preserved by default. Only one manager can be joined per space. Check-ins periodically ask it to reconcile the current goal, outstanding reviews, blockers and waiting work—not merely collect worker status. They remain enabled until explicitly turned off and do not grant additional task ownership.

Attach a checklist with `/mustr-todo path/to/TODO.md`, then enable TODO mode. `/mustr pause` is an explicit hold released by `/mustr resume`. After an interruption or genuine human-decision hold, an accepted human follow-up also resumes participation. Successful native Pi retries do not leave mustr paused. Final failed retries have a [host cancellation ambiguity](docs/architecture.md#participation-and-human-control), so unattended recovery is not guaranteed for every error. `/mustr leave` ends participation without stopping external jobs.

## Configure

`~/.config/mustr/config.toml` contains scheduling/UI settings. Editable prompts live in `~/.config/mustr/roles/*.md`; add domain-specific roles or compose compatible components using TOML front matter. Keep to one functional role—architect, researcher, implementor, or reviewer—per prompt; see [configuration and role boundaries](docs/configuration.md#composing-roles). `$XDG_CONFIG_HOME` and `--config-dir` are supported. `mustr init` and normal installs preserve existing roles. To refresh embedded shipped role files deliberately, run `mustr install pi --overwrite-roles` or `mustr install claude --overwrite-roles`; customized contents under shipped filenames are replaced after exact backups in `<config>/role-backups/`. Custom-named roles and config/state are untouched. Role files are shared across harnesses and spaces using that config directory; active prompt snapshots do not change until you explicitly reselect/rejoin after reconciling work. See [role refresh and backup/restore details](docs/configuration.md#refreshing-shipped-role-prompts).

`mustr -v` enables debug logs; `-vv` enables trace logs. Each instance has its own log and saved state beneath `~/.local/state/mustr`; see [paths and upgrade guidance](docs/configuration.md#paths-and-cli). The UI uses your terminal's foreground/background and ANSI palette; `NO_COLOR=1` disables color accents.

[Claude Code](docs/claude-code.md) · [Configuration and roles](docs/configuration.md) · [Start fresh](docs/start-fresh.md) · [Behavior and protocol](docs/architecture.md) · [Development](docs/development.md)

Licensed under [GPLv3](LICENSE) (`GPL-3.0-only`).
