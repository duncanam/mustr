import test from "node:test";
import assert from "node:assert/strict";
import { chmod, writeFile, rename } from "node:fs/promises";
import { join } from "node:path";
import { fixture, eventually, delay } from "../../tests/support.mjs";
import { HubClient, HubRemoteError } from "../transport.mjs";
import { harness } from "./adapter-harness.mjs";

const role = { name: "implementor", description: "isolated fixture", prompt: "No external work." };
const task = (id, requester, owner, state = "active", revision = 1) => ({ id, requester, owner, state, revision, title: id });
const userStart = text => ({ message: { role: "user", content: [{ type: "text", text }] } });
const envelope = fields => ({ id: "message", from: "peer", to: "pi-session", task_id: null, revision: null, kind: "request", text: "Authorized fixture input.", wake: true, ...fields });

function mockHub(seedTasks = []) {
  const state = { tasks: structuredClone(seedTasks), inbox: [], calls: [], syncs: [], reports: [], clients: [], offered: new Map(), nextTask: 1 };
  const original = { connect: HubClient.prototype.connect, request: HubClient.prototype.request, close: HubClient.prototype.close };
  HubClient.prototype.connect = async function (_path, hello) {
    this.__mustrAgentId = hello.agent_id;
    this.connected = true;
    state.clients.push(this);
    return { delivery_policy: 1, inbox_notifications: 1, reset_protocol: 1 };
  };
  HubClient.prototype.request = async function (command) {
    const agentId = this.__mustrAgentId;
    state.calls.push({ agentId, ...command });
    if (state.rejectNext?.op === command.op) {
      const error = state.rejectNext.error ?? new HubRemoteError("Fixture identity lookup unavailable; not executed", "identity_unavailable");
      state.rejectNext = undefined;
      throw error;
    }
    if (command.op === "reset_capability") return { reset_protocol: 1 };
    if (command.op === "subscribe" || command.op === "leave" || command.op === "mode") return { ok: true };
    if (command.op === "sync") {
      state.syncs.push({ agentId, ...command });
      const result = {
        members: [{ id: agentId, name: "fixture", role: "implementor", activity: command.activity }],
        tasks: structuredClone(state.tasks), automation: {}, insertion_policy: "idle_only",
      };
      if (command.receive) {
        const index = state.inbox.findIndex(message => message.to === agentId);
        if (index >= 0) {
          const [message] = state.inbox.splice(index, 1);
          state.offered.set(message.id, message);
          result.inbox = structuredClone(message);
          const beforeOffer = state.beforeOffer; state.beforeOffer = undefined;
          beforeOffer?.();
        }
      }
      return result;
    }
    if (command.op === "ack") { state.offered.delete(command.message_id); return { acked: true }; }
    if (command.op === "defer") {
      const message = state.offered.get(command.message_id);
      if (message) state.inbox.unshift(message);
      state.offered.delete(command.message_id);
      return { deferred: true };
    }
    if (command.op === "report") {
      const current = state.tasks.find(item => item.id === command.task_id);
      if (!current || current.owner !== agentId || current.revision !== command.revision) throw new Error("invalid report owner/revision");
      state.reports.push({ agentId, ...command });
      if (command.kind === "done") current.state = "review";
      else if (command.kind === "blocked" || command.kind === "waiting") current.state = command.kind;
      return { reported: true, task: structuredClone(current) };
    }
    if (command.op === "assign") {
      const id = `assigned-${state.nextTask++}`;
      state.tasks.push(task(id, agentId, command.to, "assigned", 1));
      return { task_id: id };
    }
    if (["accept", "guide", "cancel"].includes(command.op)) {
      const current = state.tasks.find(item => item.id === command.task_id);
      if (!current || current.requester !== agentId || current.revision !== command.revision) throw new Error("invalid requester/revision");
      if (command.op === "accept") current.state = "accepted";
      else if (command.op === "guide") { current.revision++; current.state = "assigned"; }
      else current.state = "cancelled";
      return { task: structuredClone(current) };
    }
    if (command.op === "send") return { sent: true };
    throw new Error(`Unexpected fixture Hub operation: ${command.op}`);
  };
  HubClient.prototype.close = function () {
    if (!this.connected) return;
    this.connected = false;
    this.onClose?.();
  };
  state.deliver = message => {
    state.inbox.push(structuredClone(message));
    for (const client of state.clients) if (client.connected && client.__mustrAgentId === message.to) client.onInboxReady?.();
  };
  state.restore = () => {
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
  await writeFile(contextControl, "{}");
  await writeFile(fakeBinary, `#!/usr/bin/env node\nimport { readFileSync } from 'node:fs';\nconst args = process.argv.slice(2);\nif (args.includes("roles")) console.log(${JSON.stringify(JSON.stringify([role]))});\nelse if (args.includes("context")) {\nconst control = JSON.parse(readFileSync(${JSON.stringify(contextControl)}, 'utf8'));\nif (control.fail) { console.error('Fixture herdr unavailable'); process.exitCode = 1; }\nelse console.log(JSON.stringify({...${JSON.stringify(identity)}, ...control.identity}));\n}\nelse process.exitCode = 2;\n`);
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
  return { f, h, hub, setContext };
}

function reportCalls(hub, taskId) { return hub.reports.filter(call => call.task_id === taskId); }
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


test("decision and interruption holds require accepted non-extension input, including transformed input", { timeout: 18000 }, async t => {
  const { h, hub } = await setup(t);
  const pause = await h.tools.get("mustr_pause").execute("pause", { reason: "A human decision is genuinely required" }, undefined);
  assert.equal(pause.terminate, true);
  await h.settle(false); // normal terminate:true settlement is not an abort
  assert.match(h.statuses.at(-1), /awaiting human decision/);
  hub.deliver(envelope({ id: "decision-message" }));

  await acceptUser(h, "Extension prompt", "extension");
  await delay(100);
  assert.equal(h.messages.length, 0, "extension-only acceptance does not release the hold");
  await h.emit("input", { source: "interactive", text: "Original prompt before transformation" });
  await h.emit("message_start", userStart("Expanded skill prompt"));
  await eventually(() => h.messages.some(item => item.message.details?.id === "decision-message"));
  assert.doesNotMatch(h.statuses.at(-1), /awaiting human decision/);

  await h.settle(true); // real session abort creates an interruption hold
  assert.match(h.statuses.at(-1), /interrupted/);
  hub.deliver(envelope({ id: "abort-message" }));
  await acceptUser(h, "Extension after abort", "extension");
  await delay(100);
  assert.equal(h.messages.length, 1);
  await acceptUser(h, "Accepted non-extension recovery after abort", "rpc");
  await eventually(() => h.messages.some(item => item.message.details?.id === "abort-message"));
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


test("exhaustion/cancel ambiguity is labelled fail-closed until accepted human input or resume", { timeout: 15000 }, async t => {
  const { h, hub } = await setup(t);
  h.idle(false);
  await h.emit("agent_start");
  await h.emit("message_end", { message: { role: "assistant", stopReason: "error" } });
  await h.settle(false); // same public extension events for exhausted retry and retry-Escape
  assert.match(h.statuses.at(-1), /unresolved error.*retry outcome ambiguous/);
  hub.deliver(envelope({ id: "held-assignment", kind: "assignment", from: "manager", task_id: "new-task", revision: 1 }));
  await eventually(() => hub.syncs.some(sync => sync.receive === false && sync.activity === "paused"));
  await delay(100);
  assert.equal(h.messages.length, 0, "no peer message starts another turn while retry outcome is ambiguous");
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0].block, true);
  await h.tools.get("mustr_roster").execute("status-only", {}, undefined);
  assert.equal(hub.syncs.at(-1).receive, false, "roster status cannot bypass the no-inbox fault hold");
  assert.equal(h.messages.length, 0);

  await acceptUser(h, "Human explicitly restarts after ambiguous provider outcome");
  await eventually(() => h.messages.some(item => item.message.details?.id === "held-assignment"));
  assert.doesNotMatch(h.statuses.at(-1), /unresolved error/);
});


test("accepted input clears a report gate; extension or handled input cannot discard a staged report", { timeout: 15000 }, async t => {
  const parent = task("parent", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  const staged = await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Fixture parent is done." }, undefined);
  assert.equal(staged.terminate, true);
  await acceptUser(h, "Extension cannot clear a terminal report", "extension");
  await h.settle(false);
  await eventually(() => reportCalls(hub, parent.id).length === 1);
  assert.equal(hub.tasks.find(item => item.id === parent.id).state, "review");
  await acceptUser(h, "Human authorizes the next fixture task");

  const second = task("second", "requester", "pi-session");
  hub.tasks.push(second);
  await h.tools.get("mustr_roster").execute("refresh", {}, undefined);
  await h.tools.get("mustr_report").execute("stage", { task_id: second.id, revision: 1, kind: "done", text: "Staged but not yet sent." }, undefined);
  await h.emit("input", { source: "interactive", text: "Handled and rejected" });
  await h.settle(false);
  await delay(100);
  assert.equal(reportCalls(hub, second.id).length, 0, "handled input leaves the staged report and report latch intact");
  await acceptUser(h, "A later message cannot silently discard an unaccepted older slot");
  await delay(100);
  assert.equal(reportCalls(hub, second.id).length, 0, "unmatched preflight remains fenced even after a later input");
  await h.commands.get("mustr").handler("resume", h.context);
  await eventually(() => reportCalls(hub, second.id).length === 1);
  await acceptUser(h, "Accepted new work after explicit preflight reconciliation");

  const third = task("third", "requester", "pi-session");
  hub.tasks.push(third);
  await h.tools.get("mustr_report").execute("stage-third", { task_id: third.id, revision: 1, kind: "done", text: "Superseded only at accepted human boundary." }, undefined);
  await acceptUser(h, "Fresh accepted human work supersedes the still-staged report");
  await h.settle(false);
  await delay(100);
  assert.equal(reportCalls(hub, third.id).length, 0, "accepted human input cancels a staged report without publishing/replaying it");
  assert.doesNotMatch(h.statuses.at(-1), /awaiting guidance/);
});


test("a genuine abort cancels a staged terminal report before publication", { timeout: 12000 }, async t => {
  const parent = task("aborted-parent", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Must not publish after abort." }, undefined);
  await h.settle(true);
  await delay(100);
  assert.equal(reportCalls(hub, parent.id).length, 0);
  assert.match(h.statuses.at(-1), /interrupted/);
  await acceptUser(h, "Human resolves the interruption");
  await delay(100);
  assert.equal(reportCalls(hub, parent.id).length, 0, "aborted report is not replayed after recovery");
});


test("child review and taskless manager check-in are scoped; new assignment reopens work but not an old task report", { timeout: 20000 }, async t => {
  const parent = task("parent", "requester", "pi-session");
  const child = task("child", "pi-session", "child-agent", "review", 3);
  const waiting = task("waiting", "pi-session", "waiter", "waiting", 4);
  const { h, hub } = await setup(t, [parent, child, waiting]);
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Parent work is done." }, undefined);
  await h.settle(false);
  await eventually(() => reportCalls(hub, parent.id).length === 1);

  hub.deliver(envelope({ id: "child-done", from: "child-agent", task_id: child.id, revision: child.revision, kind: "done", text: "Child completed; inspect evidence.", wake: true }));
  await eventually(() => h.entries.some(entry => entry.type === "custom_message" && entry.customType === "mustr" && entry.details?.id === "child-done"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "valid child report permits ordinary review tools");
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
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0].block, true, "nonwaking Waiting does not grant a review scope");
  hub.deliver(envelope({ id: "waiting-wake-drift", from: "waiter", task_id: waiting.id, revision: waiting.revision, kind: "blocked", text: "A waiting state must not gain a waking scope.", wake: true }));
  await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "waiting-wake-drift"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0].block, true, "receipt of an invalid review envelope grants no work scope");
  await h.settle(false);

  hub.deliver(envelope({ id: "manager-checkin", kind: "checkin", from: null, to: "pi-session", task_id: null, revision: null, text: "Reconcile approved coordination goals.", wake: true }));
  await eventually(() => h.entries.some(entry => entry.type === "custom_message" && entry.customType === "mustr" && entry.details?.id === "manager-checkin"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "taskless authoritative check-in permits local reconciliation");
  await h.tools.get("mustr_assign").execute("delegate", { to: "child-agent", title: "Follow-up", text: "Verify the fixture follow-up." }, undefined);
  await h.tools.get("mustr_report").execute("parent-status", { task_id: parent.id, revision: 1, kind: "status", text: "Parent remains under review." }, undefined);
  const afterStatus = hub.reports.length;
  await assert.rejects(h.tools.get("mustr_report").execute("parent-progress-again", { task_id: parent.id, revision: 1, kind: "progress", text: "Still forbidden." }, undefined), /prior task report|already has a terminal report/);
  assert.equal(hub.reports.length, afterStatus, "status is allowed but fresh work claims on the stopped task are not");
  await h.settle(false);
  assert.notEqual((await h.emit("tool_call", { toolName: "read" }))[0]?.block, undefined, "manager check-in scope expires at settlement");

  const workerCheckin = envelope({ id: "worker-checkin", kind: "checkin", from: "child-agent", task_id: child.id, revision: child.revision, wake: true });
  hub.deliver(workerCheckin);
  await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "worker-checkin"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0].block, true, "task-linked worker check-in drains without bypassing the report latch");
  await h.settle(false);

  const next = task("next", "manager", "pi-session", "assigned", 1);
  hub.tasks.push(next);
  h.deferInsertion(true);
  hub.deliver(envelope({ id: "new-assignment", kind: "assignment", from: "manager", task_id: next.id, revision: next.revision, wake: true, text: "Fresh authorized task." }));
  await eventually(() => h.messages.some(item => item.message.details?.id === "new-assignment"));
  await h.emit("message_start", userStart("Unrelated start before branch insertion"));
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0].block, true, "an offered/attempted message cannot release the old report latch");
  const assignmentIndex = h.messages.findIndex(item => item.message.details?.id === "new-assignment");
  await h.insert(assignmentIndex);
  h.deferInsertion(false);
  await h.emit("tool_call", { toolName: "read" }); // Commits the permit on current-branch insertion evidence.
  assert.equal((await h.emit("tool_call", { toolName: "read" }))[0], undefined, "fresh assignment releases the process work latch");
  await assert.rejects(h.tools.get("mustr_report").execute("old-parent", { task_id: parent.id, revision: 1, kind: "progress", text: "Still fenced." }, undefined), /terminal report/);
  await h.tools.get("mustr_report").execute("new-done", { task_id: next.id, revision: next.revision, kind: "done", text: "New task completed once." }, undefined);
  await h.settle(false);
  await eventually(() => reportCalls(hub, next.id).length === 1);
  assert.equal(reportCalls(hub, parent.id).filter(call => call.kind !== "status").length, 1);
});

