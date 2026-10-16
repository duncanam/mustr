# Configuration and roles

## Paths and CLI

Run `mustr init` to create defaults, or let the first ordinary command initialize them. `--help` and `--version` do not need herdr or configuration.

| Purpose | Default | Override |
|---|---|---|
| Configuration | `~/.config/mustr` | `--config-dir`, `MUSTR_CONFIG_DIR`, then `XDG_CONFIG_HOME/mustr` |
| Persistent state and logs | `~/.local/state/mustr` | `MUSTR_STATE_DIR`, then `XDG_STATE_HOME/mustr` |
| Local transport | `/tmp/mustr-<uid>/<server-key>.sock` | Derived from herdr server identity |
| Pi adapter | `~/.pi/agent/extensions/mustr` | `mustr install pi --agent-dir /absolute/agent-directory` or `PI_CODING_AGENT_DIR` |

Configuration, state, and Pi agent overrides must be absolute paths. The adapter remembers the installed binary and configuration directory; `MUSTR_BIN` and `MUSTR_CONFIG_DIR` override those. Re-run `mustr install pi` after moving the executable.

```sh
mustr                      # hub and dashboard
mustr run --headless        # same hub, no dashboard
mustr roles                # available role names and descriptions
mustr roles --json         # resolved role prompts for the adapter
mustr -v                   # debug logging; -vv for trace, -q for warnings
```

Logs go to `mustr.log` in the state directory, not the dashboard. A log larger than 5 MiB rotates to `mustr.log.1` at startup. Logs are not a complete transcript or a security audit trail.

## config.toml

```toml
version = 1

[scheduling]
checkin_seconds = 1200
continuation_seconds = 30
max_nudges = 3

[ui]
color = true
```

Unknown keys and unsupported versions fail with an error. Check-in intervals are 5–86400 seconds, continuation grace is 1–3600 seconds, and nudge limits are 1–20. Restart the hub after editing settings. PR polling is fixed at ten minutes in protocol v1, with an additional freshness check before delivering queued feedback.

The foreground and background always follow the terminal. Accents use its ANSI palette. Set `color = false` or `NO_COLOR=1` for monochrome; emphasis and reverse-video selection remain.

Automation enablement is deliberately **not** an automatic-start configuration setting. TODO continuation, check-ins, and PR watching are independent, per-space switches, all off when a hub starts or recovers.

## Editable roles

Role files are `roles/<name>.md`. A name is 1–48 lowercase ASCII letters, digits, or hyphens, starting with a letter. Plain Markdown is sufficient; optional TOML front matter supplies a description and one parent:

```markdown
+++
description = "Review performance and benchmark evidence"
extends = "reviewer"
+++

Check algorithmic complexity and realistic benchmark coverage.
Request changes rather than editing the implementation yourself.
```

Save this as `roles/performance-reviewer.md`, then use:

```text
/role performance-reviewer
```

There are no role-name shortcut commands. `/role` lists roles; `/role NAME [PARTICIPANT-NAME]` selects one. Use a short unique participant name to distinguish multiple instances of the same role:

```text
/role implementor api-worker
/role implementor ui-worker
```

Those commands belong in **separate Pi sessions**, not one session. Names identify participants in the dashboard; tools address their stable Pi session IDs.

The initial architect, implementor, reviewer, and researcher prompts are examples, not privileged positions. The hub does not prescribe a hierarchy. Any role can request work; the requester owns guidance and acceptance for that task. Reviewer and researcher initially extend implementor, so edit their inheritance before removing that parent.

Inheritance appends the child prompt to its parent's resolved prompt. Missing parents, cycles, unknown header keys, empty prompts, and oversized files are errors. Existing files are never overwritten, and a deleted role is not recreated while `config.toml` exists. `/role` reloads definitions; an already-selected role retains its prompt until reselected. An unfinished owner assignment prevents changing roles.

Role prompts do not remove existing system prompts, skills, source-control restrictions, protected-path rules, or user direction. A role is not a permission boundary.

## Adapter installation and migration

The installer takes an explicit harness: `mustr install <harness>`. Pi is currently the only supported target; other harnesses will require their own adapters and target-specific options.

`mustr install pi` installs only `index.ts` and `transport.mjs`. It does not edit `settings.json`, remove other extensions, or join any session. Updating an existing mustr-managed adapter preserves changed files in content-addressed `.bak` files. Unmanaged entry points are not overwritten.

Disable the old pair, TODO, and PR-watch schedulers before selecting a mustr role: `/pair-disable`, `/todo-disable`, `/pr-watch off`, where installed. Remove their extension registrations if you no longer need them. mustr cannot safely infer or take over another extension's private scheduler state, and does not silently deactivate unrelated extensions. Running both scheduling systems is unsupported.

Restart Pi or `/reload` after installation. New/reloaded sessions are unjoined. `mustr install pi` must be invoked explicitly; building or testing this repository never installs into a live Pi directory.
