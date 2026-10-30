# Configuration and roles

## Paths and CLI

Run `mustr init` to create configuration and restore missing shipped role files and `profiles.toml`; it never overwrites existing files. The first ordinary command also initializes a fresh configuration, but later ordinary commands respect deleted roles/profiles. Normal installer commands preserve roles and seed `profiles.toml` only when missing; the explicit `--overwrite-roles` installer option is documented below. `--help` and `--version` do not need herdr or configuration.

| Purpose | Default | Override |
|---|---|---|
| Configuration | `~/.config/mustr` | `--config-dir`, `MUSTR_CONFIG_DIR`, then `XDG_CONFIG_HOME/mustr` |
| Persistent state and logs | `~/.local/state/mustr` | `MUSTR_STATE_DIR`, then `XDG_STATE_HOME/mustr` |
| Local transport | `/tmp/mustr-<uid>/<instance-key>.sock` | Derived from herdr server identity and space ID |
| Pi adapter | `~/.pi/agent/extensions/mustr` | `mustr install pi --agent-dir /absolute/agent-directory` or `PI_CODING_AGENT_DIR` |

Configuration, state, and adapter-directory overrides must be absolute paths. The adapter remembers the installed binary and configuration directory; `MUSTR_BIN` and `MUSTR_CONFIG_DIR` override those. Re-run `mustr install pi` after moving the executable.

```sh
mustr                      # hub and dashboard for this herdr space
mustr run --headless        # same per-space hub, no dashboard
mustr roles                # available role names and descriptions
mustr roles --json         # valid resolved role prompts for the adapter
mustr roles reviewer --json # resolve one requested graph; report its errors
mustr -v                   # debug logging; -vv for trace, -q for warnings
```

Each instance stays bound to its launching pane's live herdr space. Start another `mustr` in another space to coordinate it; a second instance in the same space is rejected. Configuration and role files are shared, but sockets, locks, saved tasks/messages, automation, and logs are isolated.

An instance stores `hub.sqlite` and `mustr.log` under `<state>/<server-key>/<instance-key>/`. `server-key` identifies the live herdr server incarnation; `instance-key` hashes that identity with the exact space ID and also names the socket and lock. `mustr context` reports the caller's live space, server key, and socket path.

Logs go to that instance's `mustr.log`, not the dashboard. A log larger than 5 MiB rotates to `mustr.log.1` at startup, after acquiring the instance lock. Logs are not a complete transcript or a security audit trail.

Restart persistence remains enabled. The per-space layout starts with fresh history; old shared multi-space databases are left untouched and are not imported. Reconcile work and any incomplete reset before stopping an old hub, then launch one new instance per intended space.

## Pi profiles

Profiles are user-maintained `profiles.toml` entries under the configuration directory; using them remains optional. Fresh setup, `mustr init`, and `mustr install pi` install the bundled [`default` crew](../profiles/default.toml) **only when `profiles.toml` is absent**:

| Slot / tab | Role | Exact Pi model | Thinking |
|---|---|---|---|
| `arch` | `architect` (manager) | `openai-codex/gpt-6.1-sol` | `high` |
| `impl` | `implementor` | `openai-codex/gpt-6-luna` | `high` |
| `review` | `reviewer` | `openai-codex/gpt-6-astra` | `high` |
| `research` | `researcher` | `openai-codex/gpt-6-luna` | `high` |

```sh
mustr --profile default          # launch from the project you want the crew to use
mustr --profile default --jailed # opt into the jailed-agent setup below
mustr agent start impl --profile default --name extra
```

The shipped profile pins no cwd and leaves jail opt-in. Models use Pi's `openai-codex` provider, not an external Codex harness; configure its authentication separately. Edit exact provider/model IDs in your file if you use another provider. Bare `mustr` still starts only the hub/dashboard, not this crew. Existing roles remain unchanged, including their manager metadata.

Existing `profiles.toml` files are never parsed, merged, or replaced by init/install, including `--overwrite-roles`. To add this crew to an existing file, deliberately copy the `profiles.default` agent tables from the bundled file, retaining one top-level `version = 1`. There is no automatic profile migration or overwrite flag. Deleting the entire file and running init/install restores the shipped defaults, not any previous custom profiles; keep your own backup.

