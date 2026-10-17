import test from "node:test";
import assert from "node:assert/strict";
import { writeFile, stat, mkdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { HubClient } from "../pi/transport.mjs";
import { fixture, binary, exec, eventually, delay, stop } from "./support.mjs";

async function join(t, f, id, pane, space = "w1", role = "implementor") {
  const client = new HubClient(); t.after(() => client.close());
  await client.connect(f.discovery.socket, { agent_id: id, name: id, role, pane_id: pane, space_id: space });
  await client.request({ op: "sync", activity: "idle", receive: false });
  return client;
}
const sync = (c, receive = false, activity = "idle") => c.request({ op: "sync", activity, receive });
async function receive(client) {
  const snapshot = await sync(client, true);
  if (snapshot.inbox) await client.request({ op: "ack", message_id: snapshot.inbox.id });
  return snapshot.inbox;
}

test("socket admission rejects invalid initial/reset joins and resolves shared components per agent", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const roles = `${f.directory}/config/roles`;
  await writeFile(`${roles}/missing.md`, "+++\ncompose = [\"absent\"]\n+++\nBroken role.\n");
  await writeFile(`${roles}/late-component.md`, "Selected component remains valid only until admission.\n");
  await writeFile(`${roles}/late.md`, "+++\ncompose = [\"late-component\"]\n+++\nSelected role.\n");
  await writeFile(`${roles}/shared.md`, "Shared instructions.\n");
  await writeFile(`${roles}/shared-a.md`, "+++\ncompose = [\"shared\"]\n+++\nRole A.\n");
  await writeFile(`${roles}/shared-b.md`, "+++\ncompose = [\"shared\"]\n+++\nRole B.\n");
  const selected = await exec(binary, ["roles", "late", "--json"], { env: f.env });
  assert.equal(JSON.parse(selected.stdout)[0].name, "late");
  await writeFile(`${roles}/late-component.md`, "+++\ncompose = [\"absent-after-selection\"]\n+++\nChanged component.\n");
  await f.start();

  const staleSelection = new HubClient(); t.after(() => staleSelection.close());
  await assert.rejects(staleSelection.connect(f.discovery.socket, {
    agent_id: "stale-selection", name: "stale-selection", role: "late", pane_id: "w1:p1", space_id: "w1",
  }), /late -> late-component -> absent-after-selection/);

  const rejectedInitial = new HubClient(); t.after(() => rejectedInitial.close());
  await assert.rejects(rejectedInitial.connect(f.discovery.socket, {
    agent_id: "unadmitted", name: "unadmitted", role: "missing", pane_id: "w1:p1", space_id: "w1",
  }), /missing -> absent/);
  const rejectedReset = new HubClient(); t.after(() => rejectedReset.close());
  await assert.rejects(rejectedReset.connect(f.discovery.socket, {
    agent_id: "retryable", name: "retryable", role: "missing", pane_id: "w1:p1", space_id: "w1",
    reset: { plan_id: "not-a-real-plan", token: "not-a-real-token" },
  }), /missing -> absent/);

  // A valid join with the same identity/pane after the rejected reset variant
  // would fail if validation had partially registered the earlier request.
  const first = await join(t, f, "retryable", "w1:p1", "w1", "shared-a");
  const second = await join(t, f, "second", "w1:p2", "w1", "shared-b");
  const members = (await sync(first)).members;
  assert.deepEqual(members.map(member => [member.id, member.role]).sort(), [
    ["retryable", "shared-a"], ["second", "shared-b"],
  ]);
  assert.equal(members.some(member => member.id === "unadmitted" || member.role === "missing"), false);
  assert.equal((await sync(second)).members.length, 2);
});

