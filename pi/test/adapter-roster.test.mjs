import test from "node:test";
import assert from "node:assert/strict";
import { fixture, binary, eventually } from "../../tests/support.mjs";
import { HubClient } from "../transport.mjs";
import { harness } from "./adapter-harness.mjs";

const self = { id: "pi-session", name: "local-implementor", role: "implementor" };
const finishedStates = new Set(["accepted", "cancelled"]);

function task(index, state, note = `task note ${index}`) {
  return {
    id: `task-${String(index).padStart(2, "0")}`,
    requester: `requester-${index}`,
    owner: index % 2 ? "peer" : "pi-session",
    title: `Synthetic task ${index}`,
    state,
    revision: index + 1,
    ...(state === "queued" ? { queue_position: index + 1 } : {}),
    note,
  };
}

function snapshot(tasks, tasksTruncated = false) {
  return {
    members: [
      { id: "pi-session", name: self.name, role: self.role, pane_id: "w1:p1", space_id: "w1", manager: false, activity: "idle", last_seen: 100, idle_since: 100 },
      { id: "peer", name: "architect-peer", role: "architect", pane_id: "w1:p2", space_id: "w1", manager: true, activity: "working", last_seen: 99, idle_since: 90 },
    ],
    tasks,
    tasks_truncated: tasksTruncated,
    inbox: null,
    insertion_policy: "live",
    automation: { todo_enabled: false, checkin_enabled: true, pr_enabled: false, todo: null, pr: null },
    reset_control: {
      plan_id: "fixture-plan", token: "private-fixture-ticket", space_id: "w1", old_agent_id: self.id,
      name: self.name, role: self.role, pane_id: "w1:p1", state: "claimed",
    },
  };
}

function expectedSummary(tasks) {
  const finished = tasks.filter(item => finishedStates.has(item.state));
  return {
    counts: {
      accepted: finished.filter(item => item.state === "accepted").length,
      cancelled: finished.filter(item => item.state === "cancelled").length,
    },
    sample: finished.slice(0, 3).map(({ id, title, state, revision }) => ({ id, title, state, revision })),
    omitted_from_sample: Math.max(0, finished.length - 3),
    scope: "Returned task summaries only; the hub may truncate the roster (tasks_truncated).",
    sample_order: "hub-returned order; not a global recency ranking",
  };
}

