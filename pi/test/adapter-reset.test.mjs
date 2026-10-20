import test from "node:test";
import assert from "node:assert/strict";
import { rm, writeFile } from "node:fs/promises";
import { HubClient, HubRemoteError } from "../transport.mjs";
import { eventually, delay } from "../../tests/support.mjs";
import extension from "../index.ts";
import { harness } from "./adapter-harness.mjs";
import { resetFixture } from "./adapter-reset-support.mjs";

const registry = () => globalThis[Symbol.for("mustr.reset.bridge.v1")];

function assertPrivate(f) {
  assert.equal(f.privateSurfaces().includes(f.hub.control.token), false, "no ticket in transcript/UI/command dispatch");
  assert.deepEqual(f.h.root.errors, []);
  assert.deepEqual(f.h.root.staleCalls, [], "old runtime APIs must not be used after replacement");
}

test("actual adapter commands replace factory/session/history only after one claim and validated receipt", async t => {
  const f = await resetFixture(t);
  assert.equal(f.hub.connections[0].hello.cwd, f.f.directory, "ordinary Pi Join advertises the absolute checkout");
  f.h.entries.push({ type: "message", message: { role: "user", content: "OLD SENTINEL CONTEXT" } });
  f.hub.prepare();
  const result = await f.discover();
  assert.equal(f.h.root.attempts, 0, "roster callback never recursively dispatches replacement");
  assert.equal(result.details.reset_control, undefined);
  await eventually(() => f.hub.completed && f.h.root.current.active().includes("mustr_roster"));
  const fresh = f.h.root.current;
  assert.notEqual(fresh, f.h);
  assert.notEqual(fresh.context.sessionManager.getSessionId(), "pi-session");
  assert.equal(f.h.root.replacements, 1);
  assert.equal(f.hub.claims, 1);
  assert.equal(f.hub.receiptAttempts.length, 1);
  assert.equal(f.hub.receiptAttempts[0].cwd, f.f.directory, "fresh-reset Join advertises the new checkout");
  assert.equal(f.hub.capabilities, 2, "old and fresh connections negotiate independently");
  assert.ok(fresh.active().includes("policy-tool"));
  const promptResult = await fresh.prompt("BASE POLICY: protected paths");
  const prompt = promptResult.text;
  assert.equal(promptResult.options.forceSystemPrompt, undefined, "prompt sections are not flattened");
  assert.equal(promptResult.options.sections.preamble, "BASE POLICY: protected paths");
  assert.match(prompt, /^BASE POLICY: protected paths/);
  assert.match(prompt, /Role: implementor/);
  assert.equal(JSON.stringify(fresh.entries).includes("OLD SENTINEL"), false);
  assert.equal(fresh.entries.length, 0, "no old custom entries or receipt tickets copied");
  assert.equal(f.h.root.dispatches.length, 2);
  assert.equal(f.h.root.dispatches[0].depth, 0, "old command is dispatched outside hooks/tools");
  assertPrivate(f);
  await fresh.tools.get("mustr_roster").execute("fresh-roster", {}, undefined);
  assertPrivate(f);
});

for (const gate of ["idle", "busy", "human", "paused", "editor", "dialog", "pending", "staged", "delivery"]) {
  test(`strict reset pre-claim safety gate: ${gate}`, async t => {
    const f = await resetFixture(t);
    if (gate === "idle") f.h.idle(false);
    if (gate === "busy") await f.h.emit("agent_start");
    if (gate === "human") await f.h.emit("input", { source: "interactive" });
    if (gate === "paused") await f.h.commands.get("mustr").handler("pause", f.h.context);
    if (gate === "editor") f.h.editor("Unsent human draft");
    if (gate === "dialog") await f.h.emit("ui_prompt_start");
    if (gate === "pending") f.h.pending(true);
    if (gate === "staged") await f.h.tools.get("mustr_report").execute("stage", { task_id: "task", revision: 1, kind: "done", text: "Staged only" }, undefined);
    if (gate === "delivery") {
      f.h.deferInsertion(true);
      f.hub.inbox = { id: "unsettled", kind: "info", text: "Not confirmed inserted", wake: false };
      await f.h.emit("turn_end");
      await eventually(() => f.h.messages.length === 1);
    }
    f.hub.prepare(); await f.discover();
    await eventually(() => f.hub.statuses.some(s => s.status === "blocked"));
    assert.equal(f.hub.claims, 0);
    assert.equal(f.h.root.attempts, 0);
    if (gate === "staged") assert.equal(f.hub.records.filter(r => r.op === "report").length, 0, "control wins over staged report");
    await delay(30);
    assertPrivate(f);
  });
}

