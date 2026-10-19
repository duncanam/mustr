# Configuration and roles

## Paths and CLI

Run `mustr init` to create configuration and restore any missing shipped role files, without overwriting existing files. The first ordinary command also initializes a fresh configuration, but later ordinary commands respect deleted roles. `--help` and `--version` do not need herdr or configuration.

| Purpose | Default | Override |
|---|---|---|
| Configuration | `~/.config/mustr` | `--config-dir`, `MUSTR_CONFIG_DIR`, then `XDG_CONFIG_HOME/mustr` |
| Persistent state and logs | `~/.local/state/mustr` | `MUSTR_STATE_DIR`, then `XDG_STATE_HOME/mustr` |
| Local transport | `/tmp/mustr-<uid>/<server-key>.sock` | Derived from herdr server identity |
| Pi adapter | `~/.pi/agent/extensions/mustr` | `mustr install pi --agent-dir /absolute/agent-directory` or `PI_CODING_AGENT_DIR` |
| Claude MCP bundle | `<mustr-config>/claude` | `mustr install claude --directory /absolute/output-directory` |

Configuration, state, and adapter-directory overrides must be absolute paths. The adapter remembers the installed binary and configuration directory; `MUSTR_BIN` and `MUSTR_CONFIG_DIR` override those. Re-run the appropriate `mustr install pi` or `mustr install claude` command after moving the executable.

```sh
mustr                      # hub and dashboard
mustr run --headless        # same hub, no dashboard
mustr roles                # available role names and descriptions
mustr roles --json         # valid resolved role prompts for the adapter
mustr roles reviewer --json # resolve one requested graph; report its errors
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

Role files are `roles/<name>.md`. A name is 1–48 lowercase ASCII letters, digits, or hyphens, starting with a letter. Plain Markdown is sufficient; optional TOML front matter supplies a description and an ordered list of component roles:

```markdown
+++
description = "Review performance and benchmark evidence"
compose = ["reviewer"]
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

Those commands belong in **separate Pi sessions**, not one session. Claude Code uses the `mustr_join` MCP tool instead; see [Claude Code](claude-code.md). Names identify participants in the dashboard; tools address IDs from the current roster (Pi session IDs or Claude adapter-process IDs).

The shipped architect, implementor, reviewer, and researcher roles all compose `base`, the shared coordination/safety instructions. `base` is normally a component rather than a standalone role, but selecting it is allowed. All roles can be components; there are no privileged positions or fixed hierarchy. Any role can request work; the requester owns guidance and acceptance for that task.

For example, a custom role can combine implementation and review capabilities:

```markdown
+++
description = "Implement changes and review other participants' work"
compose = ["implementor", "reviewer"]
+++

Focus on database changes. Obtain independent review of your own implementation.
```

Composition expands depth-first in listed order, adding each component's dependencies before its own body, then appending the selected role's body. Each role appears once per resolution: this example expands `base`, `implementor`, `reviewer`, then the custom body. Shared dependencies (diamonds) are allowed. A direct list such as `["base", "base"]` is an error, as are self/indirect cycles. Tracking is local to one resolution, so separate agents can share components normally. This is additive instruction composition, not an override mechanism; write responsibilities that can coexist.

Missing or invalid component names, malformed/unknown header fields, and empty prompts are errors. Limits are 32 nested first-visit expansions (including the root), 128 distinct roles per resolution, 64 KiB per source file, and 128 KiB for the expanded prompt. Reused components do not expand again. Diagnostics identify the dependency path. Listing roles returns usable roles with diagnostics for invalid ones; explicitly resolving a role reports its graph's error without letting unrelated broken files block it. The hub validates the requested graph before every initial, reconnect, or fresh-session join, before registering the participant.

Existing files are never overwritten. Ordinary commands do not recreate deleted roles while `config.toml` exists; explicitly run `mustr init` to restore missing shipped files, including `base`, from the installed binary. This is missing-file repair, not migration: existing role contents remain untouched. **`extends` is no longer supported**, even alongside `compose`. For an existing custom role, manually replace `extends = "reviewer"` with `compose = ["reviewer"]`, and reconcile responsibilities with the current shipped defaults; avoid preserving old implementation-only instructions in research/review roles. To replace an old default wholesale, back it up outside `roles/`, remove only that file, and run `mustr init`. Do not delete `config.toml` or reset hub history to repair roles. If generated defaults still contain `extends`, rebuild/install the current binary and check which executable `command -v mustr` selects.

`/role NAME` reloads that graph; reconnect and fresh-session restore also resolve it again. An active prompt is a snapshot: reselect to refresh it after edits, including edits made while a join was in flight. An unfinished owner assignment prevents changing roles. Selection before hub startup stays pending, without role prompts or mustr tools until admission succeeds. Disconnect deactivates them; invalid composition or authoritative join rejection stops automatic retries and shows the cause. Fix the diagnostic and explicitly `/role NAME` again. Fresh-start failures also require the documented [manual recovery](start-fresh.md).

Role prompts do not remove existing system prompts, skills, source-control restrictions, protected-path rules, or user direction. A role is not a permission boundary.

## Adapter installation and migration

The installer takes an explicit harness: `mustr install pi` or `mustr install claude`. Each has separate target-specific options. Claude installation generates an adapter bundle and registers its MCP server at user scope through the Claude CLI; normal `claude` launches then discover it automatically. Unrelated MCP entries and permission/approval controls are preserved. Use `mustr install claude --config-only` to generate only the standalone `mcp.json` and assets, for optional loading with `claude --mcp-config PATH`. See [Claude Code setup and lifecycle limits](claude-code.md).

`mustr install pi` installs only `index.ts` and content-addressed `transport.<hash>.mjs` modules. It does not edit `settings.json`, remove other extensions, or join any session. Updating an existing mustr-managed adapter preserves changed files in content-addressed `.bak` files. Unmanaged entry points are not overwritten.

Disable the old pair, TODO, and PR-watch schedulers before selecting a mustr role: `/pair-disable`, `/todo-disable`, `/pr-watch off`, where installed. Remove their extension registrations if you no longer need them. mustr cannot safely infer or take over another extension's private scheduler state, and does not silently deactivate unrelated extensions. Running both scheduling systems is unsupported.

Restart Pi or `/reload` after installation. New/reloaded sessions are unjoined. `mustr install pi` must be invoked explicitly; building or testing this repository never installs into a live Pi directory.