test("real socket hub: isolated spaces, task exchange, rollback and restart reconciliation", { timeout: 20000 }, async t => {
  const f = await fixture(t);
  let hub = await f.start();
  const architect = await join(t, f, "architect", "w1:p1");
  const worker = await join(t, f, "worker", "w1:p2");
  const stranger = await join(t, f, "stranger", "w2:p1", "w2");
  assert.equal((await stat(f.discovery.socket)).mode & 0o777, 0o600);
  assert.equal((await sync(architect)).members.length, 2);
  assert.equal((await sync(stranger)).members.length, 1);
  await assert.rejects(architect.request({ op: "send", to: "stranger", text: "cross-space", wake: true }), /space/);
  await assert.rejects(exec(binary, ["run", "--headless"], { env: f.env, timeout: 3000 }), /hub|running|lock/);
  const { task_id } = await architect.request({ op: "assign", to: "worker", title: "Verify fixture", text: "Change nothing; verify the fixture and report evidence." });
  const assignment = await receive(worker);
  assert.equal(assignment.task_id, task_id); assert.equal(assignment.kind, "assignment");
  await assert.rejects(architect.request({ op: "report", task_id, revision: 1, kind: "done", text: "False" }), /owner/);
  await assert.rejects(worker.request({ op: "report", task_id, revision: 1, kind: "done", text: "" }), /text|empty/);
  assert.equal((await sync(worker)).tasks[0].state, "active"); // failed request did not commit
  await worker.request({ op: "report", task_id, revision: 1, kind: "blocked", text: "Need a clarification." });
  assert.equal((await receive(architect)).kind, "blocked");
  await architect.request({ op: "guide", task_id, revision: 1, text: "Here is the clarification; proceed." });
  assert.equal((await receive(worker)).revision, 2);
  await worker.request({ op: "report", task_id, revision: 2, kind: "done", text: "Verified independently." });
  assert.equal((await receive(architect)).kind, "done");
  await architect.request({ op: "accept", task_id, revision: 2, text: "Evidence reviewed." });
  assert.equal((await sync(worker)).tasks[0].state, "accepted");
  await receive(worker); // acceptance information, not a new turn
  const second = await architect.request({ op: "assign", to: "worker", title: "Uncertain fixture", text: "Do not blindly replay." });
  assert.equal((await sync(worker, true)).inbox.kind, "assignment"); // offered, deliberately unacknowledged
  await architect.request({ op: "mode", mode: "todo", enabled: true });
  await stop(hub);
  await eventually(() => !architect.connected && !worker.connected && !stranger.connected);
  hub = await f.start();
  const returning = await join(t, f, "worker", "w1:p2");
  const after = await sync(returning, true);
  assert.equal(after.tasks.find(t => t.id === second.task_id).state, "paused");
  assert.equal(after.inbox, null); assert.equal(after.automation.todo_enabled, false);
  // A live topology move invalidates the binding, even with stale inherited env IDs.
  f.panes.set("w1:p2", { pane_id: "w1:p2", workspace_id: "w2", agent: "pi" });
  await assert.rejects(sync(returning), /moved|space|disconnected/);
});

