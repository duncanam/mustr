import test from "node:test";
import assert from "node:assert/strict";
import { chmod, writeFile, readFile, rename, unlink } from "node:fs/promises";
import { join } from "node:path";
import { fixture, eventually, delay } from "../../tests/support.mjs";
import { HubClient, HubRemoteError } from "../transport.mjs";
import { harness } from "./adapter-harness.mjs";

const role = { name: "implementor", description: "isolated fixture", prompt: "No external work." };
const finishedTaskStates = new Set(["accepted", "cancelled"]);
const task = (id, requester, owner, state = "active", revision = 1, context = undefined) => ({ id, requester, owner, space_id: "w1", state, revision, title: id, ...(context ? { context } : {}) });
const userStart = text => ({ message: { role: "user", content: [{ type: "text", text }] } });
const connectionRetiredError = () => new HubRemoteError("connection is retired; rejoin explicitly from the correct session", "connection_retired");
const envelope = fields => ({ id: "message", space_id: "w1", from: "peer", to: "pi-session", task_id: null, revision: null, kind: "request", text: "Authorized fixture input.", wake: true, ...fields });

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}
function holdNext(hub, op) {
  const started = deferred();
  const release = deferred();
  hub.holdNext = { op, started, release };
  return { started: started.promise, release: release.resolve, reject: release.reject };
}
function mockHub(seedTasks = []) {
  const state = { tasks: structuredClone(seedTasks), inbox: [], calls: [], syncs: [], reports: [], clients: [], offered: new Map(), uncertain: new Map(), nextTask: 1, heldRequests: new Set() };
  const original = { connect: HubClient.prototype.connect, request: HubClient.prototype.request, close: HubClient.prototype.close };
  HubClient.prototype.connect = async function (_path, hello) {
    this.__mustrAgentId = hello.agent_id;
    this.connected = true;
    state.clients.push(this);
    return { delivery_policy: 1, inbox_notifications: 1, reset_protocol: 1, context_selection: 1, context_modes: ["reuse", "fresh"] };
  };
  HubClient.prototype.request = async function (command) {
    const agentId = this.__mustrAgentId;
    state.calls.push({ agentId, ...command });
    if (state.rejectNext?.op === command.op) {
      const error = state.rejectNext.error ?? new HubRemoteError("Fixture identity lookup unavailable; not executed", "identity_unavailable");
      state.rejectNext = undefined;
      throw error;
    }
    const finish = async result => {
      const hold = state.holdNext?.op === command.op ? state.holdNext : undefined;
      if (hold) {
        state.holdNext = undefined;
        state.heldRequests.add(hold);
        hold.started.resolve();
        try { await hold.release.promise; }
        finally { state.heldRequests.delete(hold); }
      }
      return result;
    };
    if (command.op === "reset_capability") return finish({ reset_protocol: 1 });
    if (command.op === "reset_status") return finish({ ok: true });
    if (command.op === "subscribe" || command.op === "leave" || command.op === "mode") return finish({ ok: true });
    if (command.op === "configure_todo" || command.op === "configure_pr") return finish({ configured: true, enabled: false });
    if (command.op === "sync") {
      state.syncs.push({ agentId, ...command });
      const result = {
        members: [{ id: agentId, name: "fixture", role: "implementor", activity: command.activity }],
        tasks: state.omitTasks ? [] : structuredClone(state.tasks), automation: {}, insertion_policy: "idle_only",
      };
      if (state.resetSummary) result.reset_summary = structuredClone(state.resetSummary);
      if (state.resetControl) result.reset_control = structuredClone(state.resetControl);
      if (state.retireAfterResetControl && state.resetControl) {
        state.retireAfterResetControl = false;
        state.rejectNext = { op: "sync", error: connectionRetiredError() };
      }
      if (state.omitTasks) result.tasks_truncated = true;
      if (command.receive && ![...state.offered.values()].some(message => message.to === agentId)) {
        const index = state.inbox.findIndex(message => message.to === agentId);
        if (index >= 0) {
          const [message] = state.inbox.splice(index, 1);
          state.offered.set(message.id, message);
          result.inbox = structuredClone(message);
          const beforeOffer = state.beforeOffer; state.beforeOffer = undefined;
          beforeOffer?.();
        }
      }
      return finish(result);
    }
    if (command.op === "ack") { state.offered.delete(command.message_id); return finish({ acked: true }); }
    if (command.op === "defer") {
      const message = state.offered.get(command.message_id);
      if (message) state.inbox.unshift(message);
      state.offered.delete(command.message_id);
      return finish({ deferred: true });
    }
    if (command.op === "report") {
      const beforeReport = state.beforeReport; state.beforeReport = undefined;
      beforeReport?.(command);
      const current = state.tasks.find(item => item.id === command.task_id);
      if (!current) throw new HubRemoteError("unknown task", undefined);
      if (current.owner !== agentId) throw new HubRemoteError("only the owner can report on unfinished work", undefined);
      if (current.revision !== command.revision || finishedTaskStates.has(current.state)
        || current.state === "queued" || (current.state === "review" && command.kind !== "status")
        || (command.kind === "progress" && current.state !== "active")) {
        throw new HubRemoteError("stale task revision or task is no longer reportable; refresh the roster before acting", "task_conflict");
      }
      state.reports.push({ agentId, ...command });
      if (command.kind === "done") current.state = "review";
      else if (command.kind === "blocked" || command.kind === "waiting") current.state = command.kind;
      return finish({ reported: true, task: structuredClone(current) });
    }
    if (command.op === "assign") {
      const id = `assigned-${state.nextTask++}`;
      state.tasks.push(task(id, agentId, command.to, "assigned", 1, command.context));
      return finish({ task_id: id });
    }
    if (["accept", "guide", "cancel"].includes(command.op)) {
      const current = state.tasks.find(item => item.id === command.task_id);
      if (!current) throw new HubRemoteError("unknown task", undefined);
      if (current.requester !== agentId) throw new HubRemoteError("only the requester may act on this task", undefined);
      if (current.revision !== command.revision) throw new HubRemoteError("stale task revision; refresh the roster before acting", undefined);
      if (command.op === "accept") current.state = "accepted";
      else if (command.op === "guide") { current.revision++; current.state = "assigned"; }
      else current.state = "cancelled";
      return finish({ task: structuredClone(current) });
    }
    if (command.op === "send") return finish({ sent: true });
    throw new Error(`Unexpected fixture Hub operation: ${command.op}`);
  };
  HubClient.prototype.close = function () {
    if (!this.connected) return;
    this.connected = false;
    for (const [id, message] of state.offered) {
      if (message.to === this.__mustrAgentId) { state.offered.delete(id); state.uncertain.set(id, message); }
    }
    this.onClose?.();
  };
  state.deliver = message => {
    state.inbox.push(structuredClone(message));
    for (const client of state.clients) if (client.connected && client.__mustrAgentId === message.to) client.onInboxReady?.();
  };
  state.restore = () => {
    for (const hold of state.heldRequests) hold.release.resolve();
    HubClient.prototype.connect = original.connect;
    HubClient.prototype.request = original.request;
    HubClient.prototype.close = original.close;
  };
  return state;
}

async function setup(t, tasks = []) {
  const f = await fixture(t);
  const keys = ["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR", "MUSTR_BIN"];
  const previousEnv = new Map(keys.map(key => [key, process.env[key]]));
  const identity = await f.discover("w1:p1");
  const fakeBinary = join(f.directory, "fake-mustr.mjs");
  const contextControl = join(f.directory, "context-control.json");
  const contextCalls = join(f.directory, "context-calls.log");
  await writeFile(contextControl, "{}");
  await writeFile(fakeBinary, `#!/usr/bin/env node\nimport { appendFileSync, readFileSync } from 'node:fs';\nconst args = process.argv.slice(2);\nif (args.includes("roles")) console.log(${JSON.stringify(JSON.stringify([role]))});\nelse if (args.includes("context")) {\nappendFileSync(${JSON.stringify(contextCalls)}, 'context\\n');\nif (process.env.HERDR_ENV !== '1' || !process.env.HERDR_SOCKET_PATH || !process.env.HERDR_PANE_ID) { console.error('herdr context inputs are missing'); process.exit(1); }\nconst control = JSON.parse(readFileSync(${JSON.stringify(contextControl)}, 'utf8'));\nif (control.signal) process.kill(process.pid, control.signal);\nif (Number.isSafeInteger(control.exitCode)) { if (control.stderr) console.error(control.stderr); process.exit(control.exitCode); }\nif (typeof control.output === 'string') process.stdout.write(control.output);\nelse {\nconst route = control.routeIdentities?.[process.env.HERDR_SOCKET_PATH] ?? {};\nconst discovered = {...${JSON.stringify(identity)}, ...route, ...control.identity};\nif (!Object.hasOwn(route, 'pane_id') && !Object.hasOwn(control.identity ?? {}, 'pane_id')) discovered.pane_id = process.env.HERDR_PANE_ID;\nconsole.log(JSON.stringify(discovered));\n}\n}\nelse process.exitCode = 2;\n`);
  await chmod(fakeBinary, 0o755);
  Object.assign(process.env, f.env, { HERDR_PANE_ID: "w1:p1", MUSTR_BIN: fakeBinary });
  const hub = mockHub(tasks);
  let h;
  f.beforeCleanup(async () => {
    try { if (h) await h.emit("session_shutdown"); }
    finally {
      hub.restore();
      for (const [key, value] of previousEnv) value === undefined ? delete process.env[key] : process.env[key] = value;
    }
  });
  const { default: extension } = await import(`../index.ts?mock-liveness=${Date.now()}-${Math.random()}`);
  h = harness(f.directory, { factory: extension });
  await extension(h.api);
  await h.emit("session_start");
  await h.commands.get("role").handler("implementor fixture-agent", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("ready", {}, undefined));
  const setContext = async value => {
    await writeFile(`${contextControl}.next`, JSON.stringify(value));
    await rename(`${contextControl}.next`, contextControl);
  };
  const contextCallCount = async () => (await readFile(contextCalls, "utf8").catch(() => "")).split("\n").filter(Boolean).length;
  const removeBinary = () => unlink(fakeBinary);
  return { f, h, hub, setContext, contextCallCount, removeBinary };
}

