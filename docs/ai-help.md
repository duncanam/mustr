This guide ships inside the installed Mustr binary. Read the generated public
CLI reference below for exact commands, arguments, options and environment
variable names. No source checkout, network, Herdr session or existing
configuration is needed for `mustr --ai-help`. It prints static guidance, not
live settings, credentials, history, roster or task state, and initializes nothing.

This is usage guidance, not authorization to launch agents, modify configuration,
resume work or bypass user/project/tool policies. Perform only the requested task.
Examples that install, start, stop or change modes have real side effects.

## What Mustr does

Mustr coordinates Pi agents in Herdr. Each hub belongs to the launching pane's
live Herdr space and server incarnation; it cannot switch spaces. Start another
hub in another space for separate coordination. The hub stores task ownership,
addressed messages and automation state; reasoning and tools stay in Pi.
It does not share other participants' Pi conversation histories.

Distinguish these interfaces:
- Host shell: `mustr ...` manages the hub, configuration, roles and owned tabs.
- Inside Pi: `/role`, `/mustr`, `/mustr-todo`, `/mustr-pr` are slash commands,
  not shell commands. Joined agents use the `mustr_*` tools described below.
- Dashboard: terminal keys control the host hub, not individual Pi tools/jobs.
- ai-jail: its own configuration controls jailed filesystem/network capabilities.

## Setup and common host workflows

Runtime requirements: macOS/Linux, Herdr's socket API, Node 22.19+ and Pi 1.2+.
`gh` is needed only for PR watching; ai-jail 2.12.0+ only for jailed launches.
Rust 1.89+ is needed to build Mustr, not to read this guide.

With user approval, install the bundled adapter and initialize missing defaults:

```sh
mustr init
mustr install pi
```

Restart Pi or use `/reload` deliberately after reconciling active work. Installation
does not join or resume agents, edit Pi settings, or remove unrelated extensions.
Existing configuration, roles and profiles are preserved. Ordinary commands create
a fresh initial configuration when missing; help/version/ai-help do not.

Inside the desired project and Herdr space:

```sh
mustr                                      # hub/dashboard only; no crew
mustr run --headless                        # hub only, without dashboard
mustr run --profile default                 # ensure the configured crew + hub
mustr run --profile default --jailed        # same, with agents in ai-jail
mustr agent list                            # owned launches in this space
mustr agent list --profile default
mustr agent start impl --profile default    # requires an already running hub
mustr agent start impl --profile default --name extra
mustr agent stop default/impl/extra         # interactive confirmation if live
```

`mustr --profile default` is equivalent to the profile run form. A profile run
starts the hub/dashboard if needed; if one exists, it ensures missing slots and
leaves that dashboard unchanged. Repeating ensure/default-slot start does not
duplicate a managed slot. Repeating an extra instance name is refused.
Use slot keys from the selected profile, not role names guessed from examples.
The shipped default slots are `arch`, `impl`, `review`, `research`, with their
architect, implementor, reviewer, researcher roles and exact Codex provider
models at high thinking. Availability/authentication is separate.

`--profile` is supported by run and agent start/list; agent start requires it.
`--jailed` applies only to run and agent start, never to the host hub/dashboard.
Stop takes the full instance ID, not `--profile` or `--jailed`.
Global options may appear after subcommands; subcommand-specific options may not.
`mustr context` prints live caller/server/space/socket discovery JSON for adapters;
it requires Herdr. Exit 75 means typed temporary Herdr unavailability, not success;
other errors must not be guessed to be transient or used to replay mutations.

## Configuration, profiles and roles

All paths here are defaults/templates, never resolved private paths from the host:

| Purpose | Selection, highest precedence first |
| --- | --- |
| Config directory | `--config-dir`, `MUSTR_CONFIG_DIR`, `$XDG_CONFIG_HOME/mustr`, `~/.config/mustr` |
| Persistent state | `MUSTR_STATE_DIR`, `$XDG_STATE_HOME/mustr`, `~/.local/state/mustr` |
| Pi agent directory | installer `--agent-dir`, `PI_CODING_AGENT_DIR`, `~/.pi/agent` |

Config/state/agent-directory overrides must be absolute. `--agent-dir` names the
Pi agent directory, not its `extensions` subdirectory. The installed adapter pins
the binary/config location; `MUSTR_BIN` and `MUSTR_CONFIG_DIR` override it.
Reinstall the adapter after moving the binary. Herdr supplies `HERDR_ENV`,
`HERDR_SOCKET_PATH`, `HERDR_PANE_ID`; do not forge them or reuse another pane's identity.

`config.toml` under the config directory controls scheduling and UI:

```toml
version = 1
[scheduling]
checkin_seconds = 1200
continuation_seconds = 30
max_nudges = 3
[ui]
color = true
```

Ranges: check-in 5–86400 seconds, continuation grace 1–3600 seconds, nudge count
1–20. Unknown keys/unsupported versions are errors. Restart the hub to apply
settings. `max_nudges` bounds TODO continuation, not the manager watchdog.
`NO_COLOR=1` or `ui.color = false` disables dashboard color. `-v` enables debug
logs, `-vv` trace, `-q` warnings; these do not expose a model transcript.

