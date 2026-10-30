<p align="center"><img src="assets/mustr-logo.jpg" alt="mustr" width="640"></p>

A terminal-native coordination hub for Pi agents in herdr. Run one mustr instance in a tab of each space you want to coordinate; agents join with editable roles and exchange addressed messages. The hub handles task ownership and user-controlled scheduling—not model reasoning.

For a self-contained agent reference, run **`mustr --ai-help`**. It includes all public command options, setup/configuration, profiles, Pi coordination, ai-jail grants and troubleshooting for the installed version. It works outside Herdr without reading configuration or initializing state.

## Install

Requires Rust 1.89+, Node 22.19+, herdr's socket API, and Pi 1.2+. macOS and Linux; `gh` is needed only for PR watching.

```sh
cargo install --path . --locked
mustr install pi
```

Restart Pi or `/reload` for existing sessions. Installation also creates `~/.config/mustr/profiles.toml` when missing; existing profiles are never overwritten or merged.

## Use

From your project inside herdr, launch the installed crew with:

```sh
mustr --profile default
# Optional, after configuring ai-jail/provider access:
mustr --profile default --jailed
```

The default crew is `arch` (GPT-6.1 Sol), `impl` (GPT-6 Luna), `review` (GPT-6 Astra), and `research` (GPT-6 Luna), all at **high** thinking through Pi's `openai-codex` provider. New agents automatically join their configured roles. Configure provider authentication separately; see [profiles and jail setup](docs/configuration.md#pi-profiles). Use `mustr agent start/list/stop` to manage owned instances. No default project path is pinned.

For manual participation instead, run `mustr` in a herdr tab. That instance stays bound to the tab's live space at launch; it cannot switch spaces. For another space, open a tab there and run `mustr` again. Only one instance can run per space, and closing one leaves the others running.

Open separate Pi sessions in other tabs of the same space, then select **one role in each session**:

| Pi session | Command |
|---|---|
| Architect's tab | `/role architect` |
| Implementor's tab | `/role implementor` |
| Optional reviewer's tab | `/role reviewer` |

Roles can be selected before the hub starts. Agents keep their own normal conversations. Closing mustr disconnects coordination; it does **not** kill agents or external jobs.

| Dashboard key | Action |
|---|---|
| `t` | Toggle TODO/task continuation |
| `c` | Toggle periodic manager check-ins |
| `p` | Toggle PR watching; discover the branch's PR on first enable |
| `f` | Switch Messages / Tasks tab |
| `r` | Start fresh in this space / inspect reset status (confirmation required) |
| `↑` / `↓`, `k` / `j` | Scroll messages or browse tasks in the active tab |
| `?`, `q` | Help, quit |

Messages and Tasks share a pane with independent scroll positions. The pane shows queued task counts and FIFO positions; a queued task has not been dispatched or started. The pane border also shows total tasks and accepted completions for this space; awaiting-review and cancelled tasks are not counted as completed. Cancelling a task changes hub bookkeeping only and does not stop external jobs.

Manager check-ins default **on** for a fresh space; an explicitly saved off setting stays off. TODO and PR watching start **off**. Press `p` to start discovering the current branch's PR from a participant's checkout; if several participants are eligible, choose who should receive feedback. Watching begins after discovery and validation succeed. From Pi, `/mustr pr on` discovers for that session when no watch is attached. Discovery uses `gh` without a model and pins the resulting PR URL; errors appear in the dashboard. `/mustr-pr https://github.com/OWNER/REPO/pull/123` remains an explicit override, initially off.

For unattended coordination, join one role with `manager = true`; default-on check-ins wait for that manager to become available. `c` or `/mustr checkin off` opts out; `/mustr checkin on` re-enables it. The shipped architect is a manager; existing role files are preserved by default. Only one manager can be joined per space. Check-ins periodically ask it to reconcile the current goal, outstanding reviews, blockers and waiting work—not merely collect worker status. They remain enabled until explicitly turned off. Manager status grants no general task ownership or Guide/Accept authority; only explicit user-directed redirection permits a manager to cancel another requester's queued/current work. Bulk cancellation requires an explicit scope and current per-owner roster guard. Cancellation changes hub bookkeeping only and never stops external work.

Every new assignment requires an explicit `reuse` or `fresh` context choice; the hub checks that the recipient currently supports it before creating work. For Pi, `fresh` starts a task-scoped model-context window at the inserted Assignment without resetting the Pi session or deleting its history; `reuse` keeps the existing window. See [behavior and protocol](docs/architecture.md#assignment-context).

Attach a checklist with `/mustr-todo path/to/TODO.md`, then enable TODO mode. `/mustr pause` is an explicit hold on Mustr delivery released only by `/mustr resume` or leaving; human and extension turns do not resume coordination. Ordinary Pi tools remain available during the pause. Pi owns input, abort, and provider retry state: visible queued messages/drafts gate delivery, while mustr does not infer input acceptance or retain interruption/final-error holds. A native retry delays autonomous delivery only until Pi settles; mustr does not retry failed work. `/mustr leave` ends participation without stopping external jobs.

## Configure

Jailed launches use **ai-jail's normal Pi state sharing**: by default, Pi's credentials, settings and sessions are shared read-write and changes persist. Mustr does not create isolated Pi state or require a symlink allowlist. If a dotfiles symlink points outside ai-jail's accessible paths, allow the specific target in trusted **ai-jail configuration**; see [Pi state and symlink setup](docs/configuration.md#jailed-profiles).

`~/.config/mustr/config.toml` contains scheduling/UI settings. Editable prompts live in `~/.config/mustr/roles/*.md`; add domain-specific roles or compose compatible components using TOML front matter. Keep to one functional role—architect, researcher, implementor, or reviewer—per prompt; see [configuration and role boundaries](docs/configuration.md#composing-roles). `$XDG_CONFIG_HOME` and `--config-dir` are supported. `mustr init` and normal installs preserve existing roles. To refresh embedded shipped role files deliberately, run `mustr install pi --overwrite-roles`; customized contents under shipped filenames are replaced after exact backups in `<config>/role-backups/`. Custom-named roles and config/state are untouched. Role files are shared across spaces using that config directory; active prompt snapshots do not change until you explicitly reselect/rejoin after reconciling work. See [role refresh and backup/restore details](docs/configuration.md#refreshing-shipped-role-prompts).

`mustr -v` enables debug logs; `-vv` enables trace logs. Each instance has its own log and saved state beneath `~/.local/state/mustr`; see [paths and upgrade guidance](docs/configuration.md#paths-and-cli). The UI uses your terminal's foreground/background and ANSI palette; `NO_COLOR=1` disables color accents.

[Configuration and roles](docs/configuration.md) · [Start fresh](docs/start-fresh.md) · [Behavior and protocol](docs/architecture.md) · [Development](docs/development.md)

Licensed under [GPLv3](LICENSE) (`GPL-3.0-only`).