function reportCalls(hub, taskId) { return hub.reports.filter(call => call.task_id === taskId); }
const unavailableIdentityError = () => new HubRemoteError("herdr temporarily unavailable; Mustr delivery paused while identity is revalidated.", "identity_unavailable");
async function enterIdentityPending(h, hub, setContext) {
  hub.rejectNext = { op: "sync", error: unavailableIdentityError() };
  await setContext({ exitCode: 75, stderr: "synthetic herdr outage" });
  await h.emit("turn_end");
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
}
async function acceptUser(h, text, source = "interactive") {
  await h.emit("input", { source, text, streamingBehavior: "steer" });
  await h.emit("message_start", userStart(text));
}


test("manual pause survives accepted human input and only explicit resume restores delivery", { timeout: 15000 }, async t => {
  const { h, hub } = await setup(t);
  await h.commands.get("mustr").handler("pause", h.context);
  assert.match(h.statuses.at(-1), /manual pause/);
  hub.deliver(envelope({ id: "manual-message" }));
  await eventually(() => hub.syncs.some(sync => sync.receive === false));
  await acceptUser(h, "A fresh prompt is not /mustr resume");
  await delay(100);
  assert.equal(h.messages.length, 0);
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "manual inbox pause must not block human-directed tools");

  await h.commands.get("mustr").handler("resume", h.context);
  await eventually(() => h.messages.some(item => item.message.details?.id === "manual-message"));
  assert.doesNotMatch(h.statuses.at(-1), /manual pause/);
});


test("explicit Mustr decision pause gates delivery only; input and abort never resume or create a hold", { timeout: 18000 }, async t => {
  const { h, hub } = await setup(t);
  const pause = await h.tools.get("mustr_pause").execute("pause", { reason: "A human decision is genuinely required" }, undefined);
  assert.equal(pause.terminate, true);
  await h.settle(false);
  assert.match(h.statuses.at(-1), /awaiting human decision/);
  hub.deliver(envelope({ id: "decision-message" }));
  await eventually(() => hub.syncs.some(sync => sync.receive === false));

  await acceptUser(h, "Extension prompt", "extension");
  await acceptUser(h, "Human input does not resume Mustr delivery", "interactive");
  await delay(100);
  assert.equal(h.messages.length, 0, "neither synthetic nor human user starts release the explicit coordination hold");
  assert.match(h.statuses.at(-1), /awaiting human decision/);
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "the explicit Mustr pause does not block ordinary Pi tools");

  await h.commands.get("mustr").handler("resume", h.context);
  await eventually(() => h.messages.some(item => item.message.details?.id === "decision-message"));
  assert.doesNotMatch(h.statuses.at(-1), /awaiting human decision/);

  await h.settle(true); // Pi owns abort state; it is not a persistent Mustr pause.
  assert.doesNotMatch(h.statuses.at(-1), /interrupted/);
  hub.deliver(envelope({ id: "after-abort" }));
  await eventually(() => h.messages.some(item => item.message.details?.id === "after-abort"));
  assert.doesNotMatch(h.statuses.at(-1), /interrupted/);
});


test("a successful native retry clears only its transient gate after completed settlement", { timeout: 15000 }, async t => {
  const { h, hub } = await setup(t);
  h.idle(false);
  await h.emit("agent_start");
  await h.emit("message_end", { message: { role: "assistant", stopReason: "error" } });
  assert.match(h.statuses.at(-1), /retrying after provider error/);
  hub.deliver(envelope({ id: "after-retry" }));
  await eventually(() => hub.syncs.some(sync => sync.receive === false));
  await delay(100);
  assert.equal(h.messages.length, 0);

  await h.emit("message_end", { message: { role: "assistant", stopReason: "stop" } });
  await h.emit("agent_before_settle", { outcome: "completed" });
  assert.match(h.statuses.at(-1), /retrying after provider error/, "success is not committed before settlement");
  await h.settle(false);
  await eventually(() => h.messages.some(item => item.message.details?.id === "after-retry"));
  assert.doesNotMatch(h.statuses.at(-1), /retrying after provider error|unresolved error/);
  assert.equal(hub.calls.filter(call => call.op === "report").length, 0, "adapter does not invent a retry report or model turn");
});


test("final native retry error clears its transient delivery gate without blocking tools or retrying", { timeout: 15000 }, async t => {
  const { h, hub } = await setup(t);
  h.idle(false);
  await h.emit("agent_start");
  await h.emit("message_end", { message: { role: "assistant", stopReason: "error" } });
  assert.match(h.statuses.at(-1), /retrying after provider error/);
  hub.deliver(envelope({ id: "after-final-error" }));
  await eventually(() => hub.syncs.some(sync => sync.receive === false));
  assert.equal(h.messages.length, 0, "autonomous delivery waits for native settlement during retry");
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "retry state does not impose a global ordinary-tool lock");

  await h.settle(false); // The same extension-visible final event can mean exhaustion or retry cancellation.
  await eventually(() => h.messages.some(item => item.message.details?.id === "after-final-error"));
  assert.doesNotMatch(h.statuses.at(-1), /retrying after provider error|unresolved error/);
  assert.equal(hub.calls.filter(call => call.op === "report").length, 0, "mustr does not invent an adapter retry");
});


test("user and extension starts neither discard staged reports nor reopen published task scope", { timeout: 15000 }, async t => {
  const parent = task("parent", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  const staged = await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Fixture parent is done." }, undefined);
  assert.equal(staged.terminate, true);
  await acceptUser(h, "Extension-origin start does not publish or clear the report", "extension");
  await acceptUser(h, "Human start does not publish or clear the report");
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0].block, true, "the reporting native run keeps its provisional productive-tool fence");
  await assert.rejects(h.tools.get("mustr_report").execute("staged-progress", { task_id: parent.id, revision: 1, kind: "progress", text: "Do not replace the staged terminal claim." }, undefined), /terminal report.*staged/);
  assert.equal(reportCalls(hub, parent.id).length, 0, "Pi settlement, not user-message boundaries, controls report publication");
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => reportCalls(hub, parent.id).length === 1);
  assert.equal(hub.tasks.find(item => item.id === parent.id).state, "review");
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "a later ordinary host turn remains Pi-owned and runnable");
  await assert.rejects(h.tools.get("mustr_report").execute("old-parent", { task_id: parent.id, revision: 1, kind: "progress", text: "Do not reopen the reported task." }, undefined), /terminal report/);

  const second = task("second", "requester", "pi-session");
  hub.tasks.push(second);
  await h.tools.get("mustr_roster").execute("refresh", {}, undefined);
  await h.tools.get("mustr_report").execute("stage", { task_id: second.id, revision: 1, kind: "done", text: "Staged but not yet sent." }, undefined);
  await assert.rejects(h.tools.get("mustr_report").execute("staged-progress-second", { task_id: second.id, revision: 1, kind: "progress", text: "Same revision stays fenced until settlement." }, undefined), /terminal report.*staged/);
  await acceptUser(h, "Synthetic start leaves the second report staged", "extension");
  await acceptUser(h, "Human start also leaves the second report staged");
  assert.equal(reportCalls(hub, second.id).length, 0);
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => reportCalls(hub, second.id).length === 1);
  assert.equal(hub.tasks.find(item => item.id === second.id).state, "review");
});


test("an aborted staged report is discarded without poisoning later host tools or replaying", { timeout: 12000 }, async t => {
  const parent = task("aborted-parent", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Must not publish after abort." }, undefined);
  await h.settle(true);
  await delay(100);
  assert.equal(reportCalls(hub, parent.id).length, 0);
  assert.doesNotMatch(h.statuses.at(-1), /interrupted|awaiting guidance/);
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "Pi owns the next host turn; the aborted report leaves no global tool hold");
  await h.tools.get("mustr_report").execute("fresh-status", { task_id: parent.id, revision: 1, kind: "status", text: "Status is observational." }, undefined);
  assert.deepEqual(reportCalls(hub, parent.id).map(report => report.kind), ["status"], "only explicit status ran; the aborted terminal report was not replayed");
  assert.equal(parent.state, "active");
});

test("latest sync discards a staged report for a cancelled task without dropping the connection", { timeout: 15000 }, async t => {
  const parent = task("cancelled-stage", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "A staged report that becomes obsolete." }, undefined);
  Object.assign(hub.tasks[0], { revision: 2, state: "cancelled" });
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => h.notifications.some(item => /staged report is obsolete/.test(item.message)));

  assert.equal(hub.calls.filter(call => call.op === "report").length, 0, "the accepted current snapshot prevents dispatch of an obsolete report");
  assert.equal(hub.reports.length, 0);
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
  assert.equal(hub.tasks[0].state, "cancelled");
  const notice = h.notifications.filter(item => /staged report is obsolete/.test(item.message));
  assert.equal(notice.length, 1);
  assert.equal(notice[0].level, "info");

  const roster = await h.tools.get("mustr_roster").execute("refresh", {}, undefined);
  assert.equal(roster.details.tasks.find(item => item.id === parent.id).revision, 2, "the same connection can fetch the current task state");
  assert.equal(hub.clients.length, 1);
  await delay(50);
  assert.equal(hub.calls.filter(call => call.op === "report").length, 0, "the obsolete report is never replayed");
  assert.equal(h.notifications.filter(item => /staged report is obsolete/.test(item.message)).length, 1, "reconciliation is shown once");
});