test("push hints are opt-in, scoped, coalesced, and preserve partial request frames", { timeout: 12000 }, async t => {
  const f = await fixture(t); await f.start();
  const sender = await join(t, f, "sender", "w1:p1");
  const worker = await join(t, f, "worker", "w1:p2");
  const stranger = await join(t, f, "stranger", "w2:p1", "w2");
  let notices = 0, otherNotices = 0, unsubscribedNotices = 0;
  worker.onInboxReady = () => notices++;
  stranger.onInboxReady = () => otherNotices++;
  sender.onInboxReady = () => unsubscribedNotices++;
  await worker.request({ op: "subscribe" });
  await stranger.request({ op: "subscribe" });
  const packets = []; let buffer = "";
  worker.socket.on("data", chunk => {
    buffer += chunk;
    while (buffer.includes("\n")) {
      const index = buffer.indexOf("\n");
      packets.push(JSON.parse(buffer.slice(0, index))); buffer = buffer.slice(index + 1);
    }
  });
  // A notification must not cancel read_line and discard this partial frame.
  worker.socket.write('{"id":"frag');
  await sender.request({ op: "send", to: "worker", text: "push, not polling", wake: false });
  await eventually(() => notices === 1);
  assert.deepEqual(packets.find(p => p.event), { event: "inbox_ready" });
  worker.socket.write('ment","op":"sync","activity":"idle","receive":true}\n');
  await eventually(() => packets.some(p => p.id === "fragment"));
  const response = packets.find(p => p.id === "fragment");
  assert.equal(response.ok, true);
  assert.equal(response.result.inbox.text, "push, not polling");
  await sender.request({ op: "send", to: "worker", text: "second", wake: false });
  for (let i = 0; i < 3; i++) await sync(sender);
  await delay(150);
  assert.equal(notices, 1, "no more offers/hints while insertion is unconfirmed");
  await worker.request({ op: "ack", message_id: response.result.inbox.id });
  await eventually(() => notices === 2);
  assert.equal((await receive(worker)).text, "second");
  await worker.request({ op: "send", to: "sender", text: "legacy poller", wake: false });
  await delay(150);
  assert.equal(otherNotices, 0);
  assert.equal(unsubscribedNotices, 0);
  assert.equal((await receive(sender)).text, "legacy poller");
});

test("TODO waits for delegated work; disabling cancels pending automation", { timeout: 15000 }, async t => {
  const f = await fixture(t); await f.start();
  const driver = await join(t, f, "driver", "w1:p1");
  const worker = await join(t, f, "worker", "w1:p2");
  const path = `${f.directory}/TODO.md`; await writeFile(path, "- [ ] Verify the fixture\n");
  await driver.request({ op: "configure_todo", path });
  await driver.request({ op: "mode", mode: "todo", enabled: true });
  const assignment = await driver.request({ op: "assign", to: "worker", title: "Delegation", text: "Verify without duplication." });
  await receive(worker); await delay(2200);
  assert.equal(await receive(driver), null);
  await worker.request({ op: "report", task_id: assignment.task_id, revision: 1, kind: "done", text: "Verified." });
  await receive(driver);
  await driver.request({ op: "accept", task_id: assignment.task_id, revision: 1, text: "Accepted." });
  const todo = await eventually(async () => await receive(driver));
  assert.equal(todo.kind, "todo");
  await driver.request({ op: "mode", mode: "todo", enabled: false });
  await delay(1200); assert.equal(await receive(driver), null);
});

