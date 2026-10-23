import test from "node:test";
import assert from "node:assert/strict";
import { writeFile, stat, mkdir, readFile, readdir, realpath, symlink, unlink } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { basename } from "node:path";
import { spawn } from "node:child_process";
import net from "node:net";
import { once } from "node:events";
import { HubClient } from "../pi/transport.mjs";
import { harness as piHarness } from "../pi/test/adapter-harness.mjs";
import { fixture, binary, exec, eventually, delay, stop } from "./support.mjs";
import { installFakePrTools } from "./fake-pr-tools.mjs";

async function join(t, f, id, pane, space = "w1", role = "implementor") {
  const client = new HubClient(); t.after(() => client.close());
  const discovery = await f.discover(pane);
  await client.connect(discovery.socket, { agent_id: id, name: id, role, pane_id: pane, space_id: space });
  await client.request({ op: "sync", activity: "idle", receive: false });
  return client;
}
const sync = (c, receive = false, activity = "idle") => c.request({ op: "sync", activity, receive });

class McpProcess {
  constructor(child) {
    this.child = child; this.buffer = ""; this.nextId = 1; this.pending = new Map(); this.stderr = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", chunk => {
      this.buffer += chunk;
      while (this.buffer.includes("\n")) {
        const end = this.buffer.indexOf("\n"), line = this.buffer.slice(0, end); this.buffer = this.buffer.slice(end + 1);
        const message = JSON.parse(line), pending = this.pending.get(message.id);
        if (pending) { clearTimeout(pending.timer); this.pending.delete(message.id); pending.resolve(message); }
      }
    });
    child.stderr.setEncoding("utf8"); child.stderr.on("data", chunk => { this.stderr += chunk; });
    child.on("exit", (code, signal) => {
      for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error(`MCP exited (${code ?? signal}): ${this.stderr}`)); }
      this.pending.clear();
    });
  }
  request(method, params = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`Timed out waiting for ${method}`)); }, 10000);
      this.pending.set(id, { resolve, reject, timer });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    });
  }
  notify(method, params = {}) { this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`); }
}

async function receive(client) {
  const snapshot = await sync(client, true);
  if (snapshot.inbox) await client.request({ op: "ack", message_id: snapshot.inbox.id });
  return snapshot.inbox;
}

async function joinCheckout(t, f, id, pane, cwd) {
  const client = new HubClient(); t.after(() => client.close());
  await client.connect(f.discovery.socket, {
    agent_id: id, name: id, role: "implementor", pane_id: pane, space_id: "w1", cwd,
  });
  await client.request({ op: "sync", activity: "working", receive: false });
  return client;
}

async function startTuiPty(t, f) {
  const project = `${f.directory}/uv-pty`; await mkdir(project);
  await writeFile(`${project}/pyproject.toml`, '[project]\nname = "mustr-pr-pty-test"\nversion = "0.0.0"\nrequires-python = ">=3.9"\ndependencies = []\n');
  const child = spawn("uv", ["run", "--project", project, fileURLToPath(new URL("pty-interactive.py", import.meta.url)), binary], {
    env: { ...f.env, TERM: "xterm-256color" }, stdio: ["pipe", "pipe", "pipe"],
  });
  f.track(child);
  let output = "";
  child.stdout.on("data", chunk => { output += chunk; });
  child.stderr.on("data", chunk => { output += chunk; });
  const exited = once(child, "exit");
  await eventually(() => {
    if (child.exitCode !== null) throw new Error(`PTY TUI exited early: ${output}`);
    return output.includes("Automation");
  }, 15000);
  await eventually(() => new Promise(resolve => {
    const socket = net.createConnection(f.discovery.socket);
    socket.once("connect", () => { socket.destroy(); resolve(true); });
    socket.once("error", () => resolve(false));
  }), 10000);
  return { child, exited, output: () => output, send: bytes => child.stdin.write(bytes) };
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

test("real hub admits duplicate non-manager roles with distinct identities and names", { timeout: 20000 }, async t => {
  const f = await fixture(t);
  const roles = `${f.directory}/config/roles`;
  await writeFile(`${roles}/custom.md`, "+++\ncompose = [\"researcher\"]\n+++\nCustom composed role fixture.\n");
  await f.start();

  for (const [index, role] of ["researcher", "reviewer", "implementor", "custom"].entries()) {
    const first = await join(t, f, `${role}-first`, "w1:p1", "w1", role);
    const firstActivity = ["paused", "idle", "working", "paused"][index];
    await sync(first, false, firstActivity);
    const second = await join(t, f, `${role}-second`, "w1:p2", "w1", role);
    const roster = await sync(second, false, "working");
    const peers = roster.members.filter(member => member.role === role);
    assert.deepEqual(peers.map(member => [member.id, member.name]).sort(), [
      [`${role}-first`, `${role}-first`], [`${role}-second`, `${role}-second`],
    ]);
    assert.equal(peers.length, 2, `${role}: both peers remain in the roster`);
    assert.equal(peers.find(member => member.id === `${role}-first`).activity, firstActivity);
    assert.equal(peers.find(member => member.id === `${role}-second`).activity, "working");
    await first.request({ op: "leave" });
    await second.request({ op: "leave" });
  }
});

test("real socket hub: isolated spaces, task exchange, rollback and restart reconciliation", { timeout: 20000 }, async t => {
  const f = await fixture(t);
  let hub = await f.start();
  const architect = await join(t, f, "architect", "w1:p1");
  const worker = await join(t, f, "worker", "w1:p2");
  assert.equal((await stat(f.discovery.socket)).mode & 0o777, 0o600);
  assert.equal((await sync(architect)).members.length, 2);
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
  await eventually(() => !architect.connected && !worker.connected);
  hub = await f.start();
  const returning = await join(t, f, "worker", "w1:p2");
  const after = await sync(returning, true);
  assert.equal(after.tasks.find(t => t.id === second.task_id).state, "paused");
  assert.equal(after.inbox, null); assert.equal(after.automation.todo_enabled, false);
  // A live topology move invalidates the binding, even with stale inherited env IDs.
  f.panes.set("w1:p2", { pane_id: "w1:p2", workspace_id: "w2", agent: "pi" });
  await assert.rejects(sync(returning), /moved|space|disconnected/);
});

test("per-space hubs isolate endpoints, locks, durable state, restart and shutdown", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  const first = f.discovery;
  const second = await f.discover("w2:p0");
  assert.equal(first.server_key, second.server_key, "server_key remains the Herdr incarnation identity");
  assert.notEqual(first.socket, second.socket, "each exact space gets a distinct socket");
  assert.notEqual(f.database(first), f.database(second), "each space gets a distinct durable database");

  let firstHub = await f.start();
  const firstLog = f.database(first).replace(/hub\.sqlite$/, "mustr.log");
  await writeFile(firstLog, Buffer.alloc(5 * 1024 * 1024 + 1, 0x61));
  const logInode = (await stat(firstLog)).ino;
  await assert.rejects(
    exec(binary, ["-v", "run", "--headless"], { env: f.env, timeout: 5000 }),
    /already running|hub|lock|space/i,
  );
  assert.equal((await stat(firstLog)).ino, logInode, "duplicate startup must fail before log rotation");
  await assert.rejects(stat(firstLog.replace(/mustr\.log$/, "mustr.log.1")), /ENOENT/);

  const secondHub = await f.start({ paneId: "w2:p0" });
  assert.notEqual(firstHub.discovery.socket, secondHub.discovery.socket);
  const firstRequester = await join(t, f, "first-requester", "w1:p1", "w1", "architect");
  const firstWorker = await join(t, f, "first-worker", "w1:p2");
  const secondRequester = await join(t, f, "second-requester", "w2:p1", "w2");
  const secondWorker = await join(t, f, "second-worker", "w2:p2", "w2");

  const firstTask = await firstRequester.request({ op: "assign", to: "first-worker", title: "w1 task", text: "Persist only in w1." });
  const secondTask = await secondRequester.request({ op: "assign", to: "second-worker", title: "w2 task", text: "Persist only in w2." });
  assert.equal((await sync(secondRequester)).automation.checkin_enabled, true, "fresh spaces default check-ins on even without a manager");
  await firstRequester.request({ op: "mode", mode: "checkin", enabled: true });
  await secondRequester.request({ op: "mode", mode: "checkin", enabled: false });
  await secondRequester.request({ op: "mode", mode: "todo", enabled: true });
  const firstState = await sync(firstRequester);
  const secondState = await sync(secondRequester);
  assert.equal(firstState.members.length, 2);
  assert.equal(firstState.members.find(member => member.id === "first-requester").manager, true);
  assert.equal(secondState.members.length, 2);
  assert.equal(secondState.members.some(member => member.manager), false);
  assert.deepEqual(firstState.tasks.map(task => task.id), [firstTask.task_id]);
  assert.deepEqual(secondState.tasks.map(task => task.id), [secondTask.task_id]);
  assert.equal(firstState.automation.checkin_enabled, true);
  assert.equal(firstState.automation.todo_enabled, false);
  assert.equal(secondState.automation.todo_enabled, true);
  assert.equal(secondState.automation.checkin_enabled, false);

  const wrongEndpoint = new HubClient(); t.after(() => wrongEndpoint.close());
  await assert.rejects(wrongEndpoint.connect(first.socket, {
    agent_id: "wrong-endpoint", name: "wrong-endpoint", role: "implementor", pane_id: "w2:p1", space_id: "w2",
  }), /wrong mustr endpoint|space/);
  assert.equal((await sync(firstRequester)).members.length, 2, "foreign join was not admitted to w1");

  const databaseInode = (await stat(f.database(first))).ino;
  await stat(f.database(second));
  await stop(firstHub);
  await eventually(() => !firstRequester.connected && !firstWorker.connected);
  assert.equal((await sync(secondWorker)).members.length, 2, "stopping w1 leaves w2 live");

  firstHub = await f.start();
  const returningWorker = await join(t, f, "first-worker", "w1:p2");
  const recovered = await sync(returningWorker);
  assert.equal(recovered.tasks[0].id, firstTask.task_id);
  assert.equal(recovered.tasks[0].state, "paused", "restart retains history but fences active work");
  assert.equal(recovered.automation.checkin_enabled, true, "restart preserves saved check-in choice");
  assert.equal(recovered.automation.todo_enabled, false);
  assert.equal(recovered.automation.pr_enabled, false);
  assert.equal(recovered.members.some(member => member.manager), false, "restart waits for a verified manager rejoin, not an ordinary worker");
  const stillIndependent = await sync(secondWorker);
  assert.equal(stillIndependent.tasks[0].id, secondTask.task_id);
  assert.equal(stillIndependent.tasks[0].state, "assigned");
  assert.equal(stillIndependent.automation.todo_enabled, true);
  assert.equal(stillIndependent.automation.checkin_enabled, false, "one space's check-in choice does not affect another");

  await stop(firstHub);
  await eventually(() => !returningWorker.connected);
  assert.equal((await sync(secondRequester)).members.length, 2, "w1 shutdown does not stop w2");
  assert.equal((await stat(f.database(first))).ino, databaseInode, "restart reuses this space's saved state");
});

test("symlinked server-key state parent is rejected before writes and releases the instance lease", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const serverDir = `${f.env.MUSTR_STATE_DIR}/${f.discovery.server_key}`;
  const redirected = `${f.directory}/redirected-state`;
  await mkdir(f.env.MUSTR_STATE_DIR, { recursive: true });
  await mkdir(redirected);
  await symlink(redirected, serverDir);
  await assert.rejects(
    exec(binary, ["run", "--headless"], { env: f.env, timeout: 5000 }),
    /expected a real directory/,
  );
  const instance = basename(f.discovery.socket, ".sock");
  await assert.rejects(stat(`${redirected}/${instance}`), { code: "ENOENT" });
  assert.deepEqual(await readdir(redirected), [], "failed startup made no redirected log or database files");
  await unlink(serverDir);
  const recovered = await f.start();
  assert.equal(recovered.exitCode, null, "same-space lease is reusable after failed startup");
  await stat(f.database(f.discovery));
  assert.deepEqual(await readdir(redirected), [], "recovery wrote only under the validated state path");
});

test("Pi and Claude adapters follow their live per-space context endpoints", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  f.panes.set("w2:p1", { pane_id: "w2:p1", workspace_id: "w2", agent: "claude" });
  await f.start();
  await f.start({ paneId: "w2:p0" });
  const piContext = await f.discover("w1:p1"), claudeContext = await f.discover("w2:p1");
  assert.equal(piContext.space_id, "w1"); assert.equal(claudeContext.space_id, "w2");
  assert.notEqual(piContext.socket, claudeContext.socket);
  const piObserver = await join(t, f, "pi-observer", "w1:p2");
  const claudeObserver = await join(t, f, "claude-observer", "w2:p2", "w2");

  const envKeys = ["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR", "MUSTR_BIN"];
  const previousEnv = new Map(envKeys.map(key => [key, process.env[key]]));
  Object.assign(process.env, f.env, { HERDR_PANE_ID: "w1:p1", MUSTR_BIN: binary });
  const { default: piExtension } = await import(`../pi/index.ts?space-runtime=${Date.now()}`);
  const pi = piHarness(f.directory);
  await piExtension(pi.api); await pi.emit("session_start");
  f.beforeCleanup(async () => {
    try { await pi.emit("session_shutdown"); }
    finally {
      for (const [key, value] of previousEnv) value === undefined ? delete process.env[key] : process.env[key] = value;
    }
  });
  await pi.commands.get("role").handler("implementor pi-space-owner", pi.context);
  const piMembers = await eventually(async () => {
    const snapshot = await sync(piObserver);
    return snapshot.members.some(member => member.id === "pi-session") ? snapshot.members : undefined;
  }, 8000, "Pi adapter to join w1 context endpoint");
  assert.equal(piMembers.some(member => member.id === "pi-session"), true);
  assert.equal((await sync(claudeObserver)).members.some(member => member.id === "pi-session"), false);

  const claudeEntry = fileURLToPath(new URL("../claude/index.mjs", import.meta.url));
  const child = spawn(process.execPath, [claudeEntry], {
    env: { ...f.env, HERDR_PANE_ID: "w2:p1", MUSTR_BIN: binary },
    stdio: ["pipe", "pipe", "pipe"],
  });
  const mcp = new McpProcess(child), exited = once(child, "exit");
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      child.stdin.end();
      await Promise.race([exited, delay(3000).then(() => { if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL"); })]);
    }
  });
  const initialized = await mcp.request("initialize", {
    protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "space-runtime-test", version: "1" },
  });
  assert.equal(initialized.result.serverInfo.name, "mustr");
  mcp.notify("notifications/initialized");
  const joined = await mcp.request("tools/call", { name: "mustr_join", arguments: { role: "implementor", name: "claude-space-owner" } });
  assert.equal(joined.result.isError, false, joined.result.content?.[0]?.text);
  const claudeAgent = JSON.parse(joined.result.content[0].text).agent_id;
  assert.ok(claudeAgent);
  const claudeMembers = await eventually(async () => {
    const snapshot = await sync(claudeObserver);
    return snapshot.members.some(member => member.id === claudeAgent) ? snapshot.members : undefined;
  }, 8000, "Claude adapter to join w2 context endpoint");
  assert.equal(claudeMembers.some(member => member.id === claudeAgent), true);
  assert.equal((await sync(piObserver)).members.some(member => member.id === claudeAgent), false);
});

test("push hints are opt-in, scoped, coalesced, and preserve partial request frames", { timeout: 12000 }, async t => {
  const f = await fixture(t); await f.start();
  const sender = await join(t, f, "sender", "w1:p1");
  const worker = await join(t, f, "worker", "w1:p2");
  await f.start({ paneId: "w2:p0" });
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
if (name === "git" && JSON.stringify(args) === JSON.stringify(["rev-parse","--show-toplevel"])) console.log(process.cwd());
else if (name === "git" && JSON.stringify(args) === JSON.stringify(["rev-parse","--abbrev-ref","HEAD"])) console.log(input.branch);
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

test("Join cwd auto-discovers once, pins the URL, and leaves actor heartbeats responsive during gh lookup", { timeout: 20000 }, async t => {
  const f = await fixture(t);
  const checkout = `${f.directory}/checkout-alpha`; await mkdir(checkout);
  const url = "https://github.com/example/alpha/pull/7";
  const data = {
    delay_ms: 1600,
    checkouts: {
      [checkout]: {
        root: checkout, branch: "feature-alpha", head: "abc123",
        raw: { url, state: "OPEN", headRefName: "feature-alpha", headRefOid: "abc123", comments: [], reviews: [], statusCheckRollup: [] },
        pages: [[]],
      },
    },
  };
  const tools = await installFakePrTools(f, data);
  await f.start();
  const driver = await joinCheckout(t, f, "alpha-driver", "w1:p1", checkout);
  const observer = await join(t, f, "observer", "w1:p2");

  const requestedAt = Date.now();
  await driver.request({ op: "mode", mode: "pr", enabled: true });
  assert.ok(Date.now() - requestedAt < 1000, "Mode-on does not wait for delayed gh discovery");
  const discovery = await eventually(async () => (await tools.calls()).find(call =>
    call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] === "--json"));
  assert.equal(discovery.args[3], "url,headRefName,state");
  assert.equal(discovery.args.includes(url), false, "first lookup has no PR argument");

  const pending = await sync(observer, false, "working");
  assert.equal(pending.automation.pr_enabled, true);
  assert.equal(pending.automation.pr.url, null);
  assert.equal(pending.automation.pr.error, null);
  assert.ok(pending.automation.pr.baseline == null, "no baseline is committed before full poll success");
  for (let index = 0; index < 3; index++) {
    const heartbeatAt = Date.now();
    const snapshot = await sync(observer, false, "working");
    assert.ok(Date.now() - heartbeatAt < 500, "unrelated actor sync remains responsive during the blocked child process");
    assert.equal(snapshot.members.find(member => member.id === "observer").activity, "working");
    await delay(100);
  }

  const resolved = await eventually(async () => {
    const snapshot = await sync(observer, false, "working");
    if (snapshot.automation.pr?.error) throw new Error(`automatic discovery failed: ${snapshot.automation.pr.error}; calls=${JSON.stringify(await tools.calls())}`);
    return snapshot.automation.pr?.url === url ? snapshot : undefined;
  }, 8000);
  const checkoutRoot = await realpath(checkout);
  const pinnedBefore = (await tools.calls()).filter(call =>
    call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] === url).length;
  assert.ok(pinnedBefore >= 2, "initial full poll and validation use the discovered URL");

  await driver.request({ op: "mode", mode: "pr", enabled: false });
  await driver.request({ op: "mode", mode: "pr", enabled: true });
  await eventually(async () => (await tools.calls()).filter(call =>
    call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] === url).length > pinnedBefore);
  const calls = await tools.calls();
  assert.equal(calls.filter(call => call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] === "--json").length, 1,
    "later polls retain the pinned PR rather than rediscovering the branch");
  assert.ok(calls.some(call => call.name === "git" && call.args[1] === "--show-toplevel" && call.cwd === checkoutRoot));
  assert.ok(calls.filter(call => call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] !== "--json")
    .every(call => call.args[2] === url && call.cwd === checkoutRoot));
});

test("PTY p picker cancellation closes the modal without starting discovery", { timeout: 30000, skip: !["darwin", "linux"].includes(process.platform) }, async t => {
  const f = await fixture(t);
  const alpha = `${f.directory}/checkout-alpha`, beta = `${f.directory}/checkout-beta`;
  await mkdir(alpha); await mkdir(beta);
  const url = "https://github.com/example/beta/pull/9";
  const data = { checkouts: Object.fromEntries([alpha, beta].map((cwd, index) => [cwd, {
    root: cwd, branch: index ? "feature-beta" : "feature-alpha", head: index ? "bbb" : "aaa",
    raw: { url, state: "OPEN", headRefName: index ? "feature-beta" : "feature-alpha", headRefOid: index ? "bbb" : "aaa", comments: [], reviews: [], statusCheckRollup: [] }, pages: [[]],
  }])) };
  const tools = await installFakePrTools(f, data);
  const tui = await startTuiPty(t, f);
  const first = await joinCheckout(t, f, "alpha", "w1:p1", alpha);
  await joinCheckout(t, f, "beta", "w1:p2", beta);
  await eventually(() => tui.output().includes("alpha") && tui.output().includes("beta"), 10000)
    .catch(error => { throw new Error(`${error}\n${tui.output()}`); });

  const openedAt = tui.output().length;
  tui.send("p");
  await eventually(() => tui.output().slice(openedAt).includes("Select PR checkout"), 10000)
    .catch(error => { throw new Error(`${error}\n${tui.output()}`); });
  const picker = tui.output().slice(openedAt);
  assert.ok(picker.includes("alpha"));
  assert.ok(picker.includes("beta"));
  const beforeCancel = await sync(first, false, "working");
  assert.equal(beforeCancel.automation.pr, null);
  tui.send("\x1b");
  await delay(150);
  tui.send("q");
  const [code] = await Promise.race([tui.exited, delay(5000).then(() => { throw new Error("q was swallowed by the cancelled PR picker"); })]);
  assert.equal(code, 0, tui.output());
  assert.deepEqual(await tools.calls(), [], "Escape cancels without starting git/gh discovery");
});

test("PTY p picker binds the explicitly selected checkout and polls only its pinned URL", { timeout: 30000, skip: !["darwin", "linux"].includes(process.platform) }, async t => {
  const f = await fixture(t);
  const alpha = `${f.directory}/checkout-alpha`, beta = `${f.directory}/checkout-beta`;
  await mkdir(alpha); await mkdir(beta);
  const url = "https://github.com/example/beta/pull/9";
  const data = { checkouts: Object.fromEntries([alpha, beta].map((cwd, index) => [cwd, {
    root: cwd, branch: index ? "feature-beta" : "feature-alpha", head: index ? "bbb" : "aaa",
    raw: { url, state: "OPEN", headRefName: index ? "feature-beta" : "feature-alpha", headRefOid: index ? "bbb" : "aaa", comments: [], reviews: [], statusCheckRollup: [] }, pages: [[]],
  }])) };
  const tools = await installFakePrTools(f, data);
  const tui = await startTuiPty(t, f);
  const alphaDriver = await joinCheckout(t, f, "alpha", "w1:p1", alpha);
  const betaDriver = await joinCheckout(t, f, "beta", "w1:p2", beta);
  await eventually(() => tui.output().includes("alpha") && tui.output().includes("beta"), 10000)
    .catch(error => { throw new Error(`${error}\n${tui.output()}`); });

  const openedAt = tui.output().length;
  tui.send("p");
  await eventually(() => tui.output().slice(openedAt).includes("Select PR checkout"), 10000)
    .catch(error => { throw new Error(`${error}\n${tui.output()}`); });
  const picker = tui.output().slice(openedAt);
  assert.ok(picker.includes("alpha"));
  assert.ok(picker.includes("beta"));
  tui.send("\x1b[B\r");
  const resolved = await eventually(async () => {
    const snapshot = await sync(alphaDriver, false, "working");
    return snapshot.automation.pr?.url === url ? snapshot : undefined;
  }, 10000);
  assert.equal(resolved.automation.pr.driver, "beta");
  const betaRoot = await realpath(beta);
  assert.equal(resolved.automation.pr.url, url);
  const callsBefore = await tools.calls();
  assert.ok(callsBefore.every(call => call.cwd === betaRoot), JSON.stringify(callsBefore));
  const lookups = callsBefore.filter(call => call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] === "--json");
  assert.equal(lookups.length, 1);
  assert.equal(lookups[0].args.includes(url), false);
  assert.ok(callsBefore.some(call => call.name === "gh" && call.args[2] === url), "resolved poll uses the pinned URL");

  const pinnedCount = callsBefore.filter(call => call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] === url).length;
  await betaDriver.request({ op: "mode", mode: "pr", enabled: false });
  await betaDriver.request({ op: "mode", mode: "pr", enabled: true });
  await eventually(async () => (await tools.calls()).filter(call =>
    call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] === url).length > pinnedCount);
  const callsAfter = await tools.calls();
  assert.equal(callsAfter.filter(call => call.name === "gh" && call.args[0] === "pr" && call.args[1] === "view" && call.args[2] === "--json").length, 1,
    "later polls never rediscover or silently retarget");
  tui.send("q");
  await Promise.race([tui.exited, delay(5000).then(() => { throw new Error("PTY TUI did not quit"); })]);
});

test("TUI opens its live space, restores alternate screen and uses terminal-native colors", { timeout: 15000, skip: !["darwin", "linux"].includes(process.platform) }, async t => {
  const f = await fixture(t);
  // Live pane identity must win over inherited/stale workspace environment.
  f.panes.set("w1:p0", { pane_id: "w1:p0", workspace_id: "w2", agent: null });
  const project = `${f.directory}/pty`; await mkdir(project);
  await writeFile(`${project}/pyproject.toml`, '[project]\nname = "mustr-pty-test"\nversion = "0.0.0"\nrequires-python = ">=3.9"\ndependencies = []\n');
  const child = spawn("uv", ["run", "--project", project, fileURLToPath(new URL("pty-smoke.py", import.meta.url)), binary], { env: { ...f.env, HERDR_WORKSPACE_ID: "w1", MUSTR_TEST_SPACE_LABEL: "Second", TERM: "xterm-256color" }, stdio: ["pipe", "pipe", "pipe"] });
  f.track(child);
  let output = ""; child.stdout.on("data", b => output += b); child.stderr.on("data", b => output += b);
  const [code] = await once(child, "exit");
  assert.equal(code, 0, output);
  assert.ok(output.includes("\x1b[?1049h"), "entered alternate screen");
  assert.ok(output.includes("\x1b[?1049l"), "restored alternate screen");
  assert.ok(!output.includes(";2;"), "no hard-coded truecolor palette");
});
