# Claude Code

Claude Code can participate alongside Pi through a local stdio MCP adapter. This is an **explicit-wait integration**, not Pi lifecycle parity: it does not inject messages into an idle Claude conversation, stop Claude's ordinary tools, or reset its session.

## Install and start

Requires Node 22.19+, a Claude Code release supporting local stdio MCP servers, and herdr identifying the pane's agent as `claude`. Rust is needed to build mustr. No npm dependencies are needed for this adapter.

```sh
cargo install --path . --locked
mustr install claude
```

The installer creates its adapter bundle and registers `mustr` with Claude Code at **user scope**, so it is available across projects without a launch flag. The `claude` command must be available on `PATH`. In a separate herdr pane, launch normally:

```sh
claude
```

Approve any prompts Claude presents; registration does not weaken tool permissions or managed-policy checks. Start `mustr` in another tab of the same herdr space. For another space, launch a separate mustr instance there; the adapter discovers only its own space's hub. Restart Claude after installing or updating the integration, after reconciling any active work. Starting Claude or its MCP server does not automatically join mustr.

For bundle generation without registration, use:

```sh
mustr install claude --config-only
claude --mcp-config "$HOME/.config/mustr/claude/mcp.json"
```

Use the printed configuration path when using `XDG_CONFIG_HOME`, `--config-dir`, or `mustr install claude --directory /absolute/output-directory`. `--config-only` does not require the Claude CLI. Managed Claude policies may restrict MCP servers or disable `--mcp-config`; ask the administrator for approval rather than bypassing those policies.

Automatic registration preserves unrelated MCP servers and does not change permissions, project `.mcp.json`, or `CLAUDE.md`. Conflicting user-scoped `mustr` entries are refused rather than overwritten. A local/project server with the same name can take precedence; the installer does not rewrite those project-specific choices. The bundle installer also refuses unmanaged configuration files and keeps backups of changed managed files; content-addressed JavaScript assets are never overwritten. Repeat installs reuse an exactly matching registration and atomically update the managed `mustr.mjs` launcher when needed. Keep the same bundle directory for upgrades; moving an existing registration requires explicit reconciliation.

Registration checks bounded, read-only user-profile data (`~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json` for an explicit absolute profile directory), then uses `claude mcp add-json --scope user` only when the entry is absent. It does not merge the native profile itself or use `mcp get`/`list`, which can launch configured servers. The override layout was verified with Claude Code 2.1.302; unsupported/malformed layouts fail closed. Missing/failing Claude CLI registration returns an error while retaining the generated bundle for `--config-only` recovery. Building or testing the repository does not install into a live Claude configuration.

## Join and coordinate

Ask Claude:

> Join mustr as an implementor named claude-worker. Use mustr_wait to wait for an assignment, acknowledge messages with mustr_ack before acting, and preserve all normal project policies.

`mustr_join` takes `role` and an optional `name`. The selected role is resolved from the same editable mustr roles as Pi. It is returned as tool context, not a replacement for Claude's system instructions or permissions. Starting the MCP server alone does not join.

Claude may display MCP tools with a server prefix such as `mcp__mustr__mustr_join`. Other participants address the ID returned by the current roster, never a role name. IDs belong to the adapter process; restarting Claude/MCP does not restore the old participant automatically.

| Tool | Behavior |
|---|---|
| `mustr_join` | Explicitly join with `role` and optional `name` |
| `mustr_roster` | Read the current space's participants/tasks; does not receive messages |
| `mustr_wait` | Advertise availability during a `timeout_seconds` wait window (0–30, default 25) for one addressed message |
| `mustr_ack` | Confirm observation of the exact `message_id` returned by the previous wait |
| `mustr_send`, `mustr_assign` | Send an addressed message or assign ownership |
| `mustr_report`, `mustr_guide`, `mustr_accept`, `mustr_cancel` | Use the same task ownership/revision contract as Pi |
| `mustr_pause` | Pause participation until a user requests leaving and rejoining |
| `mustr_leave` | Disconnect coordination without stopping Claude or external jobs |

Call communication tools alone. A participant is advertised **working** except while a `mustr_wait` call is in flight, when it is **idle** and eligible for assignments. It returns to working before the wait result is emitted. This prevents ordinary Claude work outside MCP from being mistaken for an idle agent. Ask Claude to wait before assigning it new work. A timeout means no message was obtained, not that the task is finished; hub round-trips have separate bounded timeouts and can extend the total call duration. The adapter never starts an unbounded wait loop or invokes a model itself.

Wait results are offers, not receipts. Claude must call `mustr_ack` with the offered ID in a later invocation before acting. Writing a tool result to stdout does not acknowledge it, and another wait does not replay an unacknowledged offer. Losing the connection before acknowledgement leaves reconciliation work, not permission to retry blindly. Acknowledgement records observation, not execution or completion.

## Limits and recovery

- No background wake, streaming steering, channel notifications, hooks, or automatic polling of the inbox. Messages remain queued until Claude explicitly waits. Transport heartbeats do not invoke a model. A Claude participant may select a manager role, but default-on manager check-ins still require explicit waits; the flag does not give Claude Pi-style unattended wake.
- No visibility into editor drafts, human input, ordinary tools, or Claude's settled state. A terminal `done`, `blocked`, or `waiting` report is a **declared status**, published immediately; unlike Pi, the adapter cannot terminate the model run or prove that tools have settled. Stop work when reporting, and independently verify the result before accepting it.
- Pause/resumption follows user direction in the conversation. MCP cannot independently authenticate whether the human requested a tool invocation. Pausing mustr does not pause Claude's ordinary tools.
- No automatic reconnect, mutation retry, or ambiguous-message replay. Inspect the roster, files, and external jobs before explicitly rejoining; an unfinished task paused by disconnect still requires requester guidance or cancellation.
- No automatic session-boundary tracking. Before `/clear`, switching/resuming a conversation, changing roles, or replacing the MCP process, explicitly leave and reconcile pending work. Rejoin in the intended conversation. A long-lived MCP process must not be shared across Claude conversations.
- No TODO/PR attachment tools or automation configuration tools in this adapter. The adapter advertises its working directory, so dashboard `p` can select it for automatic branch-PR discovery. This uses the MCP adapter process's checkout, not an inferred directory from Claude's conversation. Delivery of dashboard-generated prompts still requires an explicit wait. Idle-grace continuation may not become eligible during a bounded wait (the default continuation grace is 30 seconds); do not rely on Pi-style automatic continuation.
- No fresh-start capability. A connected Claude participant prevents the dashboard's all-participant reset preflight from succeeding. Explicitly leave first; a subsequent hub reset does **not** clear the disconnected Claude conversation. Start a fresh Claude conversation manually before rejoining.

The hub validates the live pane, workspace, and supported harness on each operation, and disconnects if the pane moves or changes from Claude to Pi (or vice versa). This remains cooperative same-user routing, not authentication against hostile programs running as your account. Existing user/tool/privacy policies remain in force.

## Integration references

- [Claude Code MCP setup](https://code.claude.com/docs/en/mcp-quickstart) and [CLI reference](https://code.claude.com/docs/en/cli-reference): local stdio servers, user-scoped registration, and optional `--mcp-config` loading.
- [Claude Code channel reference](https://code.claude.com/docs/en/channels-reference): channels are a separate push mechanism; transport writes do not prove model processing, and events can be dropped. mustr deliberately does not use them as delivery receipts or require development-channel bypass flags.
- [Claude Code hooks](https://code.claude.com/docs/en/hooks): lifecycle hooks are not Pi's insertion/settlement API. This adapter does not install hooks or claim their guarantees.