test("a missing row in a truncated sync leaves conflict judgment to the hub", { timeout: 15000 }, async t => {
  const parent = task("truncated-stage", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "The current roster may omit this task." }, undefined);
  Object.assign(hub.tasks[0], { revision: 2, state: "cancelled" });
  hub.omitTasks = true;
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => h.notifications.some(item => /staged report is obsolete/.test(item.message)));

  assert.equal(hub.syncs.at(-1).receive, false);
  assert.equal(hub.calls.filter(call => call.op === "report").length, 1, "the missing row is not treated as proof of conflict");
  assert.equal(hub.reports.length, 0, "the authoritative task_conflict still prevents execution");
  assert.equal(hub.tasks[0].state, "cancelled");
  assert.equal(hub.clients[0].connected, true, "the typed report conflict keeps the connection live");
  assert.equal(h.notifications.filter(item => /staged report is obsolete/.test(item.message)).length, 1);
});

test("an after-sync Guide race returns task_conflict, discards only the staged report, and keeps coordination live", { timeout: 15000 }, async t => {
  const parent = task("guide-race", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "The accepted sync still shows revision one." }, undefined);
  hub.beforeReport = () => { Object.assign(hub.tasks[0], { revision: 2, state: "assigned" }); };
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => h.notifications.some(item => /staged report is obsolete/.test(item.message)));

  assert.equal(hub.syncs.at(-1).receive, false);
  assert.equal(hub.calls.filter(call => call.op === "report").length, 1, "the stale report was attempted once after the older accepted sync");
  assert.equal(hub.reports.length, 0, "the typed hub conflict proves the report did not execute");
  assert.equal(hub.tasks[0].revision, 2);
  assert.equal(hub.tasks[0].state, "assigned", "the new task remains assigned, not paused by disconnect handling");
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true, "task_conflict does not retire a healthy connection");
  const notice = h.notifications.filter(item => /staged report is obsolete/.test(item.message));
  assert.equal(notice.length, 1);
  assert.equal(notice[0].level, "info");

  const roster = await h.tools.get("mustr_roster").execute("refresh", {}, undefined);
  assert.equal(roster.details.tasks.find(item => item.id === parent.id).revision, 2);
  await delay(50);
  assert.equal(hub.calls.filter(call => call.op === "report").length, 1, "the rejected report is not replayed");
  assert.equal(hub.clients.length, 1, "the accepted follow-up sync used the same client");
  assert.equal(h.notifications.filter(item => /staged report is obsolete/.test(item.message)).length, 1);
});

test("direct stale report, Guide and Cancel actions remain rejected with roster advice", { timeout: 12000 }, async t => {
  const current = task("direct-stale", "pi-session", "pi-session", "active", 2);
  const { h, hub } = await setup(t, [current]);
  await assert.rejects(
    h.tools.get("mustr_report").execute("stale-report", { task_id: current.id, revision: 1, kind: "progress", text: "Do not reopen the old revision." }, undefined),
    /stale task revision.*refresh the roster/,
  );
  await assert.rejects(
    h.tools.get("mustr_guide").execute("stale-guide", { task_id: current.id, revision: 1, text: "Stale direct guidance." }, undefined),
    /stale task revision.*refresh the roster/,
  );
  await assert.rejects(
    h.tools.get("mustr_cancel").execute("stale-cancel", { task_id: current.id, revision: 1, text: "Stale direct cancellation." }, undefined),
    /stale task revision.*refresh the roster/,
  );
  assert.deepEqual(hub.calls.filter(call => ["report", "guide", "cancel"].includes(call.op)).map(call => call.op), ["report", "guide", "cancel"], "direct rejected UI actions are not replayed or treated as success");
  assert.equal(hub.reports.length, 0);
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
  assert.equal(h.notifications.filter(item => /staged report is obsolete/.test(item.message)).length, 0, "direct action errors are not swallowed as staged-report reconciliation");
});


test("child/manager messages retain scoped status while ordinary tools run and old reports stay fenced", { timeout: 20000 }, async t => {
  const parent = task("parent", "requester", "pi-session");
  const child = task("child", "pi-session", "child-agent", "review", 3);
  const waiting = task("waiting", "pi-session", "waiter", "waiting", 4);
  const { h, hub } = await setup(t, [parent, child, waiting]);
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Parent work is done." }, undefined);
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => reportCalls(hub, parent.id).length === 1);

  hub.deliver(envelope({ id: "child-done", from: "child-agent", task_id: child.id, revision: child.revision, kind: "done", text: "Child completed; inspect evidence.", wake: true }));
  await eventually(() => h.entries.some(entry => entry.type === "custom_message" && entry.customType === "mustr" && entry.details?.id === "child-done"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "ordinary review tools remain Pi-owned");
  assert.match(h.statuses.at(-1), /reviewing delegated task child/, "only the validated task/revision envelope creates a review status scope");
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "review may run tests without reopening the parent task");
  await h.tools.get("mustr_accept").execute("accept-child", { task_id: child.id, revision: child.revision, text: "Independently reviewed fixture evidence." }, undefined);
  const reportCount = hub.reports.length;
  await assert.rejects(h.tools.get("mustr_report").execute("parent-progress", { task_id: parent.id, revision: 1, kind: "progress", text: "Do not reopen completed parent work." }, undefined), /prior task report|already has a terminal report/);
  assert.equal(hub.reports.length, reportCount, "old-parent report is rejected before dispatch");
  assert.equal(hub.tasks.find(item => item.id === parent.id).state, "review");
  await h.settle(false);

  hub.deliver(envelope({ id: "waiting-info", from: "waiter", task_id: waiting.id, revision: waiting.revision, kind: "blocked", text: "Waiting report is informational only.", wake: false }));
  await eventually(() => h.entries.some(entry => entry.type === "custom_message" && entry.customType === "mustr" && entry.details?.id === "waiting-info"));
  assert.equal(h.messages.find(item => item.message.details?.id === "waiting-info").options.triggerTurn, false);
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "ordinary tools are not gated by Mustr review scope");
  assert.doesNotMatch(h.statuses.at(-1), /reviewing delegated task waiting/);
  hub.deliver(envelope({ id: "waiting-wake-drift", from: "waiter", task_id: waiting.id, revision: waiting.revision, kind: "blocked", text: "A waiting state must not gain a waking scope.", wake: true }));
  await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "waiting-wake-drift"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "invalid review envelope does not alter ordinary host tools");
  assert.doesNotMatch(h.statuses.at(-1), /reviewing delegated task waiting/);
  await h.settle(false);

  hub.deliver(envelope({ id: "manager-checkin", kind: "checkin", from: null, to: "pi-session", task_id: null, revision: null, text: "Reconcile approved coordination goals.", wake: true }));
  await eventually(() => h.entries.some(entry => entry.type === "custom_message" && entry.customType === "mustr" && entry.details?.id === "manager-checkin"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "taskless authoritative check-in permits local reconciliation");
  await h.tools.get("mustr_assign").execute("delegate", { to: "child-agent", title: "Follow-up", text: "Verify the fixture follow-up.", context: "reuse" }, undefined);
  await h.tools.get("mustr_report").execute("parent-status", { task_id: parent.id, revision: 1, kind: "status", text: "Parent remains under review." }, undefined);
  const afterStatus = hub.reports.length;
  await assert.rejects(h.tools.get("mustr_report").execute("parent-progress-again", { task_id: parent.id, revision: 1, kind: "progress", text: "Still forbidden." }, undefined), /prior task report|already has a terminal report/);
  assert.equal(hub.reports.length, afterStatus, "status is allowed but fresh work claims on the stopped task are not");
  await h.settle(false);
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "coordination-scope expiry never becomes a global ordinary-tool lock");
  await assert.rejects(h.tools.get("mustr_report").execute("old-parent-after-checkin", { task_id: parent.id, revision: 1, kind: "progress", text: "Still stopped." }, undefined), /terminal report/);

  const workerCheckin = envelope({ id: "worker-checkin", kind: "checkin", from: "child-agent", task_id: child.id, revision: child.revision, wake: true });
  hub.deliver(workerCheckin);
  await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "worker-checkin"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "invalid worker scope changes no Pi tool execution policy");
  await h.settle(false);

  const next = task("next", "manager", "pi-session", "assigned", 1);
  hub.tasks.push(next);
  h.deferInsertion(true);
  hub.deliver(envelope({ id: "new-assignment", kind: "assignment", from: "manager", task_id: next.id, revision: next.revision, wake: true, text: "Fresh authorized task." }));
  await eventually(() => h.messages.some(item => item.message.details?.id === "new-assignment"));
  await h.emit("message_start", userStart("Unrelated start before branch insertion"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "ordinary tools do not wait for a Mustr permit");
  const assignmentIndex = h.messages.findIndex(item => item.message.details?.id === "new-assignment");
  await h.insert(assignmentIndex);
  h.deferInsertion(false);
  await h.emit("tool_call", { toolName: "read" }); // Commits the exact assignment permit on current-branch insertion evidence.
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "fresh assignment remains eligible for scoped Mustr actions");
  await assert.rejects(h.tools.get("mustr_report").execute("old-parent", { task_id: parent.id, revision: 1, kind: "progress", text: "Still fenced." }, undefined), /terminal report/);
  await h.tools.get("mustr_report").execute("new-done", { task_id: next.id, revision: next.revision, kind: "done", text: "New task completed once." }, undefined);
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => reportCalls(hub, next.id).length === 1);
  assert.equal(reportCalls(hub, parent.id).filter(call => call.kind !== "status").length, 1);
});