for (const race of ["editor", "pending", "human", "dialog", "topology", "connection", "generation"]) {
  test(`post-claim race consumes attempt without replacement: ${race}`, async t => {
    const f = await resetFixture(t);
    f.hub.claimHook = async current => {
      if (race === "editor") f.h.editor("Arrived while claim awaited");
      if (race === "pending") f.h.pending(true);
      if (race === "human") await f.h.emit("input", { source: "interactive" });
      if (race === "dialog") await f.h.emit("ui_prompt_start");
      if (race === "topology") f.f.panes.set("w1:p1", { pane_id: "w1:p1", workspace_id: "w2", agent: "pi" });
      if (race === "connection") current.close();
      if (race === "generation") await f.h.emit("session_before_switch", { reason: "resume" });
    };
    f.hub.prepare(); await f.discover();
    await eventually(() => f.hub.claims === 1);
    await delay(150);
    assert.equal(f.h.root.attempts, 0);
    assert.equal(f.h.root.replacements, 0);
    await f.h.emit("turn_end"); await delay(30);
    assert.equal(f.hub.claims, 1, "no repeated claim or context attempt");
    assertPrivate(f);
  });
}

for (const first of [true, false]) test(`cancellation ${first ? "before" : "after"} adapter switch handler keeps old context/role`, async t => {
  const f = await resetFixture(t);
  f.h.root.cancel = true; f.h.root.cancelFirst = first;
  f.hub.prepare(); await f.discover();
  await eventually(() => f.hub.statuses.some(s => s.status === "cancelled"));
  assert.equal(f.h.root.attempts, 1);
  assert.equal(f.h.root.replacements, 0);
  assert.equal(f.hub.receiptAttempts.length, 0);
  assert.ok(f.h.active().includes("mustr_roster"));
  assert.ok(f.h.active().includes("policy-tool"));
  const prompt = (await f.h.prompt("Base")).text;
  assert.match(prompt, /Role: implementor/);
  await f.discover(); await delay(30);
  assert.equal(f.h.root.attempts, 1);
  assertPrivate(f);
});

for (const outcome of ["lost", "duplicate", "not-confirmed"]) test(`claim ${outcome} never retries or calls newSession`, async t => {
  const f = await resetFixture(t);
  f.hub.claimHook = async current => {
    if (outcome === "lost") { current.close(); throw new Error(`Lost reply ${f.hub.control.token}`); }
    if (outcome === "duplicate") throw new Error("Already claimed; do not retry");
    return { claimed: false }; // Only an explicit first claimed:true can authorize replacement.
  };
  f.hub.prepare(); await f.discover();
  await eventually(() => f.hub.claims === 1);
  await delay(100);
  assert.equal(f.h.root.attempts, 0);
  await f.h.emit("turn_end"); await delay(30);
  assert.equal(f.hub.claims, 1);
  assertPrivate(f);
});

test("already claimed or terminal controls cannot authorize a second attempt", async t => {
  const f = await resetFixture(t);
  f.hub.prepare("claimed"); await f.discover(); await delay(100);
  assert.equal(f.hub.claims, 0); assert.equal(f.h.root.attempts, 0);
  assertPrivate(f);
});

test("lost fresh Join ACK re-acknowledges SAME fresh identity without another replacement", async t => {
  const f = await resetFixture(t);
  f.hub.failReceipt = 1;
  f.hub.prepare(); await f.discover();
  await eventually(() => f.hub.receiptAttempts.length === 2 && f.h.root.current.active().includes("mustr_roster"));
  assert.equal(f.h.root.attempts, 1);
  assert.equal(f.h.root.replacements, 1);
  assert.equal(f.hub.claims, 1);
  assert.equal(f.hub.receiptAttempts[0].agent_id, f.hub.receiptAttempts[1].agent_id);
  assertPrivate(f);
});

test("extension-origin input is not treated as a human interruption during fresh restore", async t => {
  const f = await resetFixture(t);
  f.h.root.beforeFreshRestore = fresh => fresh.emit("input", { source: "extension", text: "Extension-origin prompt" });
  f.hub.prepare(); await f.discover();
  await eventually(() => f.hub.completed && f.h.root.current.active().includes("mustr_roster"));
  assert.equal(f.hub.claims, 1);
  assert.equal(f.hub.receiptAttempts.length, 1);
  assertPrivate(f);
});

