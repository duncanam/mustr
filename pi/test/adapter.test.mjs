import test from "node:test";
import assert from "node:assert/strict";
import { fixture, binary, eventually, delay, stop } from "../../tests/support.mjs";
import { HubClient } from "../transport.mjs";

function harness(directory) {
  const events = new Map(), commands = new Map(), tools = new Map();
  const entries = [], messages = [], notifications = [];
  let active = ["read", "bash", "policy-tool"], idle = true, editor = "", pending = false, deferInsertion = false;
  const context = {
    cwd: directory, mode: "tui", isIdle: () => idle, hasPendingMessages: () => pending,
    sessionManager: { getSessionId: () => "pi-session", getBranch: () => entries },
    ui: { notify: (message, level) => notifications.push({ message, level }), setStatus() {}, getEditorText: () => editor },
  };
  async function emit(event, payload = {}) {
    const results = [];
    for (const fn of events.get(event) ?? []) results.push(await fn(payload, context));
    return results;
  }
  async function insert(index) {
    const { message, options } = messages[index];
    // Real Pi persists context-only messages without emitting an extension event.
    entries.push({ type: "custom_message", ...message });
    if (options.triggerTurn) await emit("message_end", { message: { ...message, role: "custom" } });
  }
  const api = {
    getCommands: () => [{ name: "architect" }], getActiveTools: () => active,
    setActiveTools: names => { active = names; },
    on: (event, handler) => { const handlers = events.get(event) ?? []; handlers.push(handler); events.set(event, handlers); },
    registerCommand: (name, value) => commands.set(name, value),
    registerTool: definition => tools.set(definition.name, definition),
    appendEntry: (customType, data) => entries.push({ type: "custom", customType, data }),
    sendMessage: (message, options) => {
      messages.push({ message, options });
      if (options.triggerTurn && idle) { idle = false; void emit("agent_start"); }
      if (!deferInsertion) void insert(messages.length - 1);
    },
  };
  return { api, commands, tools, entries, messages, notifications, context, emit,
    active: () => active, editor: text => { editor = text; }, pending: value => { pending = value; },
    deferInsertion: value => { deferInsertion = value; },
    insert,
    settle: async (aborted = false) => { idle = true; await emit("agent_settled", { aborted }); },
  };
}

test("adapter: explicit join before hub, human gating, settled report, policies and topology changes", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?fixture=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  t.after(() => h.emit("session_shutdown"));
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
  assert.equal(h.commands.has("architect"), false, "role shortcuts are not registered");
  assert.equal(h.commands.has("implementor"), false);
  assert.equal(h.commands.has("reviewer"), false);
  assert.equal((await h.emit("before_agent_start", { systemPrompt: "Base policy" }))[0], undefined);
  await h.commands.get("role").handler("implementor", h.context);
  await delay(150); assert.equal(h.messages.length, 0, "waiting for hub causes no model turns");
  assert.ok(h.active().includes("mustr_send")); assert.ok(h.active().includes("policy-tool"));
  await f.start();
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const prompt = (await h.emit("before_agent_start", { systemPrompt: "Base policy: protect private files." }))[0].systemPrompt;
  assert.ok(prompt.startsWith("Base policy: protect private files.")); assert.match(prompt, /protected-path/);
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });
  await peer.request({ op: "sync", activity: "idle", receive: false });
  h.editor("Human is typing");
  await peer.request({ op: "send", to: "pi-session", text: "Please review this request", wake: true });
  await delay(1200); assert.equal(h.messages.length, 0);
  h.editor(""); h.pending(true); await delay(1200); assert.equal(h.messages.length, 0);
  h.pending(false); await eventually(() => h.messages.length === 1);
  assert.equal(h.entries.filter(e => e.customType === "mustr-received").length, 1);
  assert.equal(h.messages[0].options.deliverAs, "followUp");
  await h.settle();
  await eventually(async () => (await peer.request({ op: "sync", activity: "idle", receive: false })).members.some(m => m.id === "pi-session" && m.activity === "idle"));
  const assignment = await peer.request({ op: "assign", to: "pi-session", title: "Mock task", text: "No model or files need changing." });
  await eventually(() => h.messages.length === 2);
  await eventually(async () => (await peer.request({ op: "sync", activity: "idle", receive: false })).tasks[0].state === "active");
  const report = await h.tools.get("mustr_report").execute("report", { task_id: assignment.task_id, revision: 1, kind: "done", text: "Fixture verified." }, undefined);
  assert.equal(report.terminate, true);
  await delay(1100);
  assert.equal((await peer.request({ op: "sync", activity: "idle", receive: false })).tasks[0].state, "active");
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0].block, true);
  await h.settle();
  await eventually(async () => (await peer.request({ op: "sync", activity: "idle", receive: false })).tasks[0].state === "review");
  h.entries.push({ type: "message", message: { role: "assistant", content: [{ type: "toolCall", name: "mustr_send" }, { type: "toolCall", name: "read" }] } });
  assert.match((await h.emit("tool_call", { toolName: "mustr_send" }))[0].reason, /alone/);
  await h.settle(true); // human abort wins
  await peer.request({ op: "send", to: "pi-session", text: "Do not interrupt the pause", wake: true });
  await delay(1200); assert.equal(h.messages.length, 2);
  await h.commands.get("mustr").handler("resume", h.context);
  await eventually(() => h.messages.length === 3);
  await h.settle();
  f.panes.set("w1:p1", { pane_id: "w1:p1", workspace_id: "w2", agent: "pi" });
  await eventually(() => !h.active().includes("mustr_roster"));
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
  assert.ok(h.notifications.some(n => /space\/server changed/.test(n.message)));
  await peer.request({ op: "cancel", task_id: assignment.task_id, revision: 1, text: "Fixture reconciled; no external jobs." });
  f.panes.set("w1:p1", { pane_id: "w1:p1", workspace_id: "w1", agent: "pi" });
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const paused = await h.tools.get("mustr_pause").execute("pause", { reason: "Need a human decision" }, undefined);
  assert.equal(paused.terminate, true);
  assert.ok((await peer.request({ op: "sync", activity: "idle", receive: false })).members.some(m => m.id === "pi-session" && m.activity === "paused"));
  await h.emit("session_before_switch");
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
  assert.equal((await h.emit("before_agent_start", { systemPrompt: "Normal session" }))[0], undefined);
});

