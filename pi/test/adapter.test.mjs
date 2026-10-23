import test from "node:test";
import assert from "node:assert/strict";
import { chmod, readFile, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fixture, binary, eventually, delay, stop } from "../../tests/support.mjs";
import { HubClient, HubRemoteError } from "../transport.mjs";

import { harness } from "./adapter-harness.mjs";

test("fixture drains Pi adapter discovery before removing its config directory", { timeout: 10000 }, async t => {
  const f = await fixture(t);
  const config = `${f.directory}/config`;
  const started = join(config, "context-started");
  const finished = join(config, "context-finished");
  const previousEnv = new Map(["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR", "MUSTR_BIN"]
    .map(key => [key, process.env[key]]));
  const identity = {
    protocol: 1, pane_id: "w1:p1", space_id: "w1", server_key: "fixture",
    socket: join(f.directory, "unused.sock"),
  };
  const roles = [{ name: "implementor", description: "fixture", prompt: "fixture role" }];
  const fakeBinary = join(f.directory, "fake-mustr.mjs");
  await writeFile(fakeBinary, `#!/usr/bin/env node
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
const args = process.argv.slice(2);
const config = ${JSON.stringify(config)};
if (args.includes("roles")) {
  console.log(${JSON.stringify(JSON.stringify(roles))});
} else if (args.includes("context")) {
  await writeFile(join(config, "context-started"), "started");
  await delay(150);
  await mkdir(config, { recursive: true });
  await writeFile(join(config, "context-finished"), "finished");
  console.log(${JSON.stringify(JSON.stringify(identity))});
} else {
  process.exitCode = 2;
}
`);
  await chmod(fakeBinary, 0o755);
  Object.assign(process.env, f.env, { HERDR_PANE_ID: "w1:p1", MUSTR_BIN: fakeBinary });
  const { default: extension } = await import(`../index.ts?fixture-cleanup=${Date.now()}`);
  const h = harness(f.directory);
  let sawContextStart = false;
  let shutdownComplete = false;
  f.beforeCleanup(async () => {
    try {
      await h.emit("session_shutdown");
      if (sawContextStart) assert.equal(await readFile(finished, "utf8"), "finished");
      shutdownComplete = true;
    } finally {
      for (const [key, value] of previousEnv) value === undefined ? delete process.env[key] : process.env[key] = value;
    }
  });
  await extension(h.api);
  await h.emit("session_start");
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(async () => {
    try { await readFile(started, "utf8"); sawContextStart = true; return true; }
    catch { return false; }
  });
  t.after(async () => {
    assert.equal(shutdownComplete, true, "adapter shutdown is awaited by fixture cleanup");
    await assert.rejects(stat(f.directory), { code: "ENOENT" }, "fixture directory is removed after adapter teardown");
  });
});

test("adapter: explicit join before hub, human gating, settled report, policies and topology changes", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?fixture=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
  assert.equal(h.commands.has("architect"), false, "role shortcuts are not registered");
  assert.equal(h.commands.has("implementor"), false);
  assert.equal(h.commands.has("reviewer"), false);
  assert.equal((await h.emit("before_agent_start", { systemPrompt: "Base policy" }))[0], undefined);
  await h.commands.get("role").handler("implementor", h.context);
  await delay(150); assert.equal(h.messages.length, 0, "waiting for hub causes no model turns");
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
  assert.equal((await h.emit("before_agent_start", { systemPrompt: "Still unjoined" }))[0], undefined);
  await f.start();
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const promptResult = await h.prompt("Base policy: protect private files.");
  const prompt = promptResult.text;
  assert.equal(promptResult.options.forceSystemPrompt, undefined, "role instructions remain a structured prompt section");
  assert.equal(promptResult.options.sections.preamble, "Base policy: protect private files.");
  assert.match(promptResult.options.sections.mustr_role, /Role: implementor/);
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
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0].block, true, "a provisional terminal report fences productive calls in its native run");
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle();
  await eventually(async () => (await peer.request({ op: "sync", activity: "idle", receive: false })).tasks[0].state === "review");
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "settlement releases only the same-run fence for later host work");
  h.entries.push({ type: "message", message: { role: "assistant", content: [{ type: "toolCall", name: "mustr_send" }, { type: "toolCall", name: "read" }] } });
  assert.match((await h.emit("tool_call", { toolName: "mustr_send" }))[0].reason, /alone/);
  await h.settle(true); // Pi owns abort; it does not create a Mustr pause.
  await peer.request({ op: "send", to: "pi-session", text: "Delivery after host abort", wake: true });
  await eventually(() => h.messages.length === 3);
  assert.doesNotMatch(h.statuses.at(-1), /interrupted/);
  await h.settle();
  // Healthy pumps use the pinned identity; the authenticated hub request still
  // catches pane movement, and fresh local discovery must reject rebinding.
  f.panes.set("w1:p1", { pane_id: "w1:p1", workspace_id: "w2", agent: "pi" });
  await assert.rejects(h.tools.get("mustr_roster").execute("moved-pane", {}, undefined), error => {
    if (error instanceof HubRemoteError) return error.code === "identity_unavailable" || /pane moved, disappeared/.test(error.message);
    if (!(error instanceof Error)) return false;
    return error.message.startsWith("mustr hold: identity check unavailable")
      || error.message === "mustr identity became unverified before this operation could continue"
      || error.message.startsWith("Hub disconnected; delivery may be uncertain.")
      || error.message === "Hub is disconnected";
  }, "route movement must reject the request through the authoritative typed identity, local hold, or connection-closure path");
  await eventually(() => h.notifications.some(n => /space\/server changed/.test(n.message)), 8000);
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
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