test("late restore after human abandon leaves fresh session unjoined, with no role/tools activation", async t => {
  const f = await resetFixture(t);
  f.h.root.beforeFreshRestore = () => f.hub.abandon();
  f.hub.prepare(); await f.discover();
  await eventually(() => f.h.root.replacements === 1 && f.hub.receiptAttempts.length === 1);
  await delay(80);
  const fresh = f.h.root.current;
  assert.deepEqual(fresh.active(), ["read", "bash", "policy-tool"]);
  assert.equal((await fresh.emit("before_agent_start", { systemPrompt: "Base" }))[0], undefined);
  assert.equal(f.hub.claims, 1);
  assert.equal(f.hub.receiptAttempts.length, 1, "authoritative rejection is not retried as ordinary Join");
  assertPrivate(f);
});

test("missing local role prevents fresh role activation or receipt", async t => {
  const f = await resetFixture(t);
  f.h.root.beforeFreshRestore = async () => rm(`${f.f.directory}/config/roles/implementor.md`);
  f.hub.prepare(); await f.discover();
  await eventually(() => f.h.root.replacements === 1 && f.h.root.current.notifications.some(n => /manual/.test(n.message)));
  assert.deepEqual(f.h.root.current.active(), ["read", "bash", "policy-tool"]);
  assert.equal(f.hub.receiptAttempts.length, 0);
  assertPrivate(f);
});

test("broken requested composition reports its path and prevents fresh receipt or activation", async t => {
  const f = await resetFixture(t);
  f.h.root.beforeFreshRestore = () => writeFile(`${f.f.directory}/config/roles/implementor.md`, '+++\ncompose = ["missing-fresh-component"]\n+++\nBroken fresh role');
  f.hub.prepare(); await f.discover();
  await eventually(() => f.h.root.current.notifications.some(n => /missing-fresh-component/.test(n.message)));
  assert.deepEqual(f.h.root.current.active(), ["read", "bash", "policy-tool"]);
  assert.equal((await f.h.root.current.emit("before_agent_start", { systemPrompt: "Base" }))[0], undefined);
  assert.equal(f.hub.receiptAttempts.length, 0);
  assert.ok(f.h.root.current.notifications.some(n => /manual/.test(n.message)));
  assertPrivate(f);
});

test("fresh restore resolves updated role despite unrelated invalid graph", async t => {
  const f = await resetFixture(t);
  f.h.root.beforeFreshRestore = async () => {
    await writeFile(`${f.f.directory}/config/roles/broken.md`, '+++\ncompose = ["missing"]\n+++\nBroken unrelated role');
    await writeFile(`${f.f.directory}/config/roles/implementor.md`, "UPDATED FRESH ROLE");
  };
  f.hub.prepare(); await f.discover();
  await eventually(() => f.hub.completed && f.h.root.current.active().includes("mustr_roster"));
  const promptResult = await f.h.root.current.prompt("Base");
  const text = promptResult.text;
  assert.equal(promptResult.options.forceSystemPrompt, undefined);
  assert.match(promptResult.options.sections.mustr_role, /UPDATED FRESH ROLE/);
  assert.match(text, /UPDATED FRESH ROLE/);
  assert.equal(f.hub.receiptAttempts.length, 1);
  assertPrivate(f);
});

test("fresh server graph rejection shows cause without retrying receipt or activating tools", async t => {
  const f = await resetFixture(t);
  // Admission seam: reject AFTER local targeted resolution, before fake hub mutation.
  const connect = HubClient.prototype.connect;
  let attempts = 0;
  HubClient.prototype.connect = async function(path, hello) {
    if (hello.reset) { attempts++; throw new HubRemoteError("role implementor -> missing-fresh-component: unknown component"); }
    return connect.call(this, path, hello);
  };
  f.hub.prepare(); await f.discover();
  await eventually(() => f.h.root.current.notifications.some(n => /missing-fresh-component/.test(n.message)));
  await delay(300);
  assert.equal(attempts, 1); assert.equal(f.hub.completed, false);
  assert.deepEqual(f.h.root.current.active(), ["read", "bash", "policy-tool"]);
  assert.equal((await f.h.root.current.emit("before_agent_start", { systemPrompt: "Base" }))[0], undefined);
  assertPrivate(f);
});

