import test from "node:test";
import assert from "node:assert/strict";
import { fixture, binary, eventually, delay } from "../../tests/support.mjs";
import { HubClient } from "../transport.mjs";

function harness(directory) {
  const events = new Map(), commands = new Map(), tools = new Map();
  const entries = [], messages = [], notifications = [];
  let active = ["read", "bash", "policy-tool"], idle = true, editor = "", pending = false;
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
  const api = {
    getCommands: () => [{ name: "architect" }], getActiveTools: () => active,
    setActiveTools: names => { active = names; },
    on: (event, handler) => { const handlers = events.get(event) ?? []; handlers.push(handler); events.set(event, handlers); },
    registerCommand: (name, value) => commands.set(name, value),
    registerTool: definition => tools.set(definition.name, definition),
    appendEntry: (customType, data) => entries.push({ type: "custom", customType, data }),
    sendMessage: (message, options) => {
      messages.push({ message, options });
      if (options.triggerTurn) { idle = false; void emit("agent_start"); }
      void emit("message_end", { message: { ...message, role: "custom" } });
    },
  };
  return { api, commands, tools, entries, messages, notifications, context, emit,
    active: () => active, editor: text => { editor = text; }, pending: value => { pending = value; },
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