test("mustr_roster identifies self among same-role participants", { timeout: 15000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?roster-self=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor local-implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("ready", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "same-role-peer", name: "other-implementor", role: "implementor", pane_id: "w1:p2", space_id: "w1" });
  await peer.request({ op: "sync", activity: "idle", receive: false });

  const result = await h.tools.get("mustr_roster").execute("self-check", {}, undefined);
  assert.deepEqual(result.details.self, { id: "pi-session", name: "local-implementor", role: "implementor" });
  const sameRole = result.details.members.filter(member => member.role === "implementor");
  assert.equal(sameRole.length, 2);
  assert.ok(sameRole.some(member => member.id === "pi-session"));
  assert.ok(sameRole.some(member => member.id === "same-role-peer"));
  assert.equal(JSON.stringify(result.details).includes("reset_control"), false);
});

test("default participant names use the full Pi identity and preserve explicit names", { timeout: 30000 }, async t => {
  const f = await fixture(t); await f.start();
  const longRole = "custom-role-with-a-very-long-name-for-testing";
  await writeFile(`${f.directory}/config/roles/${longRole}.md`, "Long custom role fixture.\n");
  const envKeys = ["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR", "MUSTR_BIN"];
  const previousEnv = new Map(envKeys.map(key => [key, process.env[key]]));
  for (const key of envKeys) process.env[key] = f.env[key] ?? (key === "MUSTR_BIN" ? binary : "");
  process.env.MUSTR_BIN = binary;
  process.env.HERDR_PANE_ID = "w1:p1";
  const realSetTimeout = globalThis.setTimeout;
  globalThis.setTimeout = (callback, ms, ...args) => realSetTimeout(callback, ms === 1000 ? 10000 : ms, ...args);
  const { default: extension } = await import(`../index.ts?default-names=${Date.now()}`);
  const firstId = "12345678-1111-4111-8111-111111111111";
  const secondId = "12345678-2222-4222-8222-222222222222";
  const first = harness(f.directory, { sessionId: firstId });
  let second;
  f.beforeCleanup(async () => {
    try { if (second) await second.emit("session_shutdown"); }
    finally {
      try { await first.emit("session_shutdown"); }
      finally {
        for (const [key, value] of previousEnv) value === undefined ? delete process.env[key] : process.env[key] = value;
        globalThis.setTimeout = realSetTimeout;
      }
    }
  });
  await extension(first.api); await first.emit("session_start");
  const currentRoster = async h => h.tools.get("mustr_roster").execute("default-name-check", {}, undefined);
  const select = async (h, role) => {
    await h.commands.get("role").handler(role, h.context);
    return eventually(() => currentRoster(h));
  };

  const initial = await select(first, "researcher");
  const firstDefaultName = initial.details.self.name;
  assert.equal(initial.details.self.id, firstId);
  assert.match(firstDefaultName, /^[a-z][a-z0-9-]{0,47}$/);
  assert.ok(firstDefaultName.length <= 48);

  const repeated = await select(first, "researcher");
  assert.equal(repeated.details.self.name, firstDefaultName, "the same full identity deterministically gets the same name");
  const explicit = await select(first, "researcher explicitly-named");
  assert.equal(explicit.details.self.name, "explicitly-named", "a supplied participant name is unchanged");

  const longRoleRoster = await select(first, longRole);
  assert.equal(longRoleRoster.details.self.role, longRole);
  assert.match(longRoleRoster.details.self.name, /^[a-z][a-z0-9-]{0,47}$/);
  assert.ok(longRoleRoster.details.self.name.length <= 48);
  assert.ok(longRoleRoster.details.self.name.startsWith(`${longRole.slice(0, 12).replace(/-+$/, "")}-`));

  const readyForPeer = await select(first, "researcher");
  assert.equal(readyForPeer.details.self.name, firstDefaultName);
  process.env.HERDR_PANE_ID = "w1:p2";
  second = harness(f.directory, { sessionId: secondId });
  await extension(second.api); await second.emit("session_start");
  const secondRoster = await select(second, "researcher");
  const peers = secondRoster.details.members.filter(member => member.role === "researcher");
  assert.equal(peers.length, 2, "the real hub admits both default-named same-role participants");
  const byId = new Map(peers.map(member => [member.id, member]));
  assert.equal(byId.get(firstId)?.name, firstDefaultName);
  assert.equal(byId.get(secondId)?.pane_id, "w1:p2");
  assert.notEqual(byId.get(firstId)?.name, byId.get(secondId)?.name, "full identities sharing eight leading characters stay distinct");
  for (const peer of peers) {
    assert.match(peer.name, /^[a-z][a-z0-9-]{0,47}$/);
    assert.ok(peer.name.length <= 48);
  }
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
  f.beforeCleanup(() => h.emit("session_shutdown"));
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

for (const gate of ["editor", "pending", "busy", "dialog"]) test(`offer/input race safely defers (${gate})`, { timeout: 15000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?race=${gate}-${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
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
      if (gate === "pending") h.pending(true);
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
  h.editor(""); await h.emit("ui_prompt_end");
  if (gate === "pending") h.pending(false);
  await h.settle();
  await eventually(() => h.messages.length === 1);
  await eventually(async () => (await poll()).tasks.find(t => t.id === assigned.task_id).state === "active");
  assert.equal(h.notifications.filter(n => n.level === "warning").length, 0);
});

test("delivery waits for Pi-visible pending messages, not input-hook generations", { timeout: 18000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?native-pending=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });

  h.idle(false); h.pending(true);
  await h.emit("input", { source: "interactive", text: "Native pending submission" });
  const sent = await peer.request({ op: "send", to: "pi-session", text: "Wait for host-visible pending work", wake: true });
  await delay(1200);
  assert.equal(h.messages.length, 0, "Pi's pending-message state, rather than an inferred input generation, defers delivery");

  h.pending(false); h.idle(true); await h.emit("turn_end");
  await eventually(() => h.messages.some(item => item.message.details?.id === sent.message_id));
  assert.equal(h.messages[0].options.deliverAs, "followUp");
  assert.equal(h.messages[0].options.triggerTurn, true);
});

for (const order of ["human-first", "extension-first"]) test(`input-hook order is not an acceptance correlation contract (${order})`, { timeout: 22000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?input-origin=${order}-${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });

  const humanText = `human preflight ${order}`;
  const extensionText = `extension prompt ${order}`;
  h.idle(false); await h.emit("agent_start");
  const input = (source, text) => h.emit("input", { source, text, streamingBehavior: "steer" });
  const start = text => h.emit("message_start", { message: { role: "user", content: [{ type: "text", text }] } });
  if (order === "human-first") { await input("interactive", humanText); await input("extension", extensionText); }
  else { await input("extension", extensionText); await input("interactive", humanText); }
  h.pending(false);

  // No Pi-visible message is queued. The adapter does not correlate these
  // hooks by text or origin; the active run can receive a normal steer.
  await start(order === "human-first" ? extensionText : humanText);
  await peer.request({ op: "send", to: "pi-session", text: "Input hook source is not a Mustr fence", wake: true });
  await eventually(() => h.messages.length === 1);
  assert.equal(h.messages[0].options.deliverAs, "steer");
  assert.equal(h.messages[0].options.triggerTurn, true);
  await start(order === "human-first" ? humanText : extensionText);
  assert.equal(h.messages.length, 1, "later starts do not create or consume adapter input generations");
});

