import test from "node:test";
import assert from "node:assert/strict";
import { access, chmod, mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import net from "node:net";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";
import { DEFAULT_PANE_TABLE, binary, delay, eventually, exec, fixture } from "./support.mjs";

const ptyController = fileURLToPath(new URL("./profile-startup-pty.py", import.meta.url));
const fakeHerdrSource = String.raw`#!/usr/bin/env node
const fs = require("node:fs");
const net = require("node:net");
const crypto = require("node:crypto");

const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const read = path => { try { return fs.readFileSync(path, "utf8"); } catch { return ""; } };
const write = (path, value) => fs.writeFileSync(path, value);
const append = (path, value) => fs.appendFileSync(path, value + "\n");
const args = process.argv.slice(2);
const mode = process.env.MUSTR_TEST_HERDR_MODE || "join";
if (args[0] === "agent") {
  process.on("exit", () => write(process.env.MUSTR_TEST_AGENT_EXITED, "exited"));
}
const safeArgs = args.map(arg => arg.startsWith("--mustr-startup=") ? "--mustr-startup=[REDACTED]" : arg);
append(process.env.MUSTR_TEST_HERDR_LOG, JSON.stringify({ args: safeArgs, socket: process.env.HERDR_SOCKET_PATH, cwd: process.cwd() }));

function bridgeJoin(request) {
  return new Promise((resolve, reject) => {
    const control = net.createConnection({ host: process.env.MUSTR_TEST_BRIDGE_HOST, port: Number(process.env.MUSTR_TEST_BRIDGE_PORT) });
    let buffer = "";
    control.on("connect", () => control.write(JSON.stringify(request) + "\n"));
    control.on("data", chunk => {
      buffer += chunk;
      if (!buffer.includes("\n")) return;
      const response = JSON.parse(buffer.slice(0, buffer.indexOf("\n")));
      control.destroy();
      if (response.ok) resolve();
      else reject(new Error("fixture participant Join was not admitted"));
    });
    control.on("error", error => reject(new Error("fixture Join bridge failed: " + (error.code || "unknown"))));
  });
}

if (args[0] === "--version") {
  process.stdout.write(mode === "wrong-version" ? "herdr 0.9.4\n" : "herdr 0.9.5\n");
} else if (args[0] === "tab" && args[1] === "create") {
  if (mode === "tab-error") {
    process.stderr.write(JSON.stringify({ id: "opaque", error: { code: "tab_error", message: "fixture rejection" } }) + "\n");
    process.exitCode = 1;
  } else {
    const roots = JSON.parse(process.env.MUSTR_TEST_ROOT_PANES || "[]");
    const next = Number(read(process.env.MUSTR_TEST_TAB_COUNTER) || "0");
    write(process.env.MUSTR_TEST_TAB_COUNTER, String(next + 1));
    const pane_id = roots[next] || "w1:p" + (90 + next);
    const workspace_id = args[args.indexOf("--workspace") + 1];
    process.stdout.write(JSON.stringify({
      id: "opaque",
      result: {
        type: "tab_created",
        tab: { id: "tab-" + next },
        root_pane: { pane_id, workspace_id, terminal_id: "terminal-" + next, tab_id: "tab-" + next },
      },
    }) + "\n");
  }
} else if (args[0] === "agent" && args[1] === "start") {
  (async () => {
    const pane_id = args[args.indexOf("--pane") + 1];
    const startupArg = args.slice(args.indexOf("--") + 1).find(arg => arg.startsWith("--mustr-startup="));
    if (!startupArg) throw new Error("fixture startup argument missing");
    const grant = JSON.parse(startupArg.slice("--mustr-startup=".length));
    const herdrName = args[2];
    const suffix = crypto.randomBytes(4).toString("hex");
    const participantName = grant.role.slice(0, 35) + "-" + suffix;
    const request = {
      id: "fixture-" + suffix,
      op: "join",
      version: 1,
      agent_id: "fixture-" + crypto.randomUUID(),
      name: participantName,
      role: grant.role,
      pane_id,
      space_id: process.env.MUSTR_TEST_SPACE_ID || "w1",
      startup_grant: { version: 1, launch_id: grant.launch_id, token: grant.token },
    };
    if (mode === "hold-agent") {
      write(process.env.MUSTR_TEST_AGENT_READY, "ready");
      while (!read(process.env.MUSTR_TEST_AGENT_RELEASE).includes("release")) await delay(10);
    } else if (mode === "hold-agent-invalidation") {
      write(process.env.MUSTR_TEST_AGENT_READY, "ready");
      while (!read(process.env.MUSTR_TEST_AGENT_ATTEMPT).includes("attempt")) await delay(10);
    }
    if (mode !== "duplicate") {
      await bridgeJoin({ op: "pane-agent", pane_id, agent: "pi" });
    }
    let admitted = false;
    if (mode !== "no-join" && mode !== "duplicate") {
      if (mode === "hold-agent-invalidation") {
        try {
          await bridgeJoin(request);
          admitted = true;
        } catch {}
        write(process.env.MUSTR_TEST_AGENT_ATTEMPT_RESULT, admitted ? "joined" : "rejected");
        while (!read(process.env.MUSTR_TEST_AGENT_RELEASE).includes("release")) await delay(10);
        write(process.env.MUSTR_TEST_AGENT_DONE, "done");
        if (!admitted) {
          process.stderr.write(JSON.stringify({ id: "opaque", error: { code: "agent_not_ready", message: "fixture grant was retired before Join" } }) + "\n");
          process.exitCode = 1;
          return;
        }
      } else {
        await bridgeJoin(request);
        admitted = true;
      }
    }
    if (mode === "duplicate") {
      process.stderr.write(JSON.stringify({ id: "opaque", error: { code: "agent_name_taken", message: "fixture duplicate" } }) + "\n");
      process.exitCode = 1;
      return;
    }
    if (mode === "not-ready") {
      process.stderr.write(JSON.stringify({ id: "opaque", error: { code: "agent_not_ready", message: "fixture queued Pi is not ready yet" } }) + "\n");
      process.exitCode = 1;
      return;
    }
    const index = Number(process.env.MUSTR_TEST_TAB_COUNTER) - 1;
    process.stdout.write(JSON.stringify({
      id: "opaque",
      result: {
        type: "agent_started",
        agent: { name: herdrName, agent: "pi", pane_id, terminal_id: "terminal-" + index, workspace_id: process.env.MUSTR_TEST_SPACE_ID || "w1", tab_id: "tab-" + index },
        argv: [],
      },
    }) + "\n");
  })().catch(error => {
    append(process.env.MUSTR_TEST_HERDR_ERRORS, error.message);
    process.stderr.write("fixture Herdr failure: " + error.message);
    process.exitCode = 1;
  });
} else {
  process.stderr.write("unexpected fixture Herdr command");
  process.exitCode = 99;
}
`;

async function installJoinBridge(t, f) {
  const members = [];
  const joinResults = [];
  const rejections = [];
  const startupTokens = [];
  const sockets = new Set();
  const server = net.createServer(control => {
    let buffer = "";
    control.on("error", () => {});
    control.on("data", chunk => {
      buffer += chunk;
      if (!buffer.includes("\n")) return;
      const request = JSON.parse(buffer.slice(0, buffer.indexOf("\n")));
      if (request.op === "join" && typeof request.startup_grant?.token === "string") {
        startupTokens.push(request.startup_grant.token);
      }
      if (request.op === "pane-agent") {
        const pane = f.panes.get(request.pane_id);
        if (pane) f.panes.set(request.pane_id, { ...pane, agent: request.agent });
        control.end(JSON.stringify({ ok: Boolean(pane) }) + "\n");
        return;
      }
      const hub = net.createConnection(f.discovery.socket);
      sockets.add(hub);
      hub.on("close", () => sockets.delete(hub));
      hub.on("error", () => {});
      let hubBuffer = "";
      hub.on("connect", () => hub.write(JSON.stringify(request) + "\n"));
      hub.on("data", data => {
        hubBuffer += data;
        if (!hubBuffer.includes("\n")) return;
        const response = JSON.parse(hubBuffer.slice(0, hubBuffer.indexOf("\n")));
        if (response.ok) {
          members.push({ role: request.role, pane_id: request.pane_id, space_id: request.space_id });
          joinResults.push(response.result);
        } else rejections.push(response.error || response);
        control.end(JSON.stringify({ ok: response.ok }) + "\n");
        if (!response.ok) hub.destroy();
      });
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  t.after(async () => {
    for (const socket of sockets) socket.destroy();
    await new Promise(resolve => server.close(() => resolve()));
  });
  return { host: "127.0.0.1", port: address.port, members, joinResults, rejections, startupTokens };
}

async function fakeHerdr(t, f, bridge, { roots = ["w1:p9", "w1:p10", "w1:p11"], mode = "join" } = {}) {
  const directory = join(f.directory, "fake-herdr");
  const bin = join(directory, "bin");
  await mkdir(bin, { recursive: true });
  const executable = join(bin, "herdr");
  await writeFile(executable, fakeHerdrSource);
  await chmod(executable, 0o700);
  const log = join(directory, "argv.redacted.jsonl");
  const errors = join(directory, "fixture-errors.log");
  const counter = join(directory, "tab-counter");
  const agentReady = join(directory, "agent-ready");
  const agentRelease = join(directory, "agent-release");
  const agentAttempt = join(directory, "agent-attempt");
  const agentAttemptResult = join(directory, "agent-attempt-result");
  const agentDone = join(directory, "agent-done");
  const agentExited = join(directory, "agent-exited");
  await Promise.all([log, errors, counter, agentReady, agentRelease, agentAttempt, agentAttemptResult, agentDone, agentExited].map(path => writeFile(path, "")));
  const env = {
    ...f.env,
    PATH: `${bin}${delimiter}${process.env.PATH || ""}`,
    MUSTR_TEST_HERDR_LOG: log,
    MUSTR_TEST_HERDR_ERRORS: errors,
    MUSTR_TEST_HERDR_MODE: mode,
    MUSTR_TEST_ROOT_PANES: JSON.stringify(roots),
    MUSTR_TEST_TAB_COUNTER: counter,
    MUSTR_TEST_SPACE_ID: "w1",
    MUSTR_TEST_BRIDGE_HOST: bridge.host,
    MUSTR_TEST_BRIDGE_PORT: String(bridge.port),
    MUSTR_TEST_AGENT_READY: agentReady,
    MUSTR_TEST_AGENT_RELEASE: agentRelease,
    MUSTR_TEST_AGENT_ATTEMPT: agentAttempt,
    MUSTR_TEST_AGENT_ATTEMPT_RESULT: agentAttemptResult,
    MUSTR_TEST_AGENT_DONE: agentDone,
    MUSTR_TEST_AGENT_EXITED: agentExited,
  };
  f.beforeCleanup(async () => {
    await writeFile(agentAttempt, "attempt");
    await writeFile(agentRelease, "release");
    if (mode === "hold-agent-invalidation" && (await readFile(agentReady, "utf8")).includes("ready")) {
      await eventually(async () => (await readFile(agentExited, "utf8")).includes("exited"), 15000);
    }
  });
  return {
    env,
    log,
    errors,
    counter,
    agentReady,
    agentRelease,
    agentAttempt,
    agentAttemptResult,
    agentDone,
    agentExited,
    cleanupWithTui(tui) {
      f.beforeCleanup(async () => {
        await writeFile(agentAttempt, "attempt");
        await writeFile(agentRelease, "release");
        if (tui.child.exitCode === null && tui.child.signalCode === null) {
          try { tui.send("q"); } catch {}
          await eventually(() => tui.child.exitCode !== null || tui.child.signalCode !== null, 15000);
        }
      });
    },
    async records() {
      const text = await readFile(log, "utf8");
      return text.trim() ? text.trim().split("\n").map(line => JSON.parse(line)) : [];
    },
    async reset() {
      await Promise.all([writeFile(log, ""), writeFile(counter, "0")]);
    },
  };
}

async function assertNoStartupTokenLeak(bridge, fake, tui, extraDiagnostics = []) {
  assert.ok(bridge.startupTokens.length > 0, "the test observed actual issued-token Join traffic");
  assert.ok(
    bridge.startupTokens.every(token => typeof token === "string" && token.length > 0),
    "every observed startup grant has a nonempty token",
  );
  const diagnostics = [
    tui.output(),
    tui.screen(),
    JSON.stringify(await fake.records()),
    await readFile(fake.errors, "utf8"),
    JSON.stringify(bridge.rejections),
    ...extraDiagnostics,
  ].join("\n");
  for (const token of bridge.startupTokens) {
    assert.ok(!diagnostics.includes(token), "an actual issued startup token appeared in diagnostic output");
  }
}

async function startPty(t, f, args, env, cwd = f.directory) {
  const project = join(f.directory, "uv-profile-pty");
  await mkdir(project, { recursive: true });
  await writeFile(join(project, "pyproject.toml"), '[project]\nname = "mustr-profile-pty-test"\nversion = "0.0.0"\nrequires-python = ">=3.9"\ndependencies = []\n');
  const child = spawn("uv", ["run", "--project", project, ptyController, binary, ...args], {
    cwd,
    env: { ...env, TERM: "xterm-256color", MUSTR_TEST_PTY_TIMEOUT: "45" },
    stdio: ["pipe", "pipe", "pipe"],
  });
  f.track(child);
  let output = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", chunk => { output += chunk; });
  child.stderr.on("data", chunk => { output += chunk; });
  const exited = once(child, "exit");
  const ready = await eventually(() => {
    if (child.exitCode !== null || child.signalCode !== null) return "exited";
    return output.includes("mustr  /  agent coordination") ? "ready" : false;
  }, 20000);
  if (ready !== "ready") throw new Error(`profile PTY exited early: ${output}`);
  return {
    child,
    exited,
    output: () => output,
    screen: () => {
      const start = output.lastIndexOf("__MSTR_SCREEN__\n");
      if (start < 0) return "";
      const content = start + "__MSTR_SCREEN__\n".length;
      const end = output.indexOf("\n__MSTR_END_SCREEN__", content);
      return end < 0 ? "" : output.slice(content, end);
    },
    send(key) { child.stdin.write(`${key}\n`); },
  };
}

async function stopPty(tui) {
  if (tui.child.exitCode !== null || tui.child.signalCode !== null) return;
  tui.send("QUIT");
  await Promise.race([
    tui.exited,
    delay(10000).then(() => { throw new Error("profile PTY did not exit after q"); }),
  ]);
}

async function waitForJoinedScreen(tui, roles) {
  await eventually(() => {
    const screen = tui.screen();
    const rows = screen.split("\n").filter(line => /^\s*\d+\/\d+\s/.test(line));
    if (rows.slice(0, roles.length).some(line => line.includes("uncertain"))) {
      throw new Error(`profile entry did not reconcile:\n${screen}`);
    }
    return rows.length >= roles.length && roles.every((role, index) =>
      rows[index].includes(role) && rows[index].includes("joined")
    );
  }, 15000);
}

function operation(record) {
  if (record.args[0] === "--version") return "version";
  if (record.args[0] === "tab") return "tab";
  if (record.args[0] === "agent") return "agent";
  return record.args.join(" ");
}

test("profile startup launches ordered repeated role slots serially and reports each admitted result", { timeout: 90000 }, async t => {
  const paneTable = [
    ...DEFAULT_PANE_TABLE,
    ...["w1:p9", "w1:p10", "w1:p11"].map(pane_id => [pane_id, { pane_id, workspace_id: "w1", agent: null }]),
  ];
  const f = await fixture(t, { paneTable });
  const workdir = join(f.directory, "working directory=one");
  await mkdir(workdir);
  const resolvedWorkdir = await realpath(workdir);
  const resolvedConfig = await realpath(join(f.directory, "config"));
  const bridge = await installJoinBridge(t, f);
  const profileRoles = ["reviewer", "researcher", "reviewer"];
  await writeFile(join(f.directory, "config", "profiles.toml"), `[profiles.team]\nroles = [${profileRoles.map(role => `"${role}"`).join(", ")}]\n`);
  const fake = await fakeHerdr(t, f, bridge);
  const tui = await startPty(t, f, ["--profile", "team"], fake.env, workdir);
  try {
    await waitForJoinedScreen(tui, ["reviewer", "researcher", "reviewer"]);
  } catch (error) {
    throw new Error(`${error.message}\nHerdr records: ${JSON.stringify(await fake.records())}\nHerdr errors: ${await readFile(fake.errors, "utf8")}\nJoin bridge: ${JSON.stringify(bridge.members)}\nJoin rejections: ${JSON.stringify(bridge.rejections)}\nLast screen: ${tui.screen()}`);
  }
  const screen = tui.screen();
  assert.match(screen, /1\/3|1 of 3/);
  assert.match(screen, /joined/i);
  assert.ok(screen.includes("reviewer") && screen.includes("researcher"), screen);
  assert.equal((await fake.records()).filter(record => record.args[0] === "agent").length, 3);
  assert.deepEqual(bridge.members.map(member => member.role), profileRoles);
  assert.deepEqual(
    bridge.joinResults.map(result => result?.startup_grant_protocol),
    Array(profileRoles.length).fill(1),
    "the real socket acknowledges the startup-grant protocol only after admitted startup Joins",
  );
  await stopPty(tui);

  const records = await fake.records();
  assert.deepEqual(records.map(operation), ["version", "tab", "agent", "tab", "agent", "tab", "agent"]);
  assert.ok(records.every(record => record.socket === f.env.HERDR_SOCKET_PATH), "all CLI commands use the captured Herdr socket");
  const tabs = records.filter(record => record.args[0] === "tab");
  for (const tab of tabs) {
    assert.deepEqual(tab.args, [
      "tab", "create", "--workspace", "w1", "--cwd", resolvedWorkdir,
      "--env", `MUSTR_BIN=${binary}`,
      "--env", `MUSTR_CONFIG_DIR=${resolvedConfig}`,
    ]);
    assert.ok(!tab.args.includes("--focus") && !tab.args.includes("--no-focus"));
    assert.ok(!tab.args.some(arg => ["--session", "--machine", "--remote"].includes(arg)));
  }
  const agents = records.filter(record => record.args[0] === "agent");
  assert.deepEqual(agents.map(record => record.args.slice(3, 8)), [
    ["--kind", "pi", "--pane", "w1:p9", "--timeout"],
    ["--kind", "pi", "--pane", "w1:p10", "--timeout"],
    ["--kind", "pi", "--pane", "w1:p11", "--timeout"],
  ]);
  assert.ok(agents.every(record => record.args[8] === "30000" && record.args[9] === "--"));
  assert.ok(agents.every(record => record.args[10] === "--mustr-startup=[REDACTED]"));
  const herdrNames = agents.map(record => record.args[2]);
  assert.equal(new Set(herdrNames).size, 3);
  assert.ok(herdrNames.every(name => /^[a-z][a-z0-9_-]{0,31}$/.test(name)));
  await assertNoStartupTokenLeak(bridge, fake, tui);
});

test("dashboard remains responsive during a held agent command and reports ordered admissions", { timeout: 90000 }, async t => {
  const f = await fixture(t);
  for (const pane_id of ["w1:p9", "w1:p10"]) {
    f.panes.set(pane_id, { pane_id, workspace_id: "w1", agent: null });
  }
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["researcher", "reviewer"]\n');
  const fake = await fakeHerdr(t, f, bridge, { roots: ["w1:p9", "w1:p10"], mode: "hold-agent" });
  const tui = await startPty(t, f, ["--profile", "team"], fake.env);
  await eventually(async () => (await readFile(fake.agentReady, "utf8")).includes("ready"), 15000);
  const beforeHelp = tui.output().length;
  tui.send("?");
  await eventually(() => tui.output().slice(beforeHelp).includes("Keyboard"), 5000);
  await writeFile(fake.agentRelease, "release");
  await waitForJoinedScreen(tui, ["researcher", "reviewer"]);
  await stopPty(tui);

  const records = await fake.records();
  assert.deepEqual(records.map(operation), ["version", "tab", "agent", "tab", "agent"]);
  assert.deepEqual(bridge.members.map(member => member.role), ["researcher", "reviewer"]);
  await assertNoStartupTokenLeak(bridge, fake, tui);
});

test("quit retires the held agent grant before allowing a late Join and keeps the CLI owned", { timeout: 90000 }, async t => {
  const f = await fixture(t);
  for (const pane_id of ["w1:p9", "w1:p10"]) {
    f.panes.set(pane_id, { pane_id, workspace_id: "w1", agent: null });
  }
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["researcher", "reviewer"]\n');
  const fake = await fakeHerdr(t, f, bridge, { roots: ["w1:p9", "w1:p10"], mode: "hold-agent-invalidation" });
  const tui = await startPty(t, f, ["--profile", "team"], fake.env);
  fake.cleanupWithTui(tui);
  await eventually(async () => (await readFile(fake.agentReady, "utf8")).includes("ready"), 15000);

  const beforeCancel = tui.output().length;
  tui.send("q");
  try {
    await eventually(() => tui.screen().includes("cancel requested"), 5000);
  } catch (error) {
    throw new Error(`${error.message}\nPTY output: ${tui.output().slice(beforeCancel)}`);
  }
  assert.equal(tui.child.exitCode, null, "the in-flight mutation must remain owned after dashboard cancellation");
  assert.deepEqual((await fake.records()).map(operation), ["version", "tab", "agent"]);

  await writeFile(fake.agentAttempt, "attempt");
  const admission = await eventually(async () => {
    const result = (await readFile(fake.agentAttemptResult, "utf8")).trim();
    return result || false;
  }, 10000);
  assert.equal(admission, "rejected", "the revoked grant must reject Join before the held CLI is released");
  assert.deepEqual(bridge.members, [], "no late member is admitted after cancellation");
  assert.equal(tui.child.exitCode, null, "the command remains held after its Join attempt");
  assert.equal((await readFile(fake.agentDone, "utf8")).trim(), "");
  assert.deepEqual((await fake.records()).map(operation), ["version", "tab", "agent"]);

  await writeFile(fake.agentRelease, "release");
  await Promise.race([
    tui.exited,
    delay(15000).then(() => { throw new Error("cancelled profile coordinator did not drain its held Herdr command"); }),
  ]);
  const records = await fake.records();
  assert.deepEqual(records.map(operation), ["version", "tab", "agent"]);
  assert.deepEqual(bridge.members, []);
  assert.equal((await readFile(fake.agentDone, "utf8")).trim(), "done", "test-owned child completed after explicit release");
  await eventually(async () => (await readFile(fake.agentExited, "utf8")).trim() === "exited", 15000);
  assert.ok(tui.output().includes("not attempted after launch cancelled"), tui.output());
  await assertNoStartupTokenLeak(bridge, fake, tui);
});

test("agent_not_ready followed by authoritative admission halts later profile mutations", { timeout: 90000 }, async t => {
  const f = await fixture(t);
  f.panes.set("w1:p9", { pane_id: "w1:p9", workspace_id: "w1", agent: null });
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["researcher", "reviewer"]\n');
  const fake = await fakeHerdr(t, f, bridge, { mode: "not-ready" });
  const tui = await startPty(t, f, ["--profile", "team"], fake.env);
  await waitForJoinedScreen(tui, ["researcher"]);
  await eventually(() => tui.screen().includes("not attempted"), 10000);
  await stopPty(tui);

  assert.deepEqual((await fake.records()).map(operation), ["version", "tab", "agent"]);
  assert.deepEqual(bridge.members.map(member => [member.role, member.pane_id]), [["researcher", "w1:p9"]]);
});

test("zero-participant reset during a held agent start stops later slots after the epoch change", { timeout: 90000 }, async t => {
  const f = await fixture(t);
  for (const pane_id of ["w1:p9", "w1:p10"]) {
    f.panes.set(pane_id, { pane_id, workspace_id: "w1", agent: null });
  }
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["researcher", "reviewer"]\n');
  const fake = await fakeHerdr(t, f, bridge, { roots: ["w1:p9", "w1:p10"], mode: "hold-agent" });
  const tui = await startPty(t, f, ["--profile", "team"], fake.env);
  await eventually(async () => (await readFile(fake.agentReady, "utf8")).includes("ready"), 15000);

  tui.send("r");
  await eventually(() => tui.screen().includes("Start fresh?"), 5000);
  assert.match(tui.screen(), /0 connected agents only/i);
  tui.send("RESET<ENTER>");
  try {
    await eventually(() => /State: Completed/i.test(tui.screen()), 10000);
  } catch (error) {
    throw new Error(`${error.message}\nLast screen:\n${tui.screen()}`);
  }
  assert.deepEqual((await fake.records()).map(operation), ["version", "tab", "agent"]);

  await writeFile(fake.agentRelease, "release");
  await eventually(() => tui.screen().includes("not attempted after reset epoch changed"), 15000);
  const screen = tui.screen();
  assert.match(screen, /uncertain/i);
  assert.ok(!screen.includes("w1:p10"), "later slot must not create a tab or pane");
  assert.deepEqual((await fake.records()).map(operation), ["version", "tab", "agent"]);
  assert.deepEqual(bridge.members, [], "reset retired the outstanding startup grant before its delayed Join");
  assert.equal(bridge.rejections.length, 1);
  assert.match(JSON.stringify(bridge.rejections), /startup grant/i);
  await assertNoStartupTokenLeak(bridge, fake, tui);
  tui.send("<ESC>");
  await eventually(() => !tui.screen().includes("Fresh-start status"), 5000);
  await stopPty(tui);
});

test("terminal and Herdr capability preflight failures create no launch mutations", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["researcher"]\n');
  const fake = await fakeHerdr(t, f, bridge, { mode: "wrong-version" });
  let capabilityError;
  try {
    await exec(binary, ["--profile", "team"], { env: fake.env });
  } catch (error) {
    capabilityError = error;
  }
  assert.ok(capabilityError, "unsupported Herdr version is a pre-dashboard error");
  assert.match(`${capabilityError.stderr || ""}\n${capabilityError.stdout || ""}`, /0\.9\.5|version/i);
  assert.deepEqual((await fake.records()).map(operation), ["version"]);
  await assert.rejects(access(f.discovery.socket), { code: "ENOENT" });
});

test("invalid profile role preflight fails before invoking Herdr or opening the hub", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["missing-profile-role"]\n');
  const fake = await fakeHerdr(t, f, bridge);
  let error;
  try {
    await exec(binary, ["--profile", "team"], { env: fake.env });
  } catch (cause) {
    error = cause;
  }
  assert.ok(error, "an unresolved selected role is a preflight error");
  assert.match(`${error.stderr || ""}\n${error.stdout || ""}`, /missing-profile-role/);
  assert.deepEqual(await fake.records(), [], "profile validation precedes even the Herdr capability check");
  assert.deepEqual(bridge.members, []);
  assert.deepEqual(bridge.startupTokens, []);
  await assert.rejects(access(f.discovery.socket), { code: "ENOENT" });
});

test("missing Herdr CLI fails profile preflight before opening a hub", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["researcher"]\n');
  const fake = await fakeHerdr(t, f, bridge);
  const emptyPath = join(f.directory, "empty-herdr-path");
  await mkdir(emptyPath);
  let error;
  try {
    await exec(binary, ["--profile", "team"], { env: { ...fake.env, PATH: emptyPath } });
  } catch (cause) {
    error = cause;
  }
  assert.ok(error, "profile startup requires the supported Herdr CLI");
  assert.match(`${error.stderr || ""}\n${error.stdout || ""}`, /Herdr CLI|herdr/i);
  assert.deepEqual(await fake.records(), [], "the unavailable executable cannot run a tab or agent command");
  assert.deepEqual(bridge.members, []);
  assert.deepEqual(bridge.startupTokens, []);
  await assert.rejects(access(f.discovery.socket), { code: "ENOENT" });
});

test("a non-terminal profile invocation fails before any Herdr launch mutation", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["researcher"]\n');
  const fake = await fakeHerdr(t, f, bridge);
  let error;
  try {
    await exec(binary, ["--profile", "team"], { env: fake.env });
  } catch (cause) {
    error = cause;
  }
  assert.ok(error, "profile startup requires a TTY");
  assert.match(`${error.stderr || ""}\n${error.stdout || ""}`, /terminal|dashboard/i);
  assert.deepEqual((await fake.records()).map(operation), ["version"]);
  await assert.rejects(access(f.discovery.socket), { code: "ENOENT" });
});

test("a second same-space owner is refused before any profile tabs are created", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  const owner = await f.start({ headless: true });
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["researcher"]\n');
  const fake = await fakeHerdr(t, f, bridge);
  let error;
  try {
    await startPty(t, f, ["--profile", "team"], fake.env);
  } catch (cause) {
    error = cause;
  }
  assert.ok(error, "singleton ownership refuses a second hub");
  assert.match(error.message, /already running|hub/i);
  assert.deepEqual((await fake.records()).map(operation), ["version"]);
  assert.equal(owner.exitCode, null);
});

test("an unverified returned root never reaches agent start or grant presentation", { timeout: 90000 }, async t => {
  const f = await fixture(t);
  const bridge = await installJoinBridge(t, f);
  await writeFile(join(f.directory, "config", "profiles.toml"), '[profiles.team]\nroles = ["reviewer"]\n');
  const fake = await fakeHerdr(t, f, bridge, { roots: ["w1:p99"] });
  const tui = await startPty(t, f, ["--profile", "team"], fake.env);
  await eventually(() => tui.screen().includes("uncertain"), 10000);
  await stopPty(tui);
  const records = await fake.records();
  assert.deepEqual(records.map(operation), ["version", "tab"]);
  assert.equal(bridge.startupTokens.length, 0, "no startup grant was issued for an unverified root");
});