test("healthy connected pumps reuse the pinned identity without spawning context discovery", { timeout: 8000 }, async t => {
  const { h, hub, contextCallCount } = await setup(t);
  const contexts = await contextCallCount();
  const syncs = hub.syncs.length;

  await h.emit("turn_end");
  await eventually(() => hub.syncs.length >= syncs + 2, 3500);
  assert.equal(await contextCallCount(), contexts, "ordinary periodic syncs reuse the identity discovered for Join");
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
});

test("a changed local pane input that rediscovers the same identity refreshes the route snapshot", { timeout: 12000 }, async t => {
  const { h, hub, setContext, contextCallCount } = await setup(t);
  const contextsBefore = await contextCallCount();
  process.env.HERDR_PANE_ID = "w1:p1-alias";
  await setContext({ identity: { pane_id: "w1:p1" } });
  await assert.rejects(h.tools.get("mustr_send").execute("before-route-validation", { to: "peer", text: "Blocked until discovery", wake: false }), /identity became unverified|identity check unavailable/);
  assert.equal(hub.calls.filter(call => call.op === "send").length, 0, "the old route is not used before local revalidation");

  await eventually(async () => (await contextCallCount()) > contextsBefore, 5000);
  await eventually(() => !h.statuses.at(-1)?.includes("identity check unavailable"), 5000);
  assert.equal(hub.clients.length, 1, "same-identity discovery retains the connection");
  assert.equal(hub.clients[0].connected, true);
  await h.tools.get("mustr_send").execute("after-route-validation", { to: "peer", text: "Validated route", wake: false });
  assert.equal(hub.calls.filter(call => call.op === "send").length, 1, "the refreshed local snapshot authorizes a later operation");
});

for (const scenario of [
  {
    name: "pane id change from a model request",
    change: () => { process.env.HERDR_PANE_ID = "w1:p2"; },
    invoke: h => assert.rejects(h.tools.get("mustr_send").execute("changed-pane", { to: "peer", text: "Must not use the old pane identity", wake: false }), /identity became unverified|identity check unavailable/),
    forbidden: ["send"],
  },
  {
    name: "socket path change from UI configuration",
    change: async setContext => {
      const oldPath = process.env.HERDR_SOCKET_PATH;
      const replacement = `${oldPath}.replacement`;
      process.env.HERDR_SOCKET_PATH = replacement;
      await setContext({ routeIdentities: { [replacement]: { server_key: "replacement-server", socket: "/tmp/replacement-hub.sock" } } });
    },
    invoke: h => h.commands.get("mustr-todo").handler("TODO.md", h.context),
    forbidden: ["configure_todo"],
  },
  {
    name: "HERDR_ENV loss from UI mode",
    change: () => { delete process.env.HERDR_ENV; },
    invoke: h => h.commands.get("mustr").handler("todo on", h.context),
    forbidden: ["mode"],
  },
]) test(`local ${scenario.name} is caught before an old-identity request, before the next pump`, { timeout: 15000 }, async t => {
  const { h, hub, setContext, contextCallCount } = await setup(t);
  const oldConnection = hub.clients[0];
  const callsBefore = hub.calls.length;
  const contextsBefore = await contextCallCount();
  await scenario.change(setContext);
  await scenario.invoke(h);
  assert.equal(hub.calls.slice(callsBefore).some(call => scenario.forbidden.includes(call.op)), false, "the direct model/UI operation is blocked locally before hub dispatch");
  assert.equal(hub.calls.slice(callsBefore).some(call => call.op === "leave"), false, "a changed local route never sends Leave with the old pin");

  await eventually(async () => (await contextCallCount()) > contextsBefore, 5000);
  await eventually(() => !oldConnection.connected, 5000);
  assert.equal(hub.clients.length, 1, "changed or invalid local inputs stop participation without automatic rebind");
  assert.equal(hub.calls.some(call => call.op === "leave"), false);
  assert.equal(hub.calls.filter(call => scenario.forbidden.includes(call.op)).length, 0);
  assert.ok(hub.calls.every(call => call.op !== "join"), "the healthy old hub is never rejoined under a changed local route");
  if (scenario.name.includes("HERDR_ENV")) assert.ok(h.notifications.some(notice => /mustr context failed \(exit 1\)/.test(notice.message)), "a stable invalid context stops participation visibly");
});

test("typed server identity outage suppresses work, recovers on the same identity, and rejects route changes", { timeout: 22000 }, async t => {
  const persistentTask = task("active-through-identity-outage", "requester", "pi-session");
  const { h, hub, setContext, contextCallCount } = await setup(t, [persistentTask]);
  const callsBeforeOutage = hub.calls.length;
  const warnings = () => h.notifications.filter(notice => notice.level === "warning");
  await enterIdentityPending(h, hub, setContext);
  const syncsWhilePending = hub.syncs.length;
  assert.match(h.statuses.at(-1), /identity check unavailable · retrying/, "the expected retry remains persistently visible");
  assert.equal(warnings().length, 0, "typed server rejection does not create a warning popup");
  assert.ok(hub.calls.slice(callsBeforeOutage).some(call => call.op === "sync"), "the server's authenticated sync validation caused the typed rejection");
  hub.deliver(envelope({ id: "after-context-recovery", wake: false }));
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
  assert.equal(hub.calls.filter(call => call.op === "ack" || call.op === "report").length, 0, "no ACK or report accompanies the rejected sync");
  assert.equal(h.messages.length, 0);
  assert.equal(warnings().length, 0, "the typed server rejection remains status-only");
  const retainedConnection = hub.clients[0];
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "ordinary Pi tools remain available during coordination outage");
  await assert.rejects(h.tools.get("mustr_send").execute("held-send", { to: "peer", text: "Must not dispatch", wake: false }), /identity check unavailable/);
  await assert.rejects(h.tools.get("mustr_cancel_owner_tasks").execute("held-owner-cancel", {
    owner_id: "peer", scope: "all_unfinished", expected_guard: "0".repeat(24), text: "Must not dispatch",
  }, undefined), /identity check unavailable/);
  await assert.rejects(h.tools.get("mustr_report").execute("held-report", {
    task_id: "unverified-task", revision: 1, kind: "progress", text: "Must not dispatch",
  }, undefined), /identity check unavailable/);
  assert.equal(hub.calls.some(call => ["send", "cancel_owner_tasks", "report"].includes(call.op)), false);
  const requestsBeforeRoster = hub.calls.length;
  const syncsBeforeRoster = hub.syncs.length;
  await assert.rejects(h.tools.get("mustr_roster").execute("held-roster", {}, undefined), /identity check unavailable/);
  assert.equal(hub.calls.length, requestsBeforeRoster, "mustr_roster is rejected before any hub request");
  assert.equal(hub.syncs.length, syncsBeforeRoster, "mustr_roster cannot send sync during identity revalidation");
  assert.equal(hub.clients[0], retainedConnection, "the authenticated connection is retained");
  assert.equal(retainedConnection.connected, true);
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "ordinary host tools remain available after the rejected roster call");

  const failedAttemptCount = await contextCallCount();
  await eventually(async () => (await contextCallCount()) > failedAttemptCount, 4500);
  assert.equal(warnings().length, 0, "repeated local exit-75 retries do not produce changing warning popups");
  assert.match(h.statuses.at(-1), /identity check unavailable · retrying/, "the persistent retry status survives repeated local failures");
  assert.equal(hub.syncs.length, syncsWhilePending, "failed recovery discovery sends no unverified heartbeat");
  assert.equal(h.messages.length, 0, "inbox readiness during identity suspension is not received or acknowledged");
  assert.equal(hub.calls.some(call => call.op === "ack" && call.message_id === "after-context-recovery"), false);
  assert.equal(retainedConnection.connected, true);

  const refreshedTask = task("roster-after-context-recovery", "manager", "pi-session", "assigned", 1);
  hub.tasks.push(refreshedTask);
  await setContext({});
  await h.emit("turn_end");
  await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "after-context-recovery"));
  assert.equal(hub.clients.length, 1, "same-identity recovery keeps the original connection without Join");
  assert.equal(h.messages.length, 1);
  assert.doesNotMatch(h.statuses.at(-1), /identity check unavailable/);
  assert.equal(warnings().length, 0, "successful recovery clears persistent status without an outage toast");
  assert.equal(hub.tasks.find(item => item.id === persistentTask.id).state, "active", "identity recovery does not pause or auto-resume an assignment");
  assert.equal(hub.calls.some(call => ["report", "accept", "guide", "cancel", "cancel_owner_tasks", "reset_claim", "reset_status"].includes(call.op)), false, "no task/reset mutation accompanies the outage or recovery");
  const callsBeforeRecoveredRoster = hub.calls.length;
  const refreshedRoster = await h.tools.get("mustr_roster").execute("recovered-roster", {}, undefined);
  assert.ok(hub.calls.slice(callsBeforeRecoveredRoster).some(call => call.op === "sync"), "recovered roster request actually sends sync");
  assert.ok(refreshedRoster.details.tasks.some(item => item.id === refreshedTask.id), "roster refresh returns state added during the outage");
  assert.equal(hub.clients.length, 1, "roster refresh uses the retained connection without rejoining");

  await setContext({ exitCode: 75, stderr: "a different outage episode" });
  hub.rejectNext = { op: "sync", error: unavailableIdentityError() };
  const retriesBeforeSecondOutage = await contextCallCount();
  await h.emit("turn_end");
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  await eventually(async () => (await contextCallCount()) > retriesBeforeSecondOutage, 6000);
  assert.match(h.statuses.at(-1), /identity check unavailable · retrying/);
  assert.equal(warnings().length, 0, "a new typed server rejection and changed local stderr remain status-only");
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);

  await setContext({ identity: { space_id: "different-space" } });
  await h.emit("turn_end");
  await eventually(() => !hub.clients[0].connected);
  assert.ok(warnings().some(notice => notice.message.includes("space/server changed")), "fatal changed identity still uses an actionable warning");
  assert.equal(hub.clients.length, 1, "rediscovered changed identity ends participation without Leave or automatic rejoin");
});