test("mustr_roster compacts model output while retaining the full safe snapshot", { timeout: 30000 }, async t => {
  const previousEnv = new Map(["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR", "MUSTR_BIN"]
    .map(key => [key, process.env[key]]));
  const f = await fixture(t);
  const h = harness(f.directory);
  f.beforeCleanup(async () => {
    try { await h.emit("session_shutdown"); }
    finally {
      for (const [key, value] of previousEnv) value === undefined ? delete process.env[key] : process.env[key] = value;
    }
  });
  await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary;
  process.env.HERDR_PANE_ID = "w1:p1";

  const { default: extension } = await import(`../index.ts?roster-compact=${Date.now()}`);
  await extension(h.api);
  await h.emit("session_start");
  await h.commands.get("role").handler("implementor local-implementor", h.context);
  const tool = h.tools.get("mustr_roster");
  await eventually(() => tool.execute("ready", {}, undefined));

  const schema = tool.parameters;
  assert.match(h.tools.get("mustr_assign").description, /per-owner FIFO/i);
  assert.match(h.tools.get("mustr_assign").description, /queued is not executed until dispatch/i);
  assert.equal(schema.type, "object");
  assert.equal(schema.properties.include_finished.type, "boolean");
  assert.equal(schema.required?.includes("include_finished") ?? false, false, "include_finished is optional");

  const originalRequest = HubClient.prototype.request;
  let response;
  const interceptedSyncs = [];
  HubClient.prototype.request = async function(command, ...args) {
    if (command.op === "sync" && command.receive === false && response) {
      interceptedSyncs.push(structuredClone(command));
      return response;
    }
    return originalRequest.call(this, command, ...args);
  };
  t.after(() => { HubClient.prototype.request = originalRequest; });

  async function invoke(raw, params = {}) {
    const before = structuredClone(raw);
    response = raw;
    const result = await tool.execute("roster-fixture", params, undefined);
    assert.deepEqual(raw, before, "compaction and the include_finished flag do not mutate the raw sync response");
    assert.deepEqual(result.details.tasks, raw.tasks, "details retain the full hub-returned task list");
    assert.equal(JSON.stringify(result.details).includes("reset_control"), false, "private reset control stays sanitized");
    assert.equal(JSON.stringify(result.content).includes("private-fixture-ticket"), false, "private reset ticket never reaches model content");
    const { reset_control: _control, ...safe } = raw;
    assert.deepEqual(result.details, { ...safe, self }, "details remain the full safe roster in either mode");
    assert.ok(interceptedSyncs.at(-1), "the roster tool refreshed its hub snapshot");
    assert.equal(Object.hasOwn(interceptedSyncs.at(-1), "include_finished"), false, "the Pi-only option is not sent to the hub");
    return result;
  }

  async function invokeDefault(raw, params = {}) {
    const result = await invoke(raw, params);
    const model = JSON.parse(result.content[0].text);
    const unfinished = raw.tasks.filter(item => !finishedStates.has(item.state));
    const { reset_control: _control, ...safe } = raw;
    assert.deepEqual(model, {
      ...safe,
      self,
      tasks: unfinished,
      finished_tasks_summary: expectedSummary(raw.tasks),
    });
    return { result, model };
  }

  const empty = snapshot([]);
  const emptyResult = await invokeDefault(empty);
  assert.deepEqual(emptyResult.model.tasks, []);
  assert.deepEqual(emptyResult.model.finished_tasks_summary.counts, { accepted: 0, cancelled: 0 });
  assert.deepEqual(emptyResult.model.finished_tasks_summary.sample, []);

  const activeTasks = ["assigned", "active", "blocked", "waiting", "review", "paused", "queued"]
    .map((state, index) => task(index, state, `unfinished-note-${state}`));
  const activeResult = await invokeDefault(snapshot(activeTasks));
  assert.deepEqual(activeResult.model.tasks, activeTasks, "all unfinished fields, notes, and ordering remain unchanged");
  assert.deepEqual(activeResult.model.finished_tasks_summary.counts, { accepted: 0, cancelled: 0 });
  for (const item of activeTasks) assert.ok(activeResult.result.content[0].text.includes(item.note));

  const mixedTasks = [
    task(0, "assigned", "unfinished-assigned-note"),
    task(1, "accepted", "finished-secret-one"),
    task(2, "active", "unfinished-active-note"),
    task(3, "cancelled", "finished-secret-two"),
    task(4, "waiting", "unfinished-waiting-note"),
    task(5, "accepted", "finished-secret-three"),
    task(6, "cancelled", "finished-secret-four"),
    task(7, "review", "unfinished-review-note"),
  ];
  const mixedRaw = snapshot(mixedTasks);
  const mixedResult = await invokeDefault(mixedRaw);
  assert.deepEqual(mixedResult.model.tasks, [mixedTasks[0], mixedTasks[2], mixedTasks[4], mixedTasks[7]]);
  assert.deepEqual(mixedResult.model.finished_tasks_summary.counts, { accepted: 2, cancelled: 2 });
  assert.deepEqual(mixedResult.model.finished_tasks_summary.sample, [mixedTasks[1], mixedTasks[3], mixedTasks[5]]
    .map(({ id, title, state, revision }) => ({ id, title, state, revision })));
  assert.equal(mixedResult.model.finished_tasks_summary.omitted_from_sample, 1);
  assert.deepEqual(Object.keys(mixedResult.model.finished_tasks_summary.sample[0]).sort(), ["id", "revision", "state", "title"]);
  assert.ok(mixedResult.result.content[0].text.includes("unfinished-active-note"));
  for (const note of ["finished-secret-one", "finished-secret-two", "finished-secret-three", "finished-secret-four"]) {
    assert.ok(!mixedResult.result.content[0].text.includes(note), "finished notes are omitted from default model content");
  }

  const history = Array.from({ length: 64 }, (_, index) => task(
    index,
    index % 2 === 0 ? "accepted" : "cancelled",
    `history-note-${String(index).padStart(2, "0")}-${"x".repeat(250)}`,
  ));
  const historyRaw = snapshot(history, true);
  const compactHistory = await invokeDefault(historyRaw);
  const summary = compactHistory.model.finished_tasks_summary;
  assert.equal(compactHistory.model.tasks_truncated, true, "the hub's truncation signal is preserved");
  assert.deepEqual(summary.counts, { accepted: 32, cancelled: 32 }, "counts cover only the 64 returned summaries");
  assert.deepEqual(summary.sample, history.slice(0, 3).map(({ id, title, state, revision }) => ({ id, title, state, revision })));
  assert.equal(summary.sample.length, 3);
  assert.equal(summary.omitted_from_sample, 61);
  assert.match(summary.scope, /returned task summaries only/i);
  assert.match(summary.scope, /tasks_truncated/);
  assert.match(summary.sample_order, /hub-returned order/);
  assert.match(summary.sample_order, /not a global recency ranking/);
  assert.ok(!compactHistory.result.content[0].text.includes("history-note-00-"), "finished notes do not leak into compact output");

  const fullHistory = await invoke(historyRaw, { include_finished: true });
  assert.deepEqual(JSON.parse(fullHistory.content[0].text), fullHistory.details, "opt-in content restores the previous complete safe roster");
  assert.equal(fullHistory.details.tasks.length, 64);
  assert.ok(fullHistory.content[0].text.includes("history-note-00-"));
  assert.ok(!Object.hasOwn(JSON.parse(fullHistory.content[0].text), "finished_tasks_summary"));
  assert.ok(compactHistory.result.content[0].text.length * 5 < fullHistory.content[0].text.length,
    `history-heavy model text shrinks substantially (${compactHistory.result.content[0].text.length} vs ${fullHistory.content[0].text.length} chars)`);

  const explicitFalse = await invoke(mixedRaw, { include_finished: false });
  assert.deepEqual(JSON.parse(explicitFalse.content[0].text), mixedResult.model, "false preserves the compact default");
  assert.ok(interceptedSyncs.every(command => !Object.hasOwn(command, "include_finished")));
});
