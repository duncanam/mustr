import test from "node:test";
import assert from "node:assert/strict";
import net from "node:net";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { HubClient } from "../pi/transport.mjs";
import { binary, fixture } from "./support.mjs";

// Only this process's fake herdr, temporary DB/config and forked PTY are used.
// This covers HUMAN confirmation + zero-connected-agent hub clearing, NOT a real
// Pi context reset. The SDK and adapter suites own fresh-session verification.
function controller(child) {
  let sequence = 0, buffer = "", stderr = "";
  const pending = new Map();
  child.stderr.on("data", chunk => { stderr += chunk; });
  const rejectAll = error => {
    for (const p of pending.values()) { clearTimeout(p.timer); p.reject(error); }
    pending.clear();
  };
  child.on("error", rejectAll);
  child.stdin.on("error", rejectAll);
  child.on("exit", (code, signal) => {
    rejectAll(new Error(`PTY controller exited (${code ?? signal}): ${stderr}`));
  });
  child.stdout.on("data", chunk => {
    buffer += chunk;
    while (buffer.includes("\n")) {
      const end = buffer.indexOf("\n");
      const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
      let response;
      try { response = JSON.parse(line); }
      catch { rejectAll(new Error(`Invalid PTY response: ${line}\n${stderr}`)); return; }
      const request = pending.get(response.id);
      if (!request) continue;
      pending.delete(response.id); clearTimeout(request.timer);
      if (response.error) request.reject(new Error(response.error));
      else request.resolve(response.result);
    }
  });
  return command => new Promise((resolve, reject) => {
    if (child.exitCode !== null || child.signalCode !== null || child.stdin.destroyed) {
      reject(new Error(`PTY controller is not running: ${stderr}`)); return;
    }
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`PTY request timed out: ${command.op}\n${stderr}`)); }, 9000);
    pending.set(id, { resolve, reject, timer });
    child.stdin.write(JSON.stringify({ ...command, id }) + "\n");
  });
}

async function forbiddenSocketOperation(socketPath, op) {
  const socket = net.createConnection(socketPath);
  socket.setTimeout(2000, () => socket.destroy(new Error("Schema rejection timed out")));
  try {
    await once(socket, "connect");
    let response = "";
    const result = new Promise((resolve, reject) => {
      socket.on("error", reject);
      socket.on("end", () => { if (!response.includes("\n")) reject(new Error("Missing schema rejection")); });
      socket.on("data", chunk => {
        response += chunk;
        if (response.includes("\n")) {
          try { resolve(JSON.parse(response.split("\n")[0])); } catch (error) { reject(error); }
        }
      });
    });
    socket.write(JSON.stringify({ id: "not-a-human-reset", op, space_id: "w1" }) + "\n");
    const rejected = await result;
    assert.equal(rejected.ok, false);
    assert.match(rejected.error, /schema|invalid request/i);
  } finally { socket.destroy(); }
}

function scoped(state, space) {
  return {
    tasks: state.tasks.filter(task => task.space_id === space),
    messages: state.messages.filter(message => message.space_id === space),
    automation: state.automation[space],
  };
}