test("PR watcher executes only fixture git/gh reads and catches all feedback surfaces", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const url = "https://github.com/example/repo/pull/1";
  const data = { branch: "feature", head: "abc", raw: { url, state: "OPEN", headRefName: "feature", headRefOid: "abc", comments: [{ id: "c1", body: "old conversation" }], reviews: [{ id: "r1", body: "old review", state: "CHANGES_REQUESTED" }], statusCheckRollup: [{ __typename: "CheckRun", status: "COMPLETED", conclusion: "FAILURE", name: "test", detailsUrl: `${url}/checks` }] }, pages: [[{ id: 1, body: "old inline" }]] };
  const input = `${f.directory}/github.json`, log = `${f.directory}/commands.log`, bin = `${f.directory}/bin`;
  await mkdir(bin); await writeFile(input, JSON.stringify(data));
  const program = `#!/usr/bin/env node
const fs = require("node:fs"), path = require("node:path");
const input = JSON.parse(fs.readFileSync(process.env.MUSTR_TEST_GITHUB));
const name = path.basename(process.argv[1]), args = process.argv.slice(2);
fs.appendFileSync(process.env.MUSTR_TEST_LOG, JSON.stringify({name,args}) + "\\n");
if (name === "git" && JSON.stringify(args) === JSON.stringify(["rev-parse","--abbrev-ref","HEAD"])) console.log(input.branch);
else if (name === "git" && JSON.stringify(args) === JSON.stringify(["rev-parse","HEAD"])) console.log(input.head);
else if (name === "gh" && args[0] === "pr" && args[1] === "view" && args[2] === input.raw.url && args[3] === "--json") console.log(JSON.stringify(input.raw));
else if (name === "gh" && JSON.stringify(args) === JSON.stringify(["api","repos/example/repo/pulls/1/comments?per_page=100","--paginate","--slurp"])) console.log(JSON.stringify(input.pages));
else { console.error("Unexpected command"); process.exit(99); }
`;
  for (const name of ["git", "gh"]) await writeFile(`${bin}/${name}`, program, { mode: 0o700 });
  Object.assign(f.env, { PATH: `${bin}:${f.env.PATH}`, MUSTR_TEST_GITHUB: input, MUSTR_TEST_LOG: log });
  await f.start(); const driver = await join(t, f, "driver", "w1:p1");
  await driver.request({ op: "configure_pr", url, cwd: f.directory });
  await driver.request({ op: "mode", mode: "pr", enabled: true });
  const first = await eventually(async () => await receive(driver));
  assert.equal(first.kind, "pr"); assert.match(first.text, /Failed check: test/); assert.doesNotMatch(first.text, /old conversation/);
  await driver.request({ op: "mode", mode: "pr", enabled: false });
  data.raw.comments[0].body = "edited conversation"; data.raw.reviews[0].body = "new review decision"; data.pages[0][0].body = "edited inline reply";
  await writeFile(input, JSON.stringify(data));
  await driver.request({ op: "mode", mode: "pr", enabled: true });
  const changed = await eventually(async () => await receive(driver));
  assert.match(changed.text, /edited conversation/); assert.match(changed.text, /new review decision/); assert.match(changed.text, /edited inline reply/);
  assert.doesNotMatch(changed.text, /Failed check: test/);
  await driver.request({ op: "mode", mode: "pr", enabled: false });
  data.branch = "other"; await writeFile(input, JSON.stringify(data));
  await driver.request({ op: "mode", mode: "pr", enabled: true });
  await eventually(async () => { const s = await sync(driver); return !s.automation.pr_enabled && /branch changed/.test(s.automation.pr.error); });
  const calls = (await readFile(log, "utf8")).trim().split("\n").map(line => JSON.parse(line));
  assert.ok(calls.some(c => c.args[0] === "api" && c.args.includes("--paginate")));
  assert.ok(calls.every(c => c.name === "git" ? c.args[0] === "rev-parse" : ["pr", "api"].includes(c.args[0])));
});

test("TUI opens its live space, restores alternate screen and uses terminal-native colors", { timeout: 15000, skip: !["darwin", "linux"].includes(process.platform) }, async t => {
  const f = await fixture(t);
  // Live pane identity must win over inherited/stale workspace environment.
  f.panes.set("w1:p0", { pane_id: "w1:p0", workspace_id: "w2", agent: null });
  const project = `${f.directory}/pty`; await mkdir(project);
  await writeFile(`${project}/pyproject.toml`, '[project]\nname = "mustr-pty-test"\nversion = "0.0.0"\nrequires-python = ">=3.9"\ndependencies = []\n');
  const child = spawn("uv", ["run", "--project", project, fileURLToPath(new URL("pty-smoke.py", import.meta.url)), binary], { env: { ...f.env, HERDR_WORKSPACE_ID: "w1", MUSTR_TEST_INITIAL_TAB: "2/2", TERM: "xterm-256color" }, stdio: ["pipe", "pipe", "pipe"] });
  f.track(child);
  let output = ""; child.stdout.on("data", b => output += b); child.stderr.on("data", b => output += b);
  const [code] = await once(child, "exit");
  assert.equal(code, 0, output);
  assert.ok(output.includes("\x1b[?1049h"), "entered alternate screen");
  assert.ok(output.includes("\x1b[?1049l"), "restored alternate screen");
  assert.ok(!output.includes(";2;"), "no hard-coded truecolor palette");
});