test("same-text cross-origin input hooks do not invent a hidden Mustr hold", { timeout: 18000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?ambiguous-input=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });

  h.idle(false); await h.emit("agent_start");
  await h.emit("input", { source: "interactive", text: "same content", streamingBehavior: "steer" });
  await h.emit("input", { source: "extension", text: "same content", streamingBehavior: "steer" });
  h.pending(false);
  await h.emit("message_start", { message: { role: "user", content: [{ type: "text", text: "same content" }] } });
  await peer.request({ op: "send", to: "pi-session", text: "No input-origin correlation", wake: true });
  await eventually(() => h.messages.length === 1);
  assert.equal(h.messages[0].options.deliverAs, "steer");
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "ordinary tools remain host-owned");
});

test("terminal reports settle task scope without globally blocking later Pi tools", { timeout: 15000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?terminal-scope=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });
  const task = await peer.request({ op: "assign", to: "pi-session", title: "Terminal scope", text: "No external work" });
  await h.tools.get("mustr_roster").execute("refresh", {}, undefined);

  const report = await h.tools.get("mustr_report").execute("terminal", { task_id: task.task_id, revision: task.revision, kind: "done", text: "Task is complete." }, undefined);
  assert.equal(report.terminate, true);
  await h.emit("input", { source: "extension", text: "Synthetic start" });
  await h.emit("message_start", { message: { role: "user", content: [{ type: "text", text: "Synthetic start" }] } });
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0].block, true, "same-run productive tools stay fenced until report settlement");
  await assert.rejects(h.tools.get("mustr_report").execute("staged-progress", { task_id: task.task_id, revision: task.revision, kind: "progress", text: "Must not replace the staged terminal claim." }, undefined), /terminal report.*staged/);
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(async () => (await peer.request({ op: "sync", activity: "idle", receive: false })).tasks.find(item => item.id === task.task_id)?.state === "review");
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "later host tools are not gated by the published task fence");

  await h.emit("input", { source: "interactive", text: "Human input does not clear the task stop" });
  await h.emit("message_start", { message: { role: "user", content: [{ type: "text", text: "Human input does not clear the task stop" }] } });
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined);
  await assert.rejects(h.tools.get("mustr_report").execute("old-task", { task_id: task.task_id, revision: task.revision, kind: "progress", text: "Do not reopen a terminal report." }, undefined), /terminal report/);
});

