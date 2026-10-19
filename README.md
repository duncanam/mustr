<p align="center"><img src="assets/mustr-logo.jpg" alt="mustr" width="640"></p>

A terminal-native coordination hub for Pi and Claude Code agents in herdr. Run one mustr instance in a tab of each space you want to coordinate; agents join with editable roles and exchange addressed messages. The hub handles task ownership and opt-in scheduling—not model reasoning.

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

Roles can be selected before the hub starts. Agents keep their own normal conversations. Closing mustr disconnects coordination; it does **not** kill agents or external jobs.

| Dashboard key | Action |
|---|---|
| `t` | Toggle TODO/task continuation |
| `c` | Toggle periodic check-ins |
| `p` | Toggle PR watching |
| `f` | Switch Messages / Tasks tab |
| `r` | Start fresh in this space / inspect reset status (confirmation required) |
| `↑` / `↓`, `k` / `j` | Scroll messages or browse tasks in the active tab |
| `?`, `q` | Help, quit |

Messages and Tasks share a pane with independent scroll positions. The pane border shows total tasks and accepted completions for this space; awaiting-review and cancelled tasks are not counted as completed.

Modes start **off**. From the intended Pi recipient, attach a checklist with `/mustr-todo path/to/TODO.md` or a PR with `/mustr-pr https://github.com/OWNER/REPO/pull/123`, then enable its mode. `/mustr pause`, `/mustr resume`, and `/mustr leave` control participation. Escape pauses automated delivery.

## Configure

`~/.config/mustr/config.toml` contains scheduling/UI settings. Editable prompts live in `~/.config/mustr/roles/*.md`; add your own or compose reusable roles using TOML front matter. `$XDG_CONFIG_HOME` and `--config-dir` are supported. Existing role files are never overwritten.

`mustr -v` enables debug logs; `-vv` enables trace logs. Each instance has its own log and saved state beneath `~/.local/state/mustr`; see [paths and upgrade guidance](docs/configuration.md#paths-and-cli). The UI uses your terminal's foreground/background and ANSI palette; `NO_COLOR=1` disables color accents.

[Claude Code](docs/claude-code.md) · [Configuration and roles](docs/configuration.md) · [Start fresh](docs/start-fresh.md) · [Behavior and protocol](docs/architecture.md) · [Development](docs/development.md)

Licensed under [GPLv3](LICENSE) (`GPL-3.0-only`).