Every slot names an exact `provider/model-id`, explicit Pi thinking level, role, and tab label. Profile loading validates syntax and resolves roles before tab creation; the Pi startup handshake then refuses any model/thinking selection that differs from the request. Model IDs retain everything after the first `/`, including nested paths and version suffixes such as `:0`. `cwd` defaults to the command's current directory; relative configured paths resolve from the configuration directory. `cwd` and `jailed` can be set at profile level and overridden per slot. Only one manager role may appear in a profile, and the hub admits only one connected manager per space.

```toml
version = 1

[profiles.feature]
cwd = "/absolute/path/to/checkout"

[profiles.feature.agents.architect]
tab_name = "Architect"
role = "architect"
model = "anthropic/claude-sonnet-4-5"
thinking = "high"

[profiles.feature.agents.implementor]
tab_name = "Implementor"
role = "implementor"
model = "anthropic/claude-sonnet-4-5"
thinking = "medium"

[profiles.investigate.agents.architect]
tab_name = "Investigation lead"
role = "architect"
model = "anthropic/claude-sonnet-4-5"
thinking = "high"

[profiles.investigate.agents.researcher]
tab_name = "Researcher"
role = "researcher"
model = "anthropic/claude-sonnet-4-5"
thinking = "high"
```

Choose exact models available in your Pi configuration; the example retains Anthropic/Claude model support, not the removed Claude Code harness. There is no provider-credential preflight.

`mustr --profile feature` starts the per-space host hub/dashboard when needed and ensures the configured, Mustr-owned Pi tabs. If that space already has a hub, it ensures missing slots and leaves the existing dashboard unchanged. Bare `mustr` is unchanged. `mustr agent start SLOT --profile feature [--name extra]` requires a running hub and starts one configured slot or a distinct named instance; `mustr agent list [--profile feature]` lists only recorded owned tabs. Repeating ensure/default start does not duplicate a managed slot; repeating an extra name is refused. After an explicitly closed tab, start creates a new native identity, without replaying old tasks.

`mustr agent stop PROFILE/SLOT[/NAME]` requires an interactive explicit confirmation. It verifies the exact recorded root pane is still the only pane in the owned tab and original server incarnation/space, both before and after confirmation. Split/replaced/moved panes are refused. Closing a tab does not cancel assignments or external jobs. Mustr never adopts existing tabs by label or working directory. Live profile drift and uncertain create/bootstrap outcomes fail visibly rather than automatically retrying or respawning. Ownership records reside under `<state>/profile-launches/<server-key>/<space-digest>/`; do not delete them to retry an uncertain launch.

Install the current adapter first with `mustr install pi`; profile startup verifies the embedded adapter handshake before creating tabs. Before an unjailed profile's first join, startup waits up to four seconds for Herdr to recognize Pi in the pinned launch pane; pane/space changes or another recognized agent are refused. Ordinary manual joins and jailed startup are unchanged. Only an explicit startup descriptor auto-selects the profile role. Ordinary Pi and reload/resume/new/fork remain opt-in via `/role`. Native launch/close are host CLI operations, not model-facing execution RPCs. Profiles contain no secrets or arbitrary shell commands.

### Jailed profiles

Use `mustr --profile feature --jailed`, or set `jailed = true` on the profile/slot. This requires **ai-jail 2.12.0+** on the host PATH and the current installed Mustr adapter. Only Pi enters the jail; the launcher, hub/dashboard, live Herdr validation and tab controls remain on the host. Unsafe working directories overlapping host Pi/Mustr state/configuration/control sockets, or containing the whole home, are refused before tab creation. Jail-setting changes count as live profile drift.

Each launch gets one ephemeral IPv4 localhost endpoint, one random credential in a private readonly descriptor, and one exact `--forward-port`. Before opening the existing hub socket, the host gateway authenticates and checks the launch's pane, space, role and cwd. It admits one active connection, bounds handshake/frame resources, and forwards only existing coordination operations. The hub still verifies live Herdr identity and manager authority. No generic host-execution endpoint exists. The gateway closes with the launched Pi; connections can reconnect but mutations are never replayed. Failed/uncertain startup never falls back to unjailed execution or automatically relaunches.