test("context lookup failure suspends delivery without disconnecting; recovery still checks identity", { timeout: 15000 }, async t => {
  const { h, hub, setContext } = await setup(t);
  await setContext({ fail: true });
  hub.deliver(envelope({ id: "after-context-recovery", wake: false }));
  await h.emit("turn_end");
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  assert.equal(hub.clients.length, 1);
  assert.equal(hub.clients[0].connected, true);
  assert.equal(h.messages.length, 0);
  await assert.rejects(h.tools.get("mustr_send").execute("held-send", { to: "peer", text: "Must not dispatch", wake: false }), /identity check unavailable/);
  assert.equal(hub.calls.some(call => call.op === "send"), false);
  await setContext({});
  await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "after-context-recovery"));
  assert.equal(hub.clients.length, 1, "lookup recovery must not rejoin");
  assert.equal(h.messages.length, 1);

  await setContext({ identity: { space_id: "different-space" } });
  await h.emit("turn_end");
  await eventually(() => !hub.clients[0].connected);
  assert.ok(h.notifications.some(notice => notice.message.includes("space/server changed")));
  assert.equal(hub.clients.length, 1, "changed identity is never silently rebound");
});

for (const op of ["sync", "ack", "defer", "report"]) test(`definite preexecution ${op} rejection retains only safe pending intent on the same connection`, { timeout: 15000 }, async t => {
  const parent = task(`availability-${op}`, "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  hub.rejectNext = { op };
  if (op === "report") {
    await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Already completed fixture work." });
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

test("accepted human input invalidates a report retained after definite nonexecution", { timeout: 15000 }, async t => {
  const parent = task("superseded-unavailable", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  hub.rejectNext = { op: "report" };
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Old staged claim." });
  await h.settle(false);
  await eventually(() => h.statuses.at(-1)?.includes("identity check unavailable"));
  await acceptUser(h, "New human direction supersedes the rejected old report");
  hub.deliver(envelope({ id: "after-human-reconciliation", wake: false }));
  await eventually(() => hub.calls.some(call => call.op === "ack" && call.message_id === "after-human-reconciliation"));
  assert.equal(hub.calls.filter(call => call.op === "report").length, 1);
  assert.equal(reportCalls(hub, parent.id).length, 0);
});

test("an uncertain staged-report reply is never replayed on reconnect", { timeout: 15000 }, async t => {
  const parent = task("uncertain-report", "requester", "pi-session");
  const { h, hub } = await setup(t, [parent]);
  hub.rejectNext = { op: "report", error: new Error("Hub acknowledgement timed out; delivery is uncertain") };
  await h.tools.get("mustr_report").execute("terminal", { task_id: parent.id, revision: 1, kind: "done", text: "Uncertain claim, never replay." });
  await h.settle(false);
  await eventually(() => hub.clients.length === 2 && hub.clients[1].connected);
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
  await h.settle(false);
  await eventually(() => reportCalls(hub, parent.id).length === 1);
  assert.match(h.statuses.at(-1), /manual pause/);
  assert.equal(hub.syncs.at(-1).receive, false);
});