for (const scenario of [
  { name: "mode", op: "mode", invoke: h => h.commands.get("mustr").handler("todo on", h.context) },
  { name: "TODO configuration", op: "configure_todo", invoke: h => h.commands.get("mustr-todo").handler("TODO.md", h.context) },
  { name: "PR configuration", op: "configure_pr", invoke: h => h.commands.get("mustr-pr").handler("https://example.invalid/repo/pull/1", h.context) },
]) test(`typed identity rejection from UI ${scenario.name} suspends unverified hub work without replay`, { timeout: 20000 }, async t => {
  const { h, hub, setContext, contextCallCount } = await setup(t);
  const contextsBefore = await contextCallCount();
  const warningsBefore = h.notifications.filter(notice => notice.level === "warning").length;
  await setContext({ exitCode: 75, stderr: "synthetic UI-triggered context outage" });
  hub.rejectNext = { op: scenario.op, error: unavailableIdentityError() };
  const callsBefore = hub.calls.length;
  await scenario.invoke(h);
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  assert.equal(hub.calls.slice(callsBefore).filter(call => call.op === scenario.op).length, 1, "the UI action reached the hub once and was rejected before execution");
  assert.equal(h.notifications.filter(notice => notice.level === "warning").length, warningsBefore, "typed UI rejection is status-only, not a popup");
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
  assert.ok(!h.notifications.some(notice => /for this space|attached, currently off/.test(notice.message)), "rejected settings do not report local success");

  const callsWhilePending = hub.calls.length;
  const syncsWhilePending = hub.syncs.length;
  await assert.rejects(h.tools.get("mustr_roster").execute("pending-roster", {}, undefined), /identity check unavailable/);
  await assert.rejects(h.tools.get("mustr_report").execute("pending-report", {
    task_id: "unverified-task", revision: 1, kind: "progress", text: "Must not dispatch",
  }, undefined), /identity check unavailable/);
  assert.equal(hub.calls.length, callsWhilePending, "roster and report are gated before dispatch");
  assert.equal(h.notifications.filter(notice => notice.level === "warning").length, warningsBefore, "repeated typed retries do not add warnings");
  await eventually(async () => (await contextCallCount()) > contextsBefore, 4500);
  assert.equal(hub.syncs.length, syncsWhilePending, "failed recovery discovery sends no heartbeat");
  assert.equal(hub.clients[0].connected, true);

  await setContext({});
  await h.emit("turn_end");
  await eventually(() => !h.statuses.at(-1)?.includes("identity check unavailable"));
  assert.equal(hub.calls.filter(call => call.op === scenario.op).length, 1, "a rejected UI action is never replayed after recovery");
  assert.equal(h.notifications.filter(notice => notice.level === "warning").length, warningsBefore, "same-identity recovery does not add a warning popup");
  assert.equal(hub.clients.length, 1, "recovery reuses the authenticated connection");
});

test("a delayed successful sync cannot authorize ACK, report, or inbox insertion after a concurrent UI identity rejection", { timeout: 22000 }, async t => {
  const parent = task("delayed-sync-report", "requester", "pi-session");
  const { h, hub, setContext, contextCallCount } = await setup(t, [parent]);
  const heldSync = holdNext(hub, "sync");
  const contextsBefore = await contextCallCount();
  const syncsBefore = hub.syncs.length;
  hub.deliver(envelope({ id: "delayed-sync-offer", wake: false }));
  await heldSync.started;

  await h.tools.get("mustr_report").execute("stage-during-sync", { task_id: parent.id, revision: 1, kind: "done", text: "Settled while the sync reply is delayed." }, undefined);
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await setContext({ exitCode: 75, stderr: "pending after delayed sync" });
  hub.rejectNext = { op: "mode", error: unavailableIdentityError() };
  await h.commands.get("mustr").handler("todo on", h.context);
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  assert.equal(hub.offered.has("delayed-sync-offer"), true, "the server marks the offer outstanding before the successful reply is released");
  hub.deliver(envelope({ id: "addressed-behind-stale-offer", wake: false }));
  assert.equal(hub.offered.size, 1, "the one-outstanding-offer rule suppresses later work until resolution");
  assert.equal(hub.inbox.some(item => item.id === "addressed-behind-stale-offer"), true, "later work remains queued behind the offer");
  assert.equal(hub.calls.filter(call => ["ack", "defer", "report"].includes(call.op)).length, 0);
  assert.equal(h.messages.length, 0, "no ACK/defer/report/insertion happens while identity is pending");

  heldSync.release();
  await eventually(async () => (await contextCallCount()) > contextsBefore, 6000);
  assert.equal(hub.syncs.length, syncsBefore + 1, "only the already-issued sync ran; the pending gate blocked follow-up syncs");
  assert.equal(hub.calls.filter(call => ["ack", "defer", "report"].includes(call.op)).length, 0);
  assert.equal(h.messages.length, 0, "an inbox offer in the delayed response is not inserted after another request suspended identity");
  assert.equal(hub.offered.size, 1, "the stale response was not acknowledged or released");

  const contextsAfterFailure = await contextCallCount();
  await setContext({});
  await h.emit("turn_end");
  await eventually(async () => (await contextCallCount()) > contextsAfterFailure, 6000);
  await eventually(() => reportCalls(hub, parent.id).length === 1, 6000);
  await eventually(() => h.messages.some(item => item.message.details?.id === "delayed-sync-offer")
    && h.messages.some(item => item.message.details?.id === "addressed-behind-stale-offer"), 6000);
  await eventually(() => ["delayed-sync-offer", "addressed-behind-stale-offer"].every(id => hub.calls.some(call => call.op === "ack" && call.message_id === id)), 6000);
  assert.equal(hub.calls.filter(call => call.op === "defer" && call.message_id === "delayed-sync-offer").length, 1, "the never-inserted stale offer is safely released once after matching discovery");
  assert.equal(hub.calls.filter(call => call.op === "ack" && call.message_id === "delayed-sync-offer").length, 1);
  assert.equal(hub.calls.filter(call => call.op === "ack" && call.message_id === "addressed-behind-stale-offer").length, 1);
  assert.equal(h.messages.filter(item => item.message.details?.id === "delayed-sync-offer").length, 1);
  assert.equal(h.messages.filter(item => item.message.details?.id === "addressed-behind-stale-offer").length, 1);
  assert.equal(hub.offered.size, 0);
  assert.equal(hub.inbox.length, 0);
  assert.equal(hub.calls.filter(call => call.op === "mode").length, 1, "the rejected UI mutation was not replayed");
  assert.equal(hub.calls.filter(call => call.op === "report").length, 1, "the safe staged report resumes only after same-identity discovery");
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
});

test("an accepted sync offer remains owned while a successful staged report reply is delayed", { timeout: 24000 }, async t => {
  const parent = task("report-await-offer", "requester", "pi-session");
  const { h, hub, setContext, contextCallCount } = await setup(t, [parent]);
  const heldSync = holdNext(hub, "sync");
  const contextsBefore = await contextCallCount();
  hub.deliver(envelope({ id: "offer-before-report", wake: false }));
  await Promise.race([heldSync.started, delay(6000).then(() => { throw new Error("accepted-offer sync was never held"); })]);
  const heldReport = holdNext(hub, "report");
  await h.tools.get("mustr_report").execute("stage-before-sync-reply", { task_id: parent.id, revision: 1, kind: "done", text: "The successful report will finish before inbox handling." }, undefined);
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  heldSync.release();
  await Promise.race([heldReport.started, delay(6000).then(() => { throw new Error("staged report reply was never held"); })]);
  assert.equal(hub.offered.has("offer-before-report"), true, "the accepted sync already owns one outstanding offer");
  assert.equal(reportCalls(hub, parent.id).length, 1, "the held report request already succeeded server-side");

  await setContext({ exitCode: 75, stderr: "identity hold during report reply" });
  hub.rejectNext = { op: "mode", error: unavailableIdentityError() };
  await h.commands.get("mustr").handler("todo on", h.context);
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  hub.deliver(envelope({ id: "work-behind-report-offer", wake: false }));
  heldReport.release();
  await eventually(async () => (await contextCallCount()) > contextsBefore, 6000);
  assert.equal(hub.offered.has("offer-before-report"), true, "common pump cleanup retains the uninserted offer after the report return");
  assert.equal(hub.calls.filter(call => ["ack", "defer"].includes(call.op)).length, 0, "no inbox action runs while identity is pending");
  assert.equal(h.messages.length, 0, "the report return cannot silently lose or insert the offered message");
  assert.equal(hub.inbox.some(item => item.id === "work-behind-report-offer"), true, "later work remains queued by the one-offer hub rule");
  assert.equal(reportCalls(hub, parent.id).length, 1, "the already-successful report is not replayed");

  const contextsAfterFailure = await contextCallCount();
  await setContext({});
  await h.emit("turn_end");
  await eventually(async () => (await contextCallCount()) > contextsAfterFailure, 6000);
  await eventually(() => ["offer-before-report", "work-behind-report-offer"].every(id => h.messages.some(item => item.message.details?.id === id)), 6000);
  await eventually(() => ["offer-before-report", "work-behind-report-offer"].every(id => hub.calls.some(call => call.op === "ack" && call.message_id === id)), 6000);
  assert.equal(hub.calls.filter(call => call.op === "defer" && call.message_id === "offer-before-report").length, 1, "the same-identity recovery resolves the retained uninserted offer once");
  assert.equal(h.messages.filter(item => item.message.details?.id === "offer-before-report").length, 1);
  assert.equal(h.messages.filter(item => item.message.details?.id === "work-behind-report-offer").length, 1);
  assert.equal(hub.calls.filter(call => call.op === "ack" && call.message_id === "offer-before-report").length, 1);
  assert.equal(hub.calls.filter(call => call.op === "ack" && call.message_id === "work-behind-report-offer").length, 1);
  assert.equal(reportCalls(hub, parent.id).length, 1);
  assert.equal(hub.offered.size, 0);
  assert.equal(hub.inbox.length, 0);
});