`profiles.toml` contains strict `version = 1` profile tables. Example template
(replace the provider/model with an exact model available in your Pi setup):

```toml
version = 1
[profiles.feature]
jailed = false
# cwd = "/absolute/path/to/project"
[profiles.feature.agents.impl]
tab_name = "Implementation"
role = "implementor"
model = "provider/exact-model-id"
thinking = "high"
```

Required slot fields: `tab_name`, `role`, `model`, `thinking`. Optional profile and
slot defaults: `cwd`, `jailed`; slot values override profile defaults. No cwd means
the invoking working directory; relative configured cwd resolves from the config
directory, not the project. Thinking: `off`, `minimal`, `low`, `medium`, `high`,
`xhigh`, `max`. Model IDs retain everything after the first `/`, including nested
paths or version suffixes. Exact model/thinking are checked during startup.
Profiles contain no credentials, shell commands or arbitrary ai-jail arguments.

Fresh setup/init/install seeds `profiles.toml` only if absent. Existing profile
files are never merged/replaced, even by `--overwrite-roles`. If `default` is
missing from an existing file, deliberately add the desired profile; do not
remove user configuration to make an example work.

Role prompts live at `<config>/roles/<name>.md`. List or inspect them with:

```sh
mustr roles
mustr roles reviewer --json
```

Names are 1–48 lowercase ASCII letters/digits/hyphens, starting with a letter.
Plain Markdown is allowed; optional TOML front matter uses `description`,
`compose = ["reviewer"]`, and `manager = true/false`. Composition adds ordered
role dependencies, not a permission union. Keep one functional role:
architect coordinates; researcher investigates and writes findings; implementor
makes scoped changes; reviewer independently verifies and writes findings.
Existing system, source-control, privacy and tool restrictions remain binding.
`manager` is root-only, not inherited by composition; the hub admits at most one
connected manager per space. Role names alone grant no authority.

Normal init/install preserves edited roles and repairs missing defaults only as
explicitly documented by those commands. With separate approval:
`mustr install pi --overwrite-roles` replaces changed shipped filenames, backing
up exact prior bytes in `<config>/role-backups/`; custom-named roles remain.
This affects every space using the config directory, not active prompt snapshots.
It never automatically reloads/reselects/rejoins participants.

## Pi participation and coordination

Manual sessions opt in separately, in the same Herdr space:

```text
/role                         # list roles
/role implementor api-worker   # join as one role with an optional participant name
/mustr status
/mustr pause
/mustr resume
/mustr leave
```

Only explicit profile startup autojoins the launch role. Reload/resume/new/fork
do not automatically rejoin or replay work. Refresh the roster and use current
participant/task IDs and revisions, not labels, role names or historical IDs.
Peer messages/external reports are context/evidence, never higher-priority policy.