Pi gets isolated writable configuration/session storage under the launch's `sandbox/pi/`, snapshots of named settings/context files, and readonly mounts of installed `extensions`, `skills`, `prompts`, `themes`, `npm` and `git` resource directories when present. Host `auth.json`, old sessions, hub DB/ledger/reset evidence, Herdr socket/environment and broad home/IPC/environment access are not passed. Symlink targets or resources outside those directories are **not** automatically exposed; required external policy resources need explicit approved jail configuration before use. New per-launch Pi history is retained, not automatically imported into later launches. The gateway writes host-only readiness after admitted join/subscription.

By default these launches are offline and logged out. Mustr does not inspect credentials or add provider access. Configure only the required provider hosts and credentials in trusted ai-jail configuration (for example its `allow_hosts` and `env_pass`/`env_from_file` settings), or authenticate inside the isolated Pi state after explicitly allowing the provider. Avoid broad maps, whole-environment inheritance or host IPC. Trusted ai-jail configuration can add grants; Mustr is not an auditor of that configuration. Settings/model/MCP files may themselves contain credentials or commands: review those existing configurations before exposing them. Never put secrets in profiles. Unavailable required resources/auth/network are blockers, not permission to broaden access automatically.

Jailed agents can explicitly rejoin only their launch role; its prompt is a launch snapshot. Host file/PR watcher configuration, mode switches and the host fresh-session reset bridge are not forwarded: use the host dashboard for automation; reconcile and explicitly replace a jailed agent instead of resetting it through the bridge. Task `reuse`/`fresh` context selection and ordinary messaging/reporting remain supported. Reload/resume/new/fork do not automatically rejoin or replay work.

Verified on macOS with ai-jail 2.12.0 and the actual pinned Pi CLI in offline RPC mode, synthetic Herdr and no provider calls. Tests check production gateway authentication/scope, host readiness, forbidden host files/sockets/unforwarded port, readonly descriptor and shutdown. This is not a live Herdr/TUI deployment or provider-authentication test. **Linux jail behavior remains unvalidated**; ordinary Unix-socket/unjailed behavior is unchanged.

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

Unknown keys and unsupported versions fail with an error. Manager check-in intervals are 5–86400 seconds, continuation grace is 1–3600 seconds, and nudge limits are 1–20. `max_nudges` bounds TODO/task continuation, not the periodic manager watchdog. Restart the hub after editing settings. PR polling is fixed at ten minutes in protocol v1, with an additional freshness check before delivering queued feedback.

The foreground and background always follow the terminal. Accents use its ANSI palette. Set `color = false` or `NO_COLOR=1` for monochrome; emphasis and reverse-video selection remain.

TODO continuation, manager check-ins, and PR watching are independent per-space switches. Check-ins default **on** for a fresh space and wait for a manager to join; the saved per-space choice is preserved across restart, including an explicit off setting. Existing saved off settings are not silently changed by an upgrade. TODO and PR watching remain off on startup/recovery. A human-confirmed reset leaves automation off.

PR watching requires an existing open PR for the selected checkout's branch; it does not create one. Failures identify the command stage (branch discovery, PR/checks/reviews, inline-review API, or HEAD recheck), exit status, and a safe diagnostic category when recognized. Raw stderr is bounded and never displayed or persisted because it can contain credentials/private paths. An open PR can still encounter authentication, permissions, rate-limit, or network failures in the **hub's launch environment**; success from a different shell does not prove that environment works. Check the indicated read-only operation in the selected checkout using the hub's environment. Only if discovery actually reports no PR should you select a PR checkout or attach `/mustr-pr URL` from a matching branch. No failure triggers fallback to another PR or advances its feedback baseline.