for (const resolution of ["changed identity", "socket disconnect"]) test(`a stale sync offer intent is not replayed after ${resolution}`, { timeout: 24000 }, async t => {
  const { h, hub, setContext, contextCallCount } = await setup(t);
  const oldClient = hub.clients[0];
  const heldSync = holdNext(hub, "sync");
  const contextsBefore = await contextCallCount();
  hub.deliver(envelope({ id: `stale-offer-${resolution.replaceAll(" ", "-")}`, wake: false }));
  await heldSync.started;
  await setContext({ exitCode: 75, stderr: "hold before offer recovery" });
  hub.rejectNext = { op: "mode", error: unavailableIdentityError() };
  await h.commands.get("mustr").handler("todo on", h.context);
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  heldSync.release();
  await eventually(async () => (await contextCallCount()) > contextsBefore, 6000);
  const staleId = `stale-offer-${resolution.replaceAll(" ", "-")}`;
  assert.equal(hub.offered.has(staleId), true, "the successful sync left one offer outstanding");
  assert.equal(hub.calls.some(call => ["ack", "defer"].includes(call.op) && call.message_id === staleId), false);
  assert.equal(h.messages.some(item => item.message.details?.id === staleId), false);

  if (resolution === "changed identity") {
    await setContext({ identity: { space_id: "different-space" } });
    await h.emit("turn_end");
    await eventually(() => !oldClient.connected, 8000);
    assert.equal(hub.clients.length, 1, "changed identity stops instead of reconnecting under a new pin");
    assert.equal(hub.calls.some(call => call.op === "defer" && call.message_id === staleId), false, "changed identity discards the old connection's deferral intent");
    assert.equal(hub.uncertain.has(staleId), true, "closing marks the outstanding offer uncertain for reconciliation");
  } else {
    oldClient.close();
    assert.equal(hub.offered.has(staleId), false);
    assert.equal(hub.uncertain.has(staleId), true, "socket close consumes the outstanding offer as uncertain");
    await setContext({});
    await h.emit("turn_end");
    await eventually(() => hub.clients.length === 2 && hub.clients[1].connected, 8000);
    assert.equal(hub.calls.some(call => call.op === "defer" && call.message_id === staleId), false, "a replacement connection never replays the old defer intent");
    assert.equal(h.messages.some(item => item.message.details?.id === staleId), false, "an uncertain old offer is never reinserted");
    hub.deliver(envelope({ id: "after-disconnect-recovery", wake: false }));
    await eventually(() => h.messages.some(item => item.message.details?.id === "after-disconnect-recovery"), 5000);
    await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "after-disconnect-recovery"), 5000);
  }
});

test("a typed UI rejection during ACK prevents the older pump from publishing its staged report", { timeout: 22000 }, async t => {
  const parent = task("delayed-ack-report", "requester", "pi-session");
  const { h, hub, setContext, contextCallCount } = await setup(t, [parent]);
  const heldAck = holdNext(hub, "ack");
  const contextsBefore = await contextCallCount();
  hub.deliver(envelope({ id: "insert-before-ack", wake: false }));
  await heldAck.started;
  await h.tools.get("mustr_report").execute("stage-during-ack", { task_id: parent.id, revision: 1, kind: "done", text: "Settled while ACK is delayed." }, undefined);
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await setContext({ exitCode: 75, stderr: "pending after delayed ACK" });
  hub.rejectNext = { op: "mode", error: unavailableIdentityError() };
  await h.commands.get("mustr").handler("todo on", h.context);
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));

  heldAck.release();
  await eventually(async () => (await contextCallCount()) > contextsBefore, 6000);
  assert.equal(hub.calls.filter(call => call.op === "ack" && call.message_id === "insert-before-ack").length, 1, "the already-issued successful ACK is recorded once");
  assert.equal(hub.calls.filter(call => call.op === "report").length, 0, "the older pump does not dispatch a staged report after the hold");
  assert.equal(h.messages.filter(item => item.message.details?.id === "insert-before-ack").length, 1, "there is no reinsertion");

  const contextsAfterFailure = await contextCallCount();
  await setContext({});
  await h.emit("turn_end");
  await eventually(async () => (await contextCallCount()) > contextsAfterFailure, 6000);
  await eventually(() => reportCalls(hub, parent.id).length === 1, 6000);
  assert.equal(hub.calls.filter(call => call.op === "mode").length, 1, "the rejected mode change is never replayed");
  assert.equal(hub.calls.filter(call => call.op === "ack" && call.message_id === "insert-before-ack").length, 1);
});

test("a released pending offer stops at the identity gate before a staged report or inbox insertion", { timeout: 24000 }, async t => {
  const parent = task("defer-report", "requester", "pi-session");
  const { h, hub, setContext, contextCallCount } = await setup(t, [parent]);
  hub.beforeOffer = () => h.editor("human draft holds the initial offer");
  hub.rejectNext = { op: "defer", error: unavailableIdentityError() };
  hub.deliver(envelope({ id: "pending-defer", wake: false }));
  await h.emit("turn_end");
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  assert.equal(hub.offered.size, 1, "the definitely rejected initial defer retains its safe intent");
  assert.equal(h.messages.length, 0);

  const contextsBeforeRetry = await contextCallCount();
  const heldDefer = holdNext(hub, "defer");
  await setContext({});
  await h.emit("turn_end");
  await heldDefer.started;
  await h.tools.get("mustr_report").execute("stage-during-defer", { task_id: parent.id, revision: 1, kind: "done", text: "Settled while the retained offer is being released." }, undefined);
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await setContext({ exitCode: 75, stderr: "pending after delayed defer" });
  hub.rejectNext = { op: "mode", error: unavailableIdentityError() };
  await h.commands.get("mustr").handler("todo on", h.context);
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));

  heldDefer.release();
  await eventually(async () => (await contextCallCount()) > contextsBeforeRetry, 7000);
  assert.equal(hub.calls.filter(call => call.op === "defer" && call.message_id === "pending-defer").length, 2, "the second defer succeeded exactly once after the first definite rejection");
  assert.equal(hub.calls.filter(call => ["ack", "report"].includes(call.op)).length, 0, "the pending branch performs no follow-up ACK or report");
  assert.equal(h.messages.length, 0, "no inbox insertion occurs until identity is revalidated");
  assert.equal(hub.inbox.some(item => item.id === "pending-defer"), true, "the successful defer safely returned the offer to the inbox");

  h.editor("");
  const contextsAfterFailure = await contextCallCount();
  await setContext({});
  await h.emit("turn_end");
  await eventually(async () => (await contextCallCount()) > contextsAfterFailure, 7000);
  await eventually(() => reportCalls(hub, parent.id).length === 1, 7000);
  await eventually(() => h.messages.some(item => item.message.details?.id === "pending-defer"), 7000);
  assert.equal(hub.calls.filter(call => call.op === "mode").length, 1, "the rejected mode change is never replayed");
  assert.equal(hub.calls.filter(call => call.op === "report").length, 1, "the staged report resumes once after same-identity discovery");
});

test("typed context outage blocks slash-command mode/configuration requests without replay", { timeout: 18000 }, async t => {
  const { h, hub, setContext } = await setup(t);
  await enterIdentityPending(h, hub, setContext);
  const callsBefore = hub.calls.length;
  const syncsBefore = hub.syncs.length;

  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "ordinary host tools remain available");
  for (const mode of ["todo on", "checkin on", "pr on"]) await h.commands.get("mustr").handler(mode, h.context);
  await h.commands.get("mustr-todo").handler("TODO.md", h.context);
  await h.commands.get("mustr-pr").handler("https://example.invalid/repo/pull/1", h.context);
  assert.equal(hub.calls.length, callsBefore, "mode/configuration commands send no hub request during identity revalidation");
  assert.equal(hub.syncs.length, syncsBefore);
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
  assert.ok(!h.notifications.some(notice => /for this space|attached, currently off/.test(notice.message)), "rejected settings do not report local success");

  await setContext({});
  await h.emit("turn_end");
  await eventually(() => !h.statuses.at(-1)?.includes("identity check unavailable"));
  assert.deepEqual(hub.calls.filter(call => ["mode", "configure_todo", "configure_pr"].includes(call.op)), [], "recovery does not replay settings rejected during the outage");
  assert.equal(hub.clients.length, 1, "same-identity recovery retains the existing connection");
  assert.equal(hub.clients[0].connected, true);
});

test("explicit leave after a local pane route change detaches without an old-pin Leave", { timeout: 10000 }, async t => {
  const { h, hub } = await setup(t);
  const retained = hub.clients[0];
  process.env.HERDR_PANE_ID = "w1:p2";
  const callsBefore = hub.calls.length;
  await h.commands.get("mustr").handler("leave", h.context);
  assert.equal(retained.connected, false);
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
  assert.equal(hub.calls.length, callsBefore, "changed local route prevents an old-pin Leave request");
  assert.equal(hub.calls.some(call => call.op === "leave"), false);
});

test("explicit leave during typed identity outage detaches locally without an unverified Leave", { timeout: 15000 }, async t => {
  const { h, hub, setContext } = await setup(t);
  await enterIdentityPending(h, hub, setContext);
  const retained = hub.clients[0];
  const callsBefore = hub.calls.length;

  await h.commands.get("mustr").handler("leave", h.context);
  assert.equal(retained.connected, false, "explicit leave detaches the local client");
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
  assert.equal(hub.calls.length, callsBefore, "leave sent no hub request using unverified identity");
  assert.equal(hub.calls.some(call => call.op === "leave"), false);
  assert.equal(hub.clients.length, 1, "leave does not rejoin");
});