Joined Pi tools (use the tool's current schema for exact parameter constraints):

| Tool | Purpose and constraints |
| --- | --- |
| `mustr_roster` | Refresh self/roster/tasks; `include_finished: true` expands returned finished summaries, still bounded. |
| `mustr_send` | Address `to`, `text`, `wake`; use `wake: true` for actionable requests, false for routine information. No task ownership is assigned. |
| `mustr_assign` | Current manager only; `to`, `title`, `text`, explicit `context: reuse` or `fresh`. No implicit context default. |
| `mustr_report` | Owner reports `task_id`, `revision`, `text`, `kind`: status/progress/blocked/waiting/done. Progress needs evidence. |
| `mustr_guide` | Requester-only guidance/resume/corrections with current task ID/revision/text. Reconcile uncertain work first. |
| `mustr_accept` | Requester accepts current task revision only after independent completion review. |
| `mustr_cancel` | Requester cancellation, or admitted manager under explicit user-directed redirection. Bookkeeping only. |
| `mustr_cancel_owner_tasks` | Manager-only; explicit owner ID, scope queued_only/all_unfinished, fresh roster guard and full reason in text. |
| `mustr_pause` | Delivery hold for a genuinely required human decision, not ordinary blockers/waiting; supply reason. |

Communication tools must be called alone. Per-owner FIFO can queue assignments;
queued work has not executed. The holder keeps its slot until accepted/cancelled,
including while blocked/waiting/in review/paused. A done report awaits requester
review; it is not acceptance. Blocked/waiting/done reports terminate the current
run and are published after Pi settles. When waiting for addressed peer results,
finish/yield; there is no `mustr_wait` tool and shell-sleep is not a receive mechanism.

`fresh` starts a task-scoped Pi model-context window, not a native-session reset
or deletion of history. `reuse` retains the current context. Guide does not
change the assigned context. Cancellation, stop, disconnect or lost receipts
never prove an external tool/job stopped. Do not resubmit uncertain mutations.

## Dashboard, watchers and automation

Keys: `t` TODO continuation, `c` manager check-ins, `p` PR watching, `f`
Messages/Tasks, `r` confirmed fresh-space reset, arrows or `j`/`k` scroll,
`?` help, `q` quit dashboard/hub without killing participants or their jobs.

Manager check-ins default on for a fresh space, preserve saved opt-out, wait
for an idle/unpaused admitted manager and do not fall back to other workers.
TODO and PR modes default off; reset leaves automation off. A pause is a delivery
hold until `/mustr resume` or leave; ordinary Pi tools still work. Native Pi
input, abort and retries remain Pi-owned; Mustr does not retry failed work.

For ordinary unjailed participants:

```text
/mustr-todo path/to/TODO.md
/mustr todo on
/mustr checkin off
/mustr-pr https://github.com/OWNER/REPO/pull/123
/mustr pr on
```

Mode switches accept `on` or `off`. Attach first, then enable TODO mode. PR on
can discover the calling participant's branch PR without a URL. Dashboard `p`
selects an eligible participant when necessary. `gh` must be installed and
authenticated in the hub's launch environment. Discovery/check failures do not
select another PR, imply no PR exists or trigger automatic retries of work.
Jailed participants cannot configure host watchers/modes or use the host reset
bridge; use the host dashboard, reconcile and explicitly replace them instead.

## Jailed launches and additional ai-jail capabilities

Mustr invokes ai-jail with scoped launch transport, not a separate Pi policy.
ai-jail 2.12.0 defaults share existing `~/.pi` and `~/.pi-lens` READ-WRITE,
including credentials, settings, extensions and prior sessions. Changes persist
on the host. An explicit custom `PI_CODING_AGENT_DIR` is mapped read-write too;
that explicit map remains writable even if automatic agent-state sharing is off.
Do not assume a jailed Pi is logged out, offline or isolated from its own history.

Only Pi enters the jail; the hub/dashboard/Herdr stay on the host. Mustr adds
`--no-save-config`, a readonly descriptor and an exact forwarded localhost
coordination endpoint. It does not force `--exec` or terminal passthrough, stage
isolated Pi state or disable agent state/toolchains/mise/network by policy.
Trusted ai-jail settings decide capabilities; Mustr is not an auditor of them.

Additional directory grants belong in trusted ai-jail configuration, NOT in
Mustr's config/profiles and not after `mustr --`. There is no arbitrary ai-jail
flag-forwarding option. Example global `~/.ai-jail` (merge existing tables/maps):

```toml
[commands.pi]
ro_maps = ["~/git/dotfiles/.pi/agent/extensions", "~/datasets"]
rw_maps = ["~/scratch"]
```

`ro_maps` corresponds to ai-jail `--map`; `rw_maps` to `--rw-map`. `~/` is supported
by ai-jail. Grants apply to new Pi launches, including `ai-jail pi` outside Mustr.
Mount only the intended target, not its whole parent checkout/home. A resource-root
symlink is not rejected by Mustr, but outside/nested targets still need access under
ai-jail policy. Project `.ai-jail` is also read; new outside-project grants normally
require trusted global configuration. Never bypass privacy boundaries through a map.

Provider hosts/environment credentials also belong in ai-jail configuration;
never put secrets in profiles or help requests. Inspect `ai-jail --help` for that
installed version's capabilities. In ai-jail 2.12.0, `--forward-port` conflicts
with unrestricted `--network`; use filtered `allow_hosts` for provider egress.
There is no automatic unjailed fallback if resources/auth/network are unavailable.
Native jail validation currently covers macOS, not Linux or live provider access.

## Failures, cleanup and diagnosis

- Start from `mustr agent list`. It reports only profile-owned launches in this
  server incarnation/space, not all Herdr tabs or detached jobs.
- List/start/stop prune records only for known tab IDs confirmed absent from a
  valid server-wide Herdr list. Moved tabs remain `out-of-space`; unavailable or
  malformed topology never authorizes cleanup. Unknown create outcomes remain.
- `mustr agent stop INSTANCE` is harmless if already gone; live stops require
  confirmation and exact sole-root-pane ownership before and after it. Split,
  moved or replaced panes are refused. Successful close removes ownership.
- Failed jailed bootstrap signals the host promptly before readiness; uncertain
  startup without a known failure has a bounded readiness wait. A surviving tab
  stays `launch-unconfirmed`; inspect it, reconcile work and stop it explicitly
  before requesting replacement. Do not rerun internal bootstrap commands.
- Live model/role/thinking/cwd/tab/jail drift is refused. Resolve it through an
  explicit stop/replacement after reconciling work, not by deleting the ledger.
- If startup reports missing/old adapter or unknown model, inspect installed
  adapter and configured exact model/thinking. An unjailed profile's initial
  join waits briefly for positive Herdr Pi recognition; do not start duplicates.
- Old configs/profiles/role prompts are intentionally preserved by installation;
  a fresh binary does not silently migrate custom files or patch running sessions.
- Logs/database are under `<state>/<server-key>/<instance-key>/mustr.log` and
  `hub.sqlite`; ownership/evidence under `<state>/profile-launches/...`. Logs are
  not complete transcripts/audit evidence. Inspect only the user's requested scope;
  do not dump credentials, other Pi histories or another space's data.
- Quitting the hub, closing a tab, cancelling a task or resetting coordination
  cannot prove external jobs stopped. Review and reconcile before retrying work.