test("TUI reset: empty Enter/Esc preserve history; typed RESET alone clears exact hub space", {
  timeout: 30000, skip: !["darwin", "linux"].includes(process.platform),
}, async t => {
  const f = await fixture(t);
  f.panes.set("w2:p2", { pane_id: "w2:p2", workspace_id: "w2", agent: "pi" });
  const project = `${f.directory}/reset-pty`; await mkdir(project);
  await writeFile(`${project}/pyproject.toml`, '[project]\nname = "mustr-reset-pty-test"\nversion = "0.0.0"\nrequires-python = ">=3.9"\ndependencies = []\n');
  const child = spawn("uv", ["run", "--project", project,
    fileURLToPath(new URL("reset-pty.py", import.meta.url)), binary,
    `${f.env.MUSTR_STATE_DIR}/${f.discovery.server_key}/hub.sqlite`,
  ], { env: { ...f.env, TERM: "xterm-256color" }, stdio: ["pipe", "pipe", "pipe"] });
  f.track(child);
  const exited = new Promise(resolve => {
    child.once("exit", (code, signal) => resolve([code, signal]));
    child.once("error", error => resolve([null, error.message]));
  });
  const request = controller(child);
  const screen = (keys, contains = [], absent = []) => request({ op: "screen", keys, contains, absent });
  const state = () => request({ op: "state" });
  let quit = false;
  try {
    await screen("", ["mustr", "First", "1/2"]);
    async function join(id, pane, space) {
      const client = new HubClient(); t.after(() => client.close());
      await client.connect(f.discovery.socket, { agent_id: id, name: id, role: "implementor", pane_id: pane, space_id: space });
      await client.request({ op: "sync", activity: "idle", receive: false });
      return client;
    }
    const todo = `${f.directory}/TODO.md`, otherTodo = `${f.directory}/OTHER.md`;
    const todoText = "# Fixture only\n\n- [ ] Keep this file byte-for-byte\n";
    await writeFile(todo, todoText); await writeFile(otherTodo, todoText);
    for (const [space, prefix, path] of [["w1", "target", todo], ["w2", "other", otherTodo]]) {
      const a = await join(`${prefix}-requester`, `${space}:p1`, space);
      const b = await join(`${prefix}-worker`, `${space}:p2`, space);
      await a.request({ op: "assign", to: `${prefix}-worker`, title: `${prefix} task history`, text: "Fixture data, not real work." });
      await a.request({ op: "send", to: `${prefix}-worker`, text: `${prefix} addressed history`, wake: false });
      await a.request({ op: "configure_todo", path });
      await a.request({ op: "mode", mode: "checkin", enabled: true });
      await a.request({ op: "leave" }); await b.request({ op: "leave" });
      a.close(); b.close();
    }
    // These human Service operations must never be accepted as agent protocol commands.
    for (const op of ["reset_preview", "reset_start", "reset_abandon"]) {
      await forbiddenSocketOperation(f.discovery.socket, op);
    }
    // Capture registration only: no session_start/role selection, process/socket,
    // resource discovery, model or installation. Internal slash commands aren't tools.
    const { default: adapter } = await import("../pi/index.ts");
    const tools = [];
    await adapter({ registerTool: definition => tools.push(definition.name), registerCommand() {}, on() {} });
    assert.ok(tools.includes("mustr_roster"));
    assert.ok(tools.every(name => !/reset|fresh/i.test(name)), "no model-callable reset tool");

    const before = await state();
    assert.equal(before.members.filter(member => member.space_id === "w1" && member.activity !== "offline").length, 0);
    assert.equal(scoped(before, "w1").tasks.length, 1);
    assert.equal(scoped(before, "w1").messages.length, 2);
    assert.equal(scoped(before, "w1").automation.todo.path, todo);
    assert.equal(scoped(before, "w1").automation.checkin_enabled, true);
    assert.equal(scoped(before, "w2").automation.checkin_enabled, true);
    assert.equal(before.resets.w1, undefined);
    const expectedTarget = scoped(before, "w1"), expectedOther = scoped(before, "w2");

    const confirmation = await screen("r", ["Start fresh?", "1 tasks and 2 messages", "0 connected agents only", "NOT remotely cleared", "Type RESET, then press Enter to confirm", "Confirmation: _", "Esc or empty Enter: Cancel"]);
    assert.ok(confirmation.includes("First (w1)"));
    await screen("\r", ["Messages"], ["Start fresh?"]);
    assert.deepEqual(scoped(await state(), "w1"), expectedTarget, "empty Enter must not clear or detach anything");
    assert.equal((await state()).resets.w1, undefined);

    await screen("r", ["Start fresh?"]);
    await screen("\t\x1b[Ctcpf", ["Start fresh?", "Confirmation: TCPF", "Esc or empty Enter: Cancel"]);
    await screen("\x1b", ["Messages", "First"], ["Start fresh?"]);
    assert.deepEqual(scoped(await state(), "w1"), expectedTarget, "Esc and modal keys must preserve history/modes/binding");
    assert.equal((await state()).resets.w1, undefined);

    await screen("r", ["Start fresh?", "First (w1)", "Read all details before confirming"]);
    await screen("RESET\r", ["Fresh-start progress", "State: Completed"]);
    const after = await state();
    assert.deepEqual(scoped(after, "w1").tasks, []);
    assert.deepEqual(scoped(after, "w1").messages, []);
    const automation = scoped(after, "w1").automation;
    assert.equal(automation.todo, null);
    assert.equal(automation.pr, null);
    assert.equal(automation.todo_enabled, false);
    assert.equal(automation.checkin_enabled, false);
    assert.equal(automation.pr_enabled, false);
    assert.deepEqual(after.resets.w1, { state: "completed", participants: 0 });
    assert.deepEqual(scoped(after, "w2"), expectedOther, "other-space task/message/TODO history is untouched");
    assert.equal(await readFile(todo, "utf8"), todoText);
    assert.equal(await readFile(otherTodo, "utf8"), todoText);
    await screen("\x1b", ["Messages"], ["Fresh-start progress"]);
    const exit = await request({ op: "quit" }); quit = true;
    assert.deepEqual(exit, { code: 0, entered: true, restored: true });
    assert.deepEqual(await exited, [0, null]);
  } finally {
    if (!quit && child.exitCode === null) {
      // Only this fixture-owned PTY. Escape cancels/closes any test dialog first.
      try { await screen("\x1b", [], ["Start fresh?", "Fresh-start progress"]); await request({ op: "quit" }); } catch { child.kill("SIGTERM"); }
    }
    child.stdin.end();
  }
});