test("role-switch cleanup skips unverified Leave and validates context before new admission", { timeout: 20000 }, async t => {
  const { h, hub, setContext, contextCallCount } = await setup(t);
  await enterIdentityPending(h, hub, setContext);
  const retained = hub.clients[0];
  const callsBeforeSwitch = hub.calls.length;
  const contextsBeforeSwitch = await contextCallCount();

  await h.commands.get("role").handler("implementor switched-agent", h.context);
  await eventually(() => !retained.connected);
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"], "role cleanup deactivates old participation locally");
  assert.equal(hub.calls.length, callsBeforeSwitch, "role cleanup sent no unverified hub Leave");
  await eventually(async () => (await contextCallCount()) > contextsBeforeSwitch);
  assert.equal(hub.clients.length, 1, "new role is not admitted while context still returns 75");
  assert.equal(hub.calls.length, callsBeforeSwitch);

  const contextsBeforeRecovery = await contextCallCount();
  await setContext({});
  await h.emit("turn_end");
  await eventually(() => hub.clients.length === 2 && hub.clients[1].connected);
  assert.ok(await contextCallCount() > contextsBeforeRecovery, "new admission performs a fresh context lookup");
  assert.equal(hub.clients[0].connected, false);
  assert.equal(hub.calls.some(call => call.op === "leave"), false, "role switch never sends the skipped unverified Leave later");
});

test("nontransient context child failures stop participation visibly without an automatic rejoin", { timeout: 30000 }, async t => {
  const cases = [
    { name: "generic-exit", configure: setContext => setContext({ exitCode: 1, stderr: "invalid fixture context configuration" }), warning: /mustr context failed \(exit 1\).*invalid fixture context configuration/ },
    // A signal has the same non-numeric exit shape as the parent's 6-second
    // execFile timeout; self-signal keeps this regression fast and deterministic.
    { name: "signal", configure: setContext => setContext({ signal: "SIGTERM" }), warning: /mustr context failed \(signal SIGTERM\)/ },
    { name: "malformed-json", configure: setContext => setContext({ output: "{malformed json\\n" }), warning: /mustr context returned invalid JSON/ },
    { name: "malformed-identity", configure: setContext => setContext({ output: JSON.stringify({ protocol: 2 }) }), warning: /mustr context returned malformed or unsupported identity data/ },
    { name: "enoent", configure: (_setContext, removeBinary) => removeBinary(), warning: /mustr context failed \(ENOENT\)/ },
  ];
  for (const scenario of cases) {
    const { h, hub, setContext, contextCallCount, removeBinary } = await setup(t);
    hub.rejectNext = { op: "sync", error: unavailableIdentityError() };
    if (scenario.name === "enoent") await scenario.configure(setContext, removeBinary);
    else await scenario.configure(setContext);
    await h.emit("turn_end");
    await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
    const syncsWhilePending = hub.syncs.length;
    await eventually(() => !hub.clients[0].connected);
    const notice = h.notifications.find(item => scenario.warning.test(item.message));
    assert.ok(notice, `${scenario.name}: actionable context failure is shown`);
    assert.equal(notice.level, "warning", `${scenario.name}: nontransient context failure remains prominent`);
    assert.deepEqual(h.active(), ["read", "bash", "policy-tool"], `${scenario.name}: Mustr participation/tools are stopped`);
    assert.equal(hub.syncs.length, syncsWhilePending, `${scenario.name}: no heartbeat follows unverified context`);
    assert.equal(hub.clients.length, 1, `${scenario.name}: no automatic role rejoin`);
    if (scenario.name !== "enoent") {
      const callsAtStop = await contextCallCount();
      await setContext({}); // A later successful lookup must not silently restore participation.
      await h.emit("turn_end");
      await delay(100);
      assert.equal(await contextCallCount(), callsAtStop, `${scenario.name}: stopped role is not polled/rejoined after context recovers`);
      assert.equal(hub.clients.length, 1, `${scenario.name}: explicit /role is still required after recovery`);
    }
    assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "Pi owns ordinary tool use after participation stops");
  }
});

for (const scenario of [
  { name: "pump", op: "sync", invoke: async h => h.emit("turn_end") },
  { name: "model request", op: "send", invoke: async h => assert.rejects(
    h.tools.get("mustr_send").execute("retired-send", { to: "peer", text: "Do not replay", wake: false }),
    error => error.code === "connection_retired",
  ) },
  { name: "UI request", op: "mode", invoke: async h => h.commands.get("mustr").handler("todo on", h.context) },
]) test(`authoritative connection retirement stops the current Pi episode from the ${scenario.name}`, { timeout: 15000 }, async t => {
  const { h, hub } = await setup(t);
  const before = hub.calls.length;
  hub.rejectNext = { op: scenario.op, error: connectionRetiredError() };
  await scenario.invoke(h);
  await eventually(() => h.notifications.some(item => /retired or superseded this connection/.test(item.message)));
  await eventually(() => !hub.clients[0].connected);
  const retiredCalls = hub.calls.length;
  assert.equal(h.notifications.filter(item => /retired or superseded this connection/.test(item.message)).length, 1, "one actionable notice is shown for the current episode");
  assert.equal(hub.calls.slice(before).filter(call => call.op === scenario.op).length, 1, "the rejected request is not replayed");
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"], "only Mustr participation tools are disabled");
  assert.equal(hub.clients.length, 1, "a retired connection is not automatically rejoined");
  assert.equal((await h.emit("tool_call", { toolName: "bash" }))[0], undefined, "ordinary Pi tools remain available");
  await assert.rejects(h.tools.get("mustr_send").execute("after-retirement", { to: "peer", text: "No dispatch", wake: false }), /mustr is disconnected/);
  await delay(1100);
  assert.equal(hub.calls.length, retiredCalls, "no heartbeat, follow-up, or mutation is sent after retirement");
  assert.equal(hub.clients.length, 1);
});