test("socket push delivers without a polling tick and coalesces concurrent wakeups", { timeout: 12000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  // Stretch the adapter's one-second fallback. Fast delivery must come from push.
  const timeout = globalThis.setTimeout;
  globalThis.setTimeout = (fn, ms, ...args) => timeout(fn, ms === 1000 ? 10000 : ms, ...args);
  t.after(() => { globalThis.setTimeout = timeout; });
  const original = HubClient.prototype.request;
  let inSync = 0, maxInSync = 0;
  HubClient.prototype.request = async function(command, ...args) {
    if (command.op !== "sync" || command.receive !== true) return original.call(this, command, ...args);
    inSync++; maxInSync = Math.max(maxInSync, inSync);
    try { await delay(30); return await original.call(this, command, ...args); }
    finally { inSync--; }
  };
  t.after(() => { HubClient.prototype.request = original; });
  const { default: extension } = await import(`../index.ts?push=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  t.after(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  await delay(150); // initial poll has finished; next fallback is ten seconds away
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });
  await peer.request({ op: "send", to: "pi-session", text: "Push information", wake: false });
  await eventually(() => h.messages.length === 1, 1500);
  await peer.request({ op: "send", to: "pi-session", text: "Start work", wake: true });
  await eventually(() => h.messages.length === 2, 1500);
  for (let i = 0; i < 20; i++) await h.emit("turn_end");
  await peer.request({ op: "send", to: "pi-session", text: "Steer while working", wake: true });
  await eventually(() => h.messages.length === 3, 1500);
  assert.equal(h.messages[2].options.deliverAs, "steer");
  assert.equal(maxInSync, 1, "push and lifecycle events never overlap pumps");
  assert.equal(h.entries.filter(e => e.type === "custom_message").length, 3);
});

for (const gate of ["editor", "human", "busy", "dialog"]) test(`offer/input race safely defers (${gate})`, { timeout: 15000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?race=${gate}-${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  t.after(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });
  const poll = () => peer.request({ op: "sync", activity: "idle", receive: false });
  // Interpose only at the async boundary after Rust durably offers the message.
  const original = HubClient.prototype.request;
  let raced = false, deferred = false;
  HubClient.prototype.request = async function(command, ...args) {
    const result = await original.call(this, command, ...args);
    if (command.op === "defer") deferred = true;
    if (command.op === "sync" && result.inbox && !raced) {
      raced = true;
      if (gate === "editor") h.editor("unsent human draft");
      if (gate === "human") await h.emit("input", { source: "interactive" });
      if (gate === "busy") await h.emit("agent_start");
      if (gate === "dialog") await h.emit("ui_prompt_start");
    }
    return result;
  };
  t.after(() => { HubClient.prototype.request = original; });
  const assigned = await peer.request({ op: "assign", to: "pi-session", title: "Input race", text: "No external work" });
  await eventually(() => deferred);
  assert.equal(h.messages.length, 0);
  assert.equal(h.entries.filter(e => e.customType === "mustr-received").length, 0);
  assert.equal((await poll()).tasks.find(t => t.id === assigned.task_id).state, "assigned");
  h.editor(""); await h.emit("ui_prompt_end"); await h.settle();
  await eventually(() => h.messages.length === 1);
  await eventually(async () => (await poll()).tasks.find(t => t.id === assigned.task_id).state === "active");
  assert.equal(h.notifications.filter(n => n.level === "warning").length, 0);
});

test("attempt marker without a message is never acknowledged or reinjected", { timeout: 15000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?marker=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  t.after(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });
  h.editor("hold delivery until the marker is present");
  const sent = await peer.request({ op: "send", to: "pi-session", text: "Previously attempted", wake: true });
  h.entries.push({ type: "custom", customType: "mustr-received", data: { id: sent.message_id, state: "offered" } });
  h.editor(""); await h.emit("turn_end");
  await eventually(() => h.notifications.some(n => /attempt is uncertain/.test(n.message)));
  await delay(2500);
  assert.equal(h.messages.length, 0);
  assert.ok(await h.tools.get("mustr_roster").execute("roster", {}, undefined));
});

test("late insertion after hub restart is not acknowledged on the new connection", { timeout: 18000 }, async t => {
  const f = await fixture(t); const hub = await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?restart=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  t.after(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });
  h.deferInsertion(true);
  await peer.request({ op: "send", to: "pi-session", text: "Deferred", wake: false });
  await eventually(() => h.messages.length === 1);
  await stop(hub);
  await eventually(() => !peer.connected);
  await h.insert(0);
  await f.start();
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  await delay(2200);
  assert.equal(h.messages.length, 1);
  assert.ok(!h.notifications.some(n => /not offered|insertion/.test(n.message)), "late receipt must not poison reconnection");
});

test("informational receipt does not disconnect or strand the next assignment", { timeout: 22000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?receipt=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  t.after(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });
  const poll = () => peer.request({ op: "sync", activity: "idle", receive: false });
  await peer.request({ op: "send", to: "pi-session", text: "Information before an assignment", wake: false });
  await eventually(() => h.messages.length === 1);
  const assigned = await peer.request({ op: "assign", to: "pi-session", title: "Must not strand", text: "No external work" });
  await eventually(() => h.messages.length === 2, 14000);
  await eventually(async () => (await poll()).tasks.find(t => t.id === assigned.task_id)?.state === "active");
  assert.ok(!h.notifications.some(n => /uncertain|insertion|disconnect/.test(n.message)));
});

test("live feedback reaches a busy agent; slow insertion keeps heartbeats and human gates", { timeout: 45000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?live=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  t.after(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });
  const poll = () => peer.request({ op: "sync", activity: "idle", receive: false });
  await peer.request({ op: "send", to: "pi-session", text: "Start fixture", wake: true });
  await eventually(() => h.messages.length === 1);
  await eventually(async () => (await poll()).members.find(m => m.id === "pi-session")?.activity === "working");
  h.deferInsertion(true);
  await peer.request({ op: "send", to: "pi-session", text: "A comment while you work", wake: false });
  await eventually(() => h.messages.length === 2);
  assert.equal(h.messages[1].options.deliverAs, "steer");
  assert.equal(h.messages[1].options.triggerTurn, false, "information never starts a new run");
  const before = (await poll()).members.find(m => m.id === "pi-session").last_seen;
  await delay(11000); // Beyond the old insertion timeout; models/tools may take minutes.
  const after = (await poll()).members.find(m => m.id === "pi-session");
  assert.equal(after.activity, "working"); assert.ok(after.last_seen > before + 5);
  assert.ok(!h.notifications.some(n => /uncertain|insertion/.test(n.message)));
  await peer.request({ op: "send", to: "pi-session", text: "Actionable correction", wake: true });
  await delay(1200); assert.equal(h.messages.length, 2, "one unconfirmed insertion at a time");
  h.deferInsertion(false); await h.insert(1);
  await eventually(() => h.messages.length === 3);
  assert.equal(h.messages[2].options.deliverAs, "steer");
  assert.equal(h.messages[2].options.triggerTurn, true);
  for (const gate of ["editor", "pending", "dialog", "human"]) {
    if (gate === "editor") h.editor("typing");
    if (gate === "pending") h.pending(true);
    if (gate === "dialog") await h.emit("ui_prompt_start");
    if (gate === "human") await h.emit("input", { source: "interactive" });
    const count = h.messages.length;
    await peer.request({ op: "send", to: "pi-session", text: `Respect ${gate}`, wake: false });
    await delay(1200); assert.equal(h.messages.length, count);
    if (gate === "editor") h.editor("");
    if (gate === "pending") h.pending(false);
    if (gate === "dialog") await h.emit("ui_prompt_end");
    if (gate === "human") await h.settle();
    await eventually(() => h.messages.length === count + 1);
  }
});