test("handled preflight input alone creates no persistent delivery fence", { timeout: 15000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?handled-preflight=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("roster", {}, undefined));
  const peer = new HubClient(); t.after(() => peer.close());
  await peer.connect(f.discovery.socket, { agent_id: "peer", name: "peer", role: "architect", pane_id: "w1:p2", space_id: "w1" });

  // No user message_start follows. The adapter cannot infer whether Pi handled,
  // rejected, or later transformed this hook; it relies on native host state.
  await h.emit("input", { source: "interactive", text: "handled elsewhere" });
  const sent = await peer.request({ op: "send", to: "pi-session", text: "No inferred input lifecycle", wake: false });
  await eventually(() => h.messages.some(item => item.message.details?.id === sent.message_id));
  assert.equal(h.messages[0].options.deliverAs, "followUp");
  assert.equal(h.messages[0].options.triggerTurn, false);
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined);
});

test("attempt marker without a message is never acknowledged or reinjected", { timeout: 15000 }, async t => {
  const f = await fixture(t); await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?marker=${Date.now()}`);
  const h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
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
  f.beforeCleanup(() => h.emit("session_shutdown"));
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
  f.beforeCleanup(() => h.emit("session_shutdown"));
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
  f.beforeCleanup(() => h.emit("session_shutdown"));
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
  for (const gate of ["editor", "pending", "dialog"]) {
    if (gate === "editor") h.editor("typing");
    if (gate === "pending") h.pending(true);
    if (gate === "dialog") await h.emit("ui_prompt_start");
    const count = h.messages.length;
    await peer.request({ op: "send", to: "pi-session", text: `Respect ${gate}`, wake: false });
    await delay(1200); assert.equal(h.messages.length, count);
    if (gate === "editor") h.editor("");
    if (gate === "pending") h.pending(false);
    if (gate === "dialog") await h.emit("ui_prompt_end");
    await eventually(() => h.messages.length === count + 1);
  }
});