test("a staged report is discarded on retirement and is not carried into an explicit new role", { timeout: 18000 }, async t => {
  const parent = task("retired-staged-report", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Settled before the connection retired." });
  await h.emit("agent_before_settle", { outcome: "completed" });
  hub.rejectNext = { op: "sync", error: connectionRetiredError() };
  await h.settle(false);
  await eventually(() => h.notifications.some(item => /retired or superseded this connection/.test(item.message)));
  assert.equal(reportCalls(hub, parent.id).length, 0, "the staged report never executes on the retired connection");
  assert.equal(hub.clients[0].connected, false);
  await h.commands.get("role").handler("implementor fixture-agent", h.context);
  await eventually(() => hub.clients.length === 2 && hub.clients[1].connected);
  await delay(100);
  assert.equal(reportCalls(hub, parent.id).length, 0, "the explicit rejoin does not auto-resume or replay the old task report");
  assert.equal(h.notifications.filter(item => /retired or superseded this connection/.test(item.message)).length, 1);
});

test("an offer becomes uncertain at retirement and is not carried into an explicit new role", { timeout: 18000 }, async t => {
  const { h, hub } = await setup(t);
  const id = "retired-offer";
  hub.rejectNext = { op: "ack", error: connectionRetiredError() };
  hub.deliver(envelope({ id, wake: false }));
  await h.emit("turn_end");
  await eventually(() => h.notifications.some(item => /retired or superseded this connection/.test(item.message)));
  assert.equal(hub.uncertain.has(id), true, "closing the retired socket leaves the unacknowledged offer uncertain");
  assert.equal(h.messages.filter(item => item.message.details?.id === id).length, 1, "the offer was inserted only once");
  assert.equal(hub.calls.filter(call => call.op === "ack" && call.message_id === id).length, 1, "the rejected receipt is not retried");
  await h.commands.get("role").handler("implementor fixture-agent", h.context);
  await eventually(() => hub.clients.length === 2 && hub.clients[1].connected);
  await delay(100);
  assert.equal(h.messages.filter(item => item.message.details?.id === id).length, 1, "the uncertain offer is not reinserted after explicit Join");
  assert.equal(hub.calls.filter(call => call.op === "ack" && call.message_id === id).length, 1);
  assert.equal(h.notifications.filter(item => /retired or superseded this connection/.test(item.message)).length, 1);
});

test("retirement during a reset hold preserves its fence and does not clear private reset state", { timeout: 15000 }, async t => {
  const { h, hub } = await setup(t);
  hub.resetSummary = { state: "prepared" };
  hub.resetControl = {
    plan_id: "fixture-plan", token: "private-fixture-token", space_id: "w1", old_agent_id: "pi-session",
    name: "fixture-agent", role: "implementor", pane_id: "w1:p1", state: "claimed",
  };
  hub.retireAfterResetControl = true;
  await h.emit("turn_end");
  await eventually(() => hub.calls.some(call => call.op === "sync"));
  await eventually(() => h.notifications.some(item => /Inspect and reconcile reset status/.test(item.message)));
  const retiredCalls = hub.calls.length;
  assert.equal(hub.clients[0].connected, false);
  assert.equal(h.notifications.filter(item => /Inspect and reconcile reset status/.test(item.message)).length, 1);
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
  await h.commands.get("role").handler("implementor fixture-agent", h.context);
  assert.equal(hub.clients.length, 1, "the retained reset fence still blocks role selection");
  assert.ok(h.notifications.some(item => /Fresh start is suspended or unconfirmed/.test(item.message)), "reset recovery remains explicitly gated");
  await delay(1100);
  assert.equal(hub.calls.length, retiredCalls, "retired reset state is not probed or retried");
  assert.equal(hub.calls.some(call => call.op === "reset_claim" || call.op === "reset_status"), false, "no reset mutation follows retirement");
});

test("topology mismatch retirement from reset_status preserves the hard reset fence", { timeout: 20000 }, async t => {
  const { h, hub, setContext } = await setup(t);
  hub.resetSummary = { state: "prepared" };
  hub.resetControl = {
    plan_id: "fixture-plan", token: "private-fixture-token", space_id: "w1", old_agent_id: "pi-session",
    name: "fixture-agent", role: "implementor", pane_id: "w1:p1", state: "claimed",
  };
  await h.emit("turn_end");
  await eventually(() => h.notifications.some(item => /Inspect the hub reset status/.test(item.message)));
  const oldClient = hub.clients[0];
  const callsBeforeMismatch = hub.calls.length;

  hub.rejectNext = { op: "reset_status", error: connectionRetiredError() };
  process.env.HERDR_PANE_ID = "w1:p2";
  await setContext({ identity: { space_id: "different-space", pane_id: "w1:p2" } });
  await h.emit("turn_end");
  await eventually(() => h.notifications.filter(item => /retired or superseded this connection/.test(item.message)).length === 1);
  await eventually(() => !oldClient.connected);

  assert.deepEqual(hub.calls.slice(callsBeforeMismatch).map(call => call.op), ["reset_status"], "only the typed-rejected reset status request follows topology discovery");
  assert.equal(h.notifications.filter(item => /retired or superseded this connection/.test(item.message)).length, 1);
  assert.equal(h.notifications.some(item => /space\/server changed/.test(item.message)), false, "the stale topology path cannot publish a second notice");
  assert.equal(h.notifications.some(item => item.message.includes("private-fixture-token")), false, "private reset claims never reach notifications");
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"], "Pi's active tool set is unchanged while the event gate enforces reset safety");
  const blockedTool = (await h.emit("tool_call", { toolName: "bash" }))[0];
  assert.equal(blockedTool?.block, true, "the retained reset fence blocks ordinary tools after the role is cleared");
  assert.equal(blockedTool?.terminate, true, "the hard reset fence terminates the attempted tool call");
  assert.match(blockedTool?.reason ?? "", /Fresh start is suspended or unconfirmed/);
  await h.commands.get("role").handler("implementor replacement-agent", h.context);
  await h.commands.get("mustr-reset-internal").handler("not-a-real-claim", h.context);
  assert.equal(hub.clients.length, 1, "the retained reset fence prevents role replacement");
  assert.equal(hub.calls.some(call => call.op === "reset_claim" || call.op === "leave"), false, "retirement cannot claim or send an unverified Leave");
  const afterRetirement = hub.calls.length;
  await delay(1100);
  assert.equal(hub.calls.length, afterRetirement, "no follow-up hub or reset request is sent");
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool"]);
});

test("late reset_status retirement cannot clean up a newer explicit Pi episode", { timeout: 20000 }, async t => {
  const { h, hub, setContext } = await setup(t);
  hub.resetSummary = { state: "prepared" };
  hub.resetControl = {
    plan_id: "fixture-plan", token: "private-fixture-token", space_id: "w1", old_agent_id: "pi-session",
    name: "fixture-agent", role: "implementor", pane_id: "w1:p1", state: "claimed",
  };
  await h.emit("turn_end");
  await eventually(() => h.notifications.some(item => /Inspect the hub reset status/.test(item.message)));

  const oldClient = hub.clients[0];
  const heldStatus = holdNext(hub, "reset_status");
  process.env.HERDR_PANE_ID = "w1:p2";
  await setContext({ identity: { space_id: "different-space", pane_id: "w1:p2" } });
  await h.emit("turn_end");
  await heldStatus.started;
  assert.equal(oldClient.connected, true, "the old helper is paused on its reset-status reply");

  await h.commands.get("mustr").handler("leave", h.context);
  await eventually(() => !oldClient.connected);
  assert.equal(hub.calls.some(call => call.op === "leave"), false, "explicit local leave does not send an unverified reset-session Leave");
  hub.resetControl = undefined;
  hub.resetSummary = undefined;
  await h.commands.get("role").handler("implementor replacement-agent", h.context);
  assert.equal(hub.clients.length, 1, "the old serialized pump has not yet created the queued replacement client");

  heldStatus.reject(connectionRetiredError());
  await eventually(() => hub.heldRequests.size === 0);
  await eventually(() => hub.clients.length === 2 && hub.clients[1].connected);
  const newClient = hub.clients[1];
  assert.equal(newClient.connected, true, "the newer role intent survives the delayed old reset reply");
  assert.ok(h.active().includes("mustr_roster"), "stale cleanup cannot clear the newer role/tools");
  assert.equal(hub.clients.length, 2, "stale reset handling neither replaces nor reopens a connection");
  assert.equal(hub.calls.some(call => call.op === "leave"), false, "the old reset caller cannot Leave the new episode");
  assert.equal(h.notifications.filter(item => /retired or superseded this connection/.test(item.message)).length, 0, "the old error cannot notify the new episode");
});

test("a stale retired response closes only its old client after a newer explicit role joins", { timeout: 15000 }, async t => {
  const { h, hub } = await setup(t);
  const held = holdNext(hub, "sync");
  await h.emit("turn_end");
  await held.started;
  await h.commands.get("role").handler("implementor replacement-agent", h.context);
  assert.equal(hub.clients[0].connected, false, "the explicit role switch detached the old client");
  held.reject(connectionRetiredError());
  await eventually(() => hub.clients.length === 2 && hub.clients[1].connected);
  assert.equal(hub.clients[0].connected, false);
  assert.equal(hub.clients[1].connected, true, "the stale old-generation error cannot close the replacement client");
  assert.deepEqual(h.active(), ["read", "bash", "policy-tool", "mustr_roster", "mustr_send", "mustr_assign", "mustr_report", "mustr_guide", "mustr_accept", "mustr_cancel", "mustr_cancel_owner_tasks", "mustr_pause"]);
  assert.equal(h.notifications.filter(item => /retired or superseded this connection/.test(item.message)).length, 0, "stale work cannot publish a notice into the new episode");
});

for (const op of ["sync", "ack", "defer", "report"]) test(`definite preexecution ${op} rejection retains only safe pending intent on the same connection`, { timeout: 15000 }, async t => {
  const parent = task(`availability-${op}`, "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  hub.rejectNext = { op };
  if (op === "report") {
    await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Already completed fixture work." });
    await h.emit("agent_before_settle", { outcome: "completed" });
    await h.settle(false);
  } else {
    if (op === "defer") hub.beforeOffer = () => h.editor("human draft won the offer race");
    hub.deliver(envelope({ id: `availability-${op}`, wake: false }));
    await h.emit("turn_end");
  }
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
  assert.equal(reportCalls(hub, parent.id).length, 0, "rejected work was not executed");
  if (op === "defer") {
    assert.equal(h.messages.length, 0);
    assert.equal(hub.offered.size, 1);
    h.editor("");
  }
  if (op === "ack") assert.equal(h.messages.length, 1, "insertion precedes its held receipt");
  if (op === "report") await eventually(() => reportCalls(hub, parent.id).length === 1);
  else await eventually(() => hub.calls.filter(call => call.op === "ack" && call.message_id === `availability-${op}`).length === (op === "ack" ? 2 : 1));
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.offered.size, 0);
  if (op !== "report") assert.equal(h.messages.length, 1, "no reinsertion during receipt/defer recovery");
  else assert.equal(hub.calls.filter(call => call.op === "report").length, 2, "retry only the explicitly unexecuted report");
});

test("user input does not clear a report retained after definite nonexecution", { timeout: 15000 }, async t => {
  const parent = task("retained-report", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  hub.rejectNext = { op: "report" };
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Settled claim retained after definite nonexecution." });
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  await h.commands.get("mustr").handler("resume", h.context);
  assert.match(h.statuses.at(-1), /identity check unavailable/);
  assert.equal(reportCalls(hub, parent.id).length, 0, "resume does not clear identity state or prematurely publish a retained report");
  await acceptUser(h, "Human input does not resume or erase the retained report");
  hub.deliver(envelope({ id: "after-retained-report", wake: false }));
  await eventually(() => hub.calls.some(call => call.op === "report" && call.task_id === parent.id));
  await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "after-retained-report"));
  assert.equal(reportCalls(hub, parent.id).length, 1, "the previously settled report publishes once after identity recovery");
  assert.equal(hub.tasks.find(item => item.id === parent.id).state, "review");
});

test("an uncertain staged-report reply is never replayed on reconnect", { timeout: 15000 }, async t => {
  const parent = task("uncertain-report", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  hub.rejectNext = { op: "report", error: new Error("Hub acknowledgement timed out; delivery is uncertain") };
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Uncertain claim, never replay." });
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => hub.clients.length === 2 && hub.clients[1].connected);
  assert.ok(h.notifications.some(notice => notice.level === "warning" && /Hub acknowledgement timed out; delivery is uncertain/.test(notice.message)), "uncertain transport outcomes remain warning-visible");
  await h.tools.get("mustr_roster").execute("reconciled", {});
  assert.equal(hub.calls.filter(call => call.op === "report").length, 1);
  assert.equal(hub.clients[0].connected, false);
});

test("manual inbox pause does not strand a human-directed terminal report", { timeout: 15000 }, async t => {
  const parent = task("manual-report", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  await h.commands.get("mustr").handler("pause", h.context);
  await acceptUser(h, "Finish this task while keeping my inbox paused");
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Human-directed work completed." });
  await h.emit("agent_before_settle", { outcome: "completed" });
  await h.settle(false);
  await eventually(() => reportCalls(hub, parent.id).length === 1);
  assert.match(h.statuses.at(-1), /manual pause/);
  assert.equal(hub.syncs.at(-1).receive, false);
});