PR watching does not require a URL in configuration. Dashboard `p` starts background discovery of the branch's PR from the selected participant's checkout; `/mustr pr on` uses the calling Pi's checkout when there is no attached watch. Enabling starts discovery, not immediate feedback delivery: the watcher first validates and pins the PR. `gh` must be installed and authenticated in the hub's environment. Selection and discovery errors are visible rather than falling back to another checkout or participant. `/mustr-pr URL` remains an explicit override; see [PR watching](architecture.md#pr-watching).

## Editable roles

Role files are `roles/<name>.md`. A name is 1–48 lowercase ASCII letters, digits, or hyphens, starting with a letter. Plain Markdown is sufficient; optional TOML front matter supplies a description, an ordered list of component roles, and a boolean `manager` flag (default `false`):

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

Those commands belong in **separate Pi sessions**, not one session. Names identify participants in the dashboard; tools address IDs from the current roster.

The shipped architect, implementor, reviewer, and researcher roles all compose `base`, the shared coordination/safety instructions. `base` is normally a component rather than a standalone role, but selecting it is allowed. Any participant can ask the manager for work using `mustr_send` with `wake: true`; only the server-admitted manager can create new assignments and must explicitly choose `reuse` or `fresh`. Existing task requesters retain guidance and acceptance authority, including legacy nonmanager requesters. The four shipped functional roles have distinct work boundaries: architect coordinates and decides from evidence, researcher investigates and writes Markdown findings, implementor makes scoped changes, and reviewer independently verifies and writes Markdown findings.

### One manager per space

Set `manager = true` in the selected role's front matter to make that participant the recipient of periodic check-ins. The shipped architect has this flag; role names themselves confer no priority. The flag is root-only, like the description: composing an architect or another manager role does **not** inherit manager status. A custom coordinating role must declare it explicitly. `mustr roles NAME --json` reports the resolved flag.

The hub validates the flag from its own role files, not a client claim, and admits at most one connected manager per space. A paused manager still holds that slot; a disconnected manager does not. A new, explicitly joined manager role may fill a vacant slot; there is no election, fallback to a worker, or transfer of existing task ownership. Manager status does not permit Guide/Accept or general task control. An admitted manager may cancel another requester's queued/current tasks only for explicit user-directed redirection; bulk clears require an explicit scope and the current per-owner guard shown by `mustr_roster`. Cancellation is bookkeeping only and does not stop external work.

Default-on check-ins wait for an admitted manager. Dashboard `c` or `/mustr checkin off` opts out; enable again with `/mustr checkin on`. An explicit enable with no manager produces an error rather than selecting someone else. While enabled, the watchdog waits for an idle, unpaused manager and coalesces reminders. It also runs when there are no tasks: the hub cannot infer completion of a goal held in the manager's conversation. Turn it off explicitly when monitoring is no longer wanted; that choice survives restart. Reset leaves it off.

Existing role files are preserved by default. After updating the hub, either add `manager = true` to the intended existing role's front matter or deliberately refresh shipped files with [`--overwrite-roles`](#refreshing-shipped-role-prompts); reselect the role after reconciling active work. Do not delete configuration or history to acquire manager status.

### Composing roles

A domain-specific role can extend one functional role without mixing functional permissions. For example, a security reviewer can add focus while retaining the review-only contract:

```markdown
+++
description = "Review parser changes for security issues"
compose = ["reviewer"]
+++

Inspect trust boundaries, validation, and failure behavior. Report evidence and required corrections; do not edit the implementation.
```

Composition expands depth-first in listed order, adding each component's dependencies before its own body, then appending the selected role's body. Each role appears once per resolution: this example expands `base`, `reviewer`, then the custom body. Shared dependencies (diamonds) are allowed. A direct list such as `["base", "base"]` is an error, as are self/indirect cycles. Tracking is local to one resolution, so separate agents can share components normally. The resolver concatenates prompt text; it does not detect or enforce role conflicts. Composition is not a permission union: do not combine conflicting functional missions such as implementor+reviewer. If functional instructions conflict, stop and ask the human to select one functional role directly; compatible domain and safety components may still compose.

Missing or invalid component names, malformed/unknown header fields, and empty prompts are errors. Limits are 32 nested first-visit expansions (including the root), 128 distinct roles per resolution, 64 KiB per source file, and 128 KiB for the expanded prompt. Reused components do not expand again. Diagnostics identify the dependency path. Listing roles returns usable roles with diagnostics for invalid ones; explicitly resolving a role reports its graph's error without letting unrelated broken files block it. The hub validates the requested graph before every initial, reconnect, or fresh-session join, before registering the participant.

Ordinary commands and default installer runs do not overwrite existing files. Ordinary commands do not recreate deleted roles while `config.toml` exists; explicitly run `mustr init` to restore missing shipped files, including `base`, from the installed binary. This is missing-file repair, not migration. **`extends` is no longer supported**, even alongside `compose`. For an existing custom role, manually replace `extends = "reviewer"` with `compose = ["reviewer"]`, and reconcile responsibilities with the current shipped defaults; avoid preserving old implementation-only instructions in research/review roles. To replace an old default wholesale, use the explicit installer refresh below, or back it up outside `roles/`, remove only that file, and run `mustr init`. Do not delete `config.toml` or reset hub history to repair roles. If generated defaults still contain `extends`, rebuild/install the current binary and check which executable `command -v mustr` selects.

`/role NAME` reloads that graph; reconnect and fresh-session restore also resolve it again. An active prompt is a snapshot: reselect to refresh it after edits, including edits made while a join was in flight. An unfinished owner assignment prevents changing roles. Selection before hub startup stays pending, without role prompts or mustr tools until admission succeeds. Disconnect deactivates them; invalid composition or authoritative join rejection stops automatic retries and shows the cause. Fix the diagnostic and explicitly `/role NAME` again. Fresh-start failures also require the documented [manual recovery](start-fresh.md).

These role contracts are model instructions, not a filesystem sandbox or runtime role-routing enforcement. They do not remove existing system prompts, skills, source-control restrictions, protected-path rules, or user direction. Existing role files are preserved by default and must be reviewed or updated intentionally; `--overwrite-roles` is an explicit shipped-file refresh, not runtime enforcement. Selecting a role again refreshes the prompt snapshot. Do not treat composition as permission to cross a role boundary.

### Refreshing shipped role prompts

Normal installs preserve role files. To opt into replacing them with the embedded defaults for this build, use the explicit flag:

```sh
mustr install pi --overwrite-roles
```

This refresh visits only the embedded shipped filenames in the selected config directory (`architect.md`, `base.md`, `implementor.md`, `reviewer.md`, and `researcher.md`). It creates missing shipped files, does nothing to identical files, and replaces changed contents—including user customizations under those shipped names. It leaves custom-named roles, `config.toml`, state/history, adapter settings, and unrelated extensions untouched. `mustr init` remains missing-file-only and does not accept this override.

Before changing a shipped file, mustr saves its exact prior bytes in the sibling `role-backups/` directory, outside role discovery. Backups use deterministic names of the form `<role>.md.<24-hex-content-digest>.bak` and are never replaced by different contents. If a backup path conflicts or a destination is a symlink/nonregular file, the refresh fails closed. Repeated identical refreshes do not rewrite role files or accumulate backups. Restore a selected version by copying that exact backup back to its matching `roles/<name>.md`; inspect the filename and active prompt before doing so rather than using a wildcard. Installer and role refresh are not one transaction: if a later per-file update fails, earlier replacements and their backups remain, and the error reports completed paths; no automatic rollback is claimed.

Role files are shared by all spaces using the selected config directory. `--overwrite-roles` changes files on disk only: it does not restart/reload Pi, select a role, join/resume an agent, or alter hub mode/state. Existing prompts are snapshots. Reconcile active work first, then explicitly reselect/rejoin roles when you choose to use the refreshed prompt.

## Adapter installation and migration

`mustr install pi` installs `index.ts` plus content-addressed transport and context-window helper modules, and creates the bundled default `profiles.toml` if missing. It does not edit existing profiles or `settings.json`, remove other extensions, or join any session. Updating an existing mustr-managed adapter preserves changed files in content-addressed `.bak` files. Unmanaged entry points are not overwritten.

Disable the old pair, TODO, and PR-watch schedulers before selecting a mustr role: `/pair-disable`, `/todo-disable`, `/pr-watch off`, where installed. Remove their extension registrations if you no longer need them. mustr cannot safely infer or take over another extension's private scheduler state, and does not silently deactivate unrelated extensions. Running both scheduling systems is unsupported.

Restart Pi or `/reload` after installation. New/reloaded sessions are unjoined. `mustr install pi` must be invoked explicitly; building or testing this repository never installs into a live Pi directory.

Automatic PR discovery needs the updated hub and checkout-advertising adapter. After reconciling active work, update/restart the hub before reloading and explicitly rejoining updated adapters: older hubs reject the new `cwd` join field. Existing per-space version-3 records migrate losslessly to version 4; missing checkout context defaults to unavailable until a current adapter rejoins. This does not import old shared multi-space history or re-enable automation.