test("forged restore nonce and untrusted role envelope cannot activate role/tools", async t => {
  const f = await resetFixture(t);
  const fresh = harness(f.f.directory, { sessionId: "unjoined-fresh" });
  await extension(fresh.api); await fresh.emit("session_start", { reason: "new" });
  f.f.beforeCleanup(() => fresh.emit("session_shutdown", { reason: "quit" }));
  await fresh.commands.get("mustr-reset-restore").handler("invented peer token", fresh.context);
  assert.equal(f.hub.receiptAttempts.length, 0);
  f.hub.prepare(); f.hub.claimed = true;
  const nonce = "untrusted-restore-test";
  registry().restores.set(nonce, { control: { ...f.hub.control, role: "reviewer" }, identity: { ...f.f.discovery, pane_id: "w1:p1" }, freshId: "unjoined-fresh", instance: registry().ready.get("unjoined-fresh") });
  await fresh.commands.get("mustr-reset-restore").handler(nonce, fresh.context);
  assert.equal(f.hub.receiptAttempts.length, 1);
  assert.deepEqual(fresh.active(), ["read", "bash", "policy-tool"]);
  assert.equal((await fresh.emit("before_agent_start", { systemPrompt: "Base" }))[0], undefined);
  assert.equal(registry().restores.has(nonce), false, "restoration nonce is consumed before awaits");
  assertPrivate(f);
});

test("separately evaluated module factories share only opaque private restoration approval", async t => {
  const f = await resetFixture(t);
  const duplicate = await import(`../index.ts?fresh-module=${Date.now()}`);
  f.h.root.factory = duplicate.default;
  f.hub.prepare(); await f.discover();
  await eventually(() => f.hub.completed && f.h.root.current.active().includes("mustr_roster"));
  assert.equal(f.h.root.replacements, 1);
  assertPrivate(f);
});

for (const point of ["before-handler", "roles", "capability", "subscribe", "topology", "disconnect"]) test(`fresh restore rechecks human/connection safety at ${point}`, async t => {
  const f = await resetFixture(t);
  f.h.root.beforeFreshRestore = async fresh => {
    if (point === "before-handler") await fresh.emit("input", { source: "interactive" });
    if (point === "roles") setTimeout(() => { void fresh.emit("input", { source: "interactive" }); }, 0);
    f.hub.requestHook = async (current, command) => {
      if (!current.hello?.reset) return;
      if (point === "capability" && command.op === "reset_capability") await fresh.emit("input", { source: "interactive" });
      if (point === "subscribe" && command.op === "subscribe") await fresh.emit("input", { source: "interactive" });
      if (point === "topology" && command.op === "subscribe") f.f.panes.set("w1:p1", { pane_id: "w1:p1", workspace_id: "w2", agent: "pi" });
      if (point === "disconnect" && command.op === "subscribe") current.close();
    };
  };
  f.hub.prepare(); await f.discover();
  await eventually(() => f.h.root.replacements === 1 && f.h.root.current.notifications.some(n => /manual/.test(n.message)));
  assert.deepEqual(f.h.root.current.active(), ["read", "bash", "policy-tool"]);
  assert.equal((await f.h.root.current.emit("before_agent_start", { systemPrompt: "Base" }))[0], undefined);
  assert.equal(f.h.root.attempts, 1);
  if (["before-handler", "roles"].includes(point)) assert.equal(f.hub.receiptAttempts.length, 0);
  assertPrivate(f);
});

test("abandon racing claim reply is observed before context replacement", async t => {
  const f = await resetFixture(t);
  f.hub.claimHook = () => { f.hub.abandon(); };
  f.hub.prepare(); await f.discover();
  await eventually(() => f.hub.claims === 1);
  await delay(150);
  assert.equal(f.h.root.attempts, 0);
  assert.equal(f.hub.receiptAttempts.length, 0);
  assertPrivate(f);
});

test("raw peer arguments and terminal controls never become replacement authorization", async t => {
  const f = await resetFixture(t);
  await f.h.commands.get("mustr-reset-internal").handler(f.hub.control.token, f.h.context);
  assert.equal(f.hub.claims, 0);
  for (const state of ["blocked", "cancelled", "failed", "uncertain"]) {
    f.hub.prepare(state); await f.discover();
  }
  await delay(80);
  assert.equal(f.hub.claims, 0);
  assert.equal(f.h.root.attempts, 0);
  assertPrivate(f);
});

test("old hub compatibility does not send unknown reset capability command", async t => {
  const f = await resetFixture(t, { oldHub: true });
  assert.equal(f.hub.capabilities, 0);
  assert.ok(f.h.active().includes("mustr_roster"));
  const result = await f.h.tools.get("mustr_roster").execute("normal", {}, undefined);
  assert.equal(result.details.members[0].id, "pi-session");
  assert.equal(f.h.root.attempts, 0);
});
