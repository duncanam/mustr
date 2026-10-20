import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { stat } from "node:fs/promises";
import { HubClient } from "../pi/transport.mjs";
import { fixture } from "./support.mjs";

test("fixture cleanup stops children and removes files even when adapter teardown fails", async t => {
  let cleanup;
  let cleanupStarted = false;
  const f = await fixture({ after: callback => { cleanup = callback; } });
  t.after(async () => {
    if (!cleanupStarted) {
      cleanupStarted = true;
      await cleanup();
    }
  });
  const child = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], { stdio: "ignore" });
  f.track(child);
  f.beforeCleanup(async () => { throw new Error("fixture teardown sentinel"); });

  cleanupStarted = true;
  await assert.rejects(cleanup(), /fixture teardown sentinel/);
  assert.ok(child.exitCode !== null || child.signalCode !== null, "tracked subprocess is stopped after callback failure");
  await assert.rejects(stat(f.directory), { code: "ENOENT" }, "fixture files are removed after callback failure");
});

async function join(t, f, id, pane = "w1:p2") {
  const client = new HubClient();
  t.after(() => client.close());
  await client.connect(f.discovery.socket, {
    agent_id: id, name: id, role: "implementor", pane_id: pane, space_id: "w1",
  });
  await client.request({ op: "sync", activity: "idle", receive: false });
  return client;
}

test("Pi and Claude Code share scoped messages and task ownership", async t => {
  const f = await fixture(t);
  f.panes.get("w1:p2").agent = "claude";
  await f.start();
  const pi = await join(t, f, "pi", "w1:p1");
  const claude = await join(t, f, "claude");
  const snapshot = await pi.request({ op: "sync", activity: "idle", receive: false });
  assert.deepEqual(snapshot.members.map(member => member.id).sort(), ["claude", "pi"]);
  const task = await pi.request({ op: "assign", to: "claude", title: "Mixed-harness task", text: "Verify the fixture." });
  const offer = await claude.request({ op: "sync", activity: "idle", receive: true });
  assert.equal(offer.inbox.task_id, task.task_id);
  await claude.request({ op: "ack", message_id: offer.inbox.id });
  await claude.request({ op: "report", task_id: task.task_id, revision: 1, kind: "done", text: "Fixture verified." });
  await pi.request({ op: "accept", task_id: task.task_id, revision: 1, text: "Independently verified." });
  assert.equal((await pi.request({ op: "sync", activity: "idle", receive: false })).tasks[0].state, "accepted");
});

for (const designation of [null, "unknown", "claude-code"]) {
  test(`admission rejects unsupported harness ${JSON.stringify(designation)}`, async t => {
    const f = await fixture(t);
    f.panes.get("w1:p2").agent = designation;
    await f.start();
    await assert.rejects(join(t, f, "unsupported"), /supported agent/);
  });
}

for (const [before, after] of [["pi", "claude"], ["claude", "pi"], ["claude", null]]) {
  test(`existing connection cannot survive harness replacement ${before} -> ${after}`, async t => {
    const f = await fixture(t);
    f.panes.get("w1:p2").agent = before;
    await f.start();
    const client = await join(t, f, "original");
    f.panes.get("w1:p2").agent = after;
    await assert.rejects(client.request({ op: "sync", activity: "idle", receive: false }), /supported agent/);
  });
}

test("admission rejects a mismatched live pane identity", async t => {
  const f = await fixture(t);
  f.panes.get("w1:p2").pane_id = "w1:p1";
  await f.start();
  await assert.rejects(join(t, f, "wrong-pane"), /pane moved/);
});
