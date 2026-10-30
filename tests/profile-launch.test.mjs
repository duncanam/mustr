import test from "node:test";
import assert from "node:assert/strict";
import net from "node:net";
import { once } from "node:events";
import { randomUUID } from "node:crypto";
import { chmod, mkdir, mkdtemp, readFile, readdir, realpath, rename, rm, symlink, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, execFile } from "node:child_process";
import { createInterface } from "node:readline";
import { promisify } from "node:util";
import { binary, delay } from "./support.mjs";

const exec = promisify(execFile);
const repoRoot = dirname(dirname(fileURLToPath(import.meta.url)));

async function fixture(t, { laterSlot = false, holdFirst = false, jailFailure = false, acknowledgeFailure = false, nativeJail = false, shippedProfile = false, resourceSymlinks = false, customAgentDir = false, pendingDetections = 0 } = {}) {
  const root = await mkdtemp("/tmp/mustr-profile-launch-");
  const config = join(root, "config"), state = join(root, "state"), home = join(root, "home");
  const agentDir = customAgentDir ? join(root, "pi-agent") : join(home, ".pi/agent"), cwd = join(root, "project"), bin = join(root, "bin");
  const herdrSocket = join(root, "herdr.sock"), herdrState = join(root, "herdr-state.json");
  await Promise.all([config, state, home, cwd, bin].map(path => mkdir(path, { recursive: true, mode: 0o700 })));
  await writeFile(herdrState, JSON.stringify({ tabs: [], creates: [], runs: [], closes: [] }), { mode: 0o600 });
  const env = {
    ...process.env,
    HOME: home,
    MUSTR_CONFIG_DIR: config,
    MUSTR_STATE_DIR: state,
    HERDR_ENV: "1",
    HERDR_SOCKET_PATH: herdrSocket,
    HERDR_PANE_ID: "host-pane",
    MUSTR_TEST_HERDR_STATE: herdrState,
    MUSTR_TEST_SDK_DRIVER: join(repoRoot, "pi/test/profile-sdk-driver.mjs"),
    MUSTR_TEST_HOLD: holdFirst ? join(root, "release-first") : "",
    PATH: `${bin}:${process.env.PATH}`,
  };
  delete env.PI_CODING_AGENT_DIR;
  if (customAgentDir) env.PI_CODING_AGENT_DIR = agentDir;
  let hub;
  let herdrServer;
  t.after(async () => {
    if (hub && hub.exitCode === null) {
      hub.kill("SIGTERM");
      await Promise.race([once(hub, "exit"), delay(5000)]);
    }
    if (herdrServer?.listening) await new Promise(resolve => herdrServer.close(resolve));
    await rm(root, { recursive: true, force: true });
  });

  const herdrCli = `#!${process.execPath}
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { setTimeout as delay } from "node:timers/promises";
const statePath = process.env.MUSTR_TEST_HERDR_STATE;
const read = () => JSON.parse(readFileSync(statePath, "utf8"));
const save = value => writeFileSync(statePath, JSON.stringify(value));
const args = process.argv.slice(2);
const value = (flag, fallback) => { const index = args.indexOf(flag); return index < 0 ? fallback : args[index + 1]; };
if (args[0] === "tab" && args[1] === "list") {
  const current = read();
  if (current.failTabList) { console.error("synthetic Herdr unavailable"); process.exit(1); }
  const workspace = value("--workspace", null);
  console.log(JSON.stringify(current.tabListReply ?? { id: "fixture", result: { type: "tab_list", tabs: current.tabs.filter(tab => workspace === null || tab.workspace_id === workspace) } }));
} else if (args[0] === "tab" && args[1] === "create") {
  const current = read();
  const number = current.creates.length + 1;
  const tab = { tab_id: \`synthetic-tab-\${number}\`, workspace_id: value("--workspace", ""), pane_id: \`synthetic-space:p\${number}\`, pane_count: 1, env: {} };
  for (let index = 0; index < args.length; index++) if (args[index] === "--env") { const [key, ...rest] = args[++index].split("="); tab.env[key] = rest.join("="); }
  current.tabs.push({ tab_id: tab.tab_id, workspace_id: tab.workspace_id, pane_id: tab.pane_id, pane_count: tab.pane_count, env: tab.env });
  current.creates.push({ tab_id: tab.tab_id, pane_id: tab.pane_id, label: value("--label", ""), cwd: value("--cwd", "") });
  save(current);
  console.log(JSON.stringify({ tab: { tab_id: tab.tab_id, workspace_id: tab.workspace_id }, root_pane: { pane_id: tab.pane_id } }));
} else if (args[0] === "pane" && args[1] === "run") {
  const current = read(), pane = args[2], tab = current.tabs.find(item => item.pane_id === pane), command = args[3];
  if (!tab || !command) process.exit(3);
  current.runs.push({ pane, command }); save(current);
  if (process.env.MUSTR_TEST_HOLD && pane === "synthetic-space:p1") {
    const deadline = Date.now() + 15000;
    while (!existsSync(process.env.MUSTR_TEST_HOLD)) {
      if (Date.now() >= deadline) throw new Error("Timed out waiting for test to release first slot");
      await delay(20);
    }
  }
  const child = spawnSync("/bin/sh", ["-c", command], { stdio: "inherit", env: { ...process.env, ...tab.env, HERDR_ENV: "1", HERDR_PANE_ID: pane } });
  if (child.error) throw child.error;
  // Real pane.run acknowledges dispatch, not the eventual bootstrap exit.
  process.exit(${acknowledgeFailure} ? 0 : (child.status ?? 1));
} else if (args[0] === "tab" && args[1] === "close") {
  const current = read(); current.closes.push(args[2]); current.tabs = current.tabs.filter(tab => tab.tab_id !== args[2]); save(current);
  console.log(JSON.stringify({ closed: args[2] }));
} else { console.error("unexpected synthetic herdr arguments", args); process.exit(2); }
`;
  const herdrPath = join(bin, "herdr");
  await writeFile(herdrPath, herdrCli, { mode: 0o700 });
  await chmod(herdrPath, 0o700);
  const piPath = join(bin, "pi");
  await writeFile(piPath, `#!${process.execPath}\nimport { spawnSync } from "node:child_process";\nimport { writeFileSync } from "node:fs";\nif (process.env.HERDR_PANE_ID) writeFileSync(${JSON.stringify(root)} + "/pi-started-" + process.env.HERDR_PANE_ID, "started");\nconst result = spawnSync(process.execPath, [${JSON.stringify(join(repoRoot, nativeJail ? "pi/test/profile-cli-driver.mjs" : "pi/test/profile-sdk-driver.mjs"))}, ...process.argv.slice(2)], { stdio: "inherit", env: process.env });\nif (result.error) throw result.error; process.exit(result.status ?? 1);\n`, { mode: 0o700 });
  await chmod(piPath, 0o700);
  const jailPath = join(bin, "ai-jail");
  // Argument/environment fixture only: this does NOT claim sandbox enforcement.
  await writeFile(jailPath, `#!${process.execPath}
import { writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
const args = process.argv.slice(2);
if (args[0] === "--version") { console.log("ai-jail 2.12.0"); process.exit(0); }
writeFileSync(${JSON.stringify(join(root, "jail-invocation.json"))}, JSON.stringify(args));
if (${jailFailure}) process.exit(37);
if (${nativeJail}) {
  const separator = args.indexOf("--");
  // Hermetic test policy only; production Mustr delegates these choices.
  args.splice(separator, 0, "--no-network", "--no-toolchains", "--no-mise", "--map", ${JSON.stringify(join(repoRoot, "pi"))}, "--map", ${JSON.stringify(process.execPath)}, "--map", ${JSON.stringify(join(root, "isolation.json"))});
  const usePty = ${process.env.MUSTR_NATIVE_JAIL_PTY === "1"};
  const command = usePty ? "uv" : "/opt/homebrew/bin/ai-jail";
  const commandArgs = usePty ? ["run", "--offline", "--project", ${JSON.stringify(join(root, "jail-pty"))}, ${JSON.stringify(join(repoRoot, "tests/jail-launch-pty.py"))}, "/opt/homebrew/bin/ai-jail", ...args] : args;
  const result = spawnSync(command, commandArgs, { stdio: "inherit", env: { HOME: process.env.HOME, PATH: process.env.PATH, LANG: "C", TMPDIR: ${JSON.stringify(root)} } });
  if (result.error) throw result.error;
  process.exit(result.status ?? 1);
}
const env = { HOME: process.env.HOME, PATH: process.env.PATH, MUSTR_TEST_SDK_DRIVER: ${JSON.stringify(join(repoRoot, "pi/test/profile-sdk-driver.mjs"))} };
for (let i = 0; i < args.length; i++) if (args[i] === "--env") { const [key, ...value] = args[++i].split("="); env[key] = value.join("="); }
const command = args.slice(args.indexOf("--") + 1);
const result = spawnSync(command[0], command.slice(1), { stdio: "inherit", env });
if (result.error) throw result.error;
process.exit(result.status ?? 1);
`, { mode: 0o700 });

  if (nativeJail && process.env.MUSTR_NATIVE_JAIL_PTY === "1") {
    await mkdir(join(root, "jail-pty"));
    await writeFile(join(root, "jail-pty/pyproject.toml"), '[project]\nname = "mustr-jail-pty-test"\nversion = "0.0.0"\nrequires-python = ">=3.9"\ndependencies = []\n');
  }

  const detectionChecks = new Map();
  herdrServer = net.createServer(connection => {
    let buffer = "";
    connection.on("data", async chunk => {
      buffer += chunk;
      if (!buffer.includes("\n")) return;
      const request = JSON.parse(buffer.split("\n", 1)[0]);
      const pane = request.params.caller_pane_id;
      let agent = "pi";
      if (pendingDetections && request.method === "pane.current" && pane !== "host-pane") {
        const started = await readFile(join(root, `pi-started-${pane}`), "utf8").catch(() => "");
        const checks = detectionChecks.get(pane) ?? 0;
        agent = started && checks >= pendingDetections ? "pi" : null;
        if (started) detectionChecks.set(pane, checks + 1);
      }
      const result = request.method === "pane.current"
        ? { pane: { pane_id: pane, workspace_id: "synthetic-space", agent } }
        : request.method === "workspace.list" ? { workspaces: [{ workspace_id: "synthetic-space", label: "Synthetic" }] }
        : request.method === "pane.get" ? { pane: JSON.parse(await readFile(herdrState, "utf8")).tabs.find(tab => tab.pane_id === request.params.pane_id) ?? null } : undefined;
      connection.end(`${JSON.stringify(result === undefined ? { id: request.id, error: "unexpected method" } : { id: request.id, result })}\n`);
    });
  });
  herdrServer.listen(herdrSocket);
  await once(herdrServer, "listening");

  await exec(binary, ["install", "pi", "--agent-dir", agentDir], { env, timeout: 10000 });
  const runtimeModules = join(agentDir, "extensions/mustr/node_modules");
  await mkdir(runtimeModules, { recursive: true });
  await symlink(join(repoRoot, "pi/node_modules/@earendil-works"), join(runtimeModules, "@earendil-works"));
  await symlink(join(repoRoot, "pi/node_modules/typebox"), join(runtimeModules, "typebox"));
  if (!shippedProfile) await writeFile(join(config, "profiles.toml"), `version = 1\n\n[profiles.team]\ncwd = ${JSON.stringify(cwd)}\n\n[profiles.team.agents.architect]\ntab_name = "Architect"\nrole = "architect"\nmodel = "anthropic/vendor/mustr-profile-fixture:0"\nthinking = "off"\n${laterSlot ? '\n[profiles.team.agents.implementor]\ntab_name = "Implementor"\nrole = "implementor"\nmodel = "anthropic/vendor/mustr-profile-fixture:0"\nthinking = "off"\n' : ''}`, { mode: 0o600 });

  if (resourceSymlinks) {
    const dotfiles = join(home, "dotfiles");
    await mkdir(dotfiles);
    const target = join(dotfiles, "extensions");
    await rename(join(agentDir, "extensions"), target);
    await symlink(target, join(agentDir, "extensions"));
    // External symlink access is ai-jail policy, not a Mustr allowlist.
    await writeFile(join(home, ".ai-jail"), '[commands.pi]\nro_maps = ["~/dotfiles/extensions"]\n', { mode: 0o600 });
  }

  await writeFile(join(agentDir, "auth.json"), "{}", { mode: 0o600 });
  await mkdir(join(agentDir, "sessions"), { recursive: true });
  await writeFile(join(agentDir, "sessions/prior-fixture.jsonl"), "synthetic prior session\n");
  await mkdir(join(home, ".pi-lens"));
  await writeFile(join(home, ".pi-lens/state-sentinel"), "synthetic lens state");

  hub = spawn(binary, ["--config-dir", config, "run", "--headless"], { env, stdio: ["ignore", "ignore", "pipe"] });
  let hubError = "";
  hub.stderr.setEncoding("utf8"); hub.stderr.on("data", chunk => { hubError += chunk; });
  const contextResult = JSON.parse((await exec(binary, ["--config-dir", config, "context"], { env, timeout: 10000 })).stdout);
  const hubSocket = contextResult.socket;
  const deadline = Date.now() + 10000;
  let connected = false;
  while (Date.now() < deadline && !connected) {
    try {
      await new Promise((resolve, reject) => {
        const socket = net.createConnection(hubSocket);
        socket.once("connect", () => { socket.destroy(); resolve(); });
        socket.once("error", reject);
      });
      connected = true;
    } catch { await delay(50); }
    if (hub.exitCode !== null) throw new Error(`synthetic hub exited early: ${hubError}`);
  }
  assert.ok(connected, `synthetic hub did not become ready: ${hubError}`);
  if (nativeJail) {
    const forbiddenFiles = [join(home, "credential-sentinel"), join(config, "host-sentinel"), join(state, "host-sentinel")];
    if (resourceSymlinks) forbiddenFiles.push(join(home, "dotfiles/private-sentinel"));
    for (const path of forbiddenFiles) await writeFile(path, "synthetic forbidden sentinel", { mode: 0o600 });
    let unexpectedConnections = 0;
    const unforwarded = net.createServer(socket => { unexpectedConnections++; socket.destroy(); });
    unforwarded.listen({ host: "127.0.0.1", port: 0 }); await once(unforwarded, "listening");
    t.after(async () => { await new Promise(resolve => unforwarded.close(resolve)); assert.equal(unexpectedConnections, 0); });
    await writeFile(join(root, "isolation.json"), JSON.stringify({ forbiddenFiles, forbiddenSockets: [herdrSocket, hubSocket], unforwardedPort: unforwarded.address().port }), { mode: 0o400 });
    await writeFile(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: "https://synthetic.invalid", api: "anthropic-messages", apiKey: "synthetic-offline-key", models: [{ id: "vendor/mustr-profile-fixture:0", reasoning: false, input: ["text"], contextWindow: 100000, maxTokens: 128 }] } } }), { mode: 0o600 });
  }

  return {
    root, env, config, herdrState, agentDir, cwd, detectionChecks,
    ledgerPath: async () => {
      const launches = join(state, "profile-launches");
      const [server] = await readdir(launches);
      const [space] = await readdir(join(launches, server));
      return join(launches, server, space, "ownership.json");
    },
    stopHub: async () => {
      const ownedHub = hub;
      if (ownedHub && ownedHub.exitCode === null) {
        const exited = once(ownedHub, "exit");
        ownedHub.kill("SIGTERM");
        await exited;
      }
      hub = undefined;
    },
    run: (...args) => exec(binary, ["--config-dir", config, ...args], { env, cwd, timeout: 30000 }),
    owned: async () => JSON.parse(await readFile(herdrState, "utf8")),
    releaseFirst: () => writeFile(env.MUSTR_TEST_HOLD, "released"),
  };
}

async function confirmedStop(f, answer, beforeAnswer = async () => {}) {
  const project = join(f.root, "pty");
  await mkdir(project, { recursive: true });
  await writeFile(join(project, "pyproject.toml"), '[project]\nname = "mustr-profile-stop-test"\nversion = "0.0.0"\nrequires-python = ">=3.9"\ndependencies = []\n');
  const child = spawn("uv", ["run", "--project", project, join(repoRoot, "tests/profile-stop-pty.py"), binary, "--config-dir", f.config, "agent", "stop", "team/architect"], { env: f.env, stdio: ["pipe", "pipe", "pipe"] });
  let stderr = "";
  child.stderr.on("data", chunk => { stderr += chunk; });
  const exited = once(child, "exit");
  const lines = createInterface({ input: child.stdout });
  let result;
  try {
    for await (const line of lines) {
      const reply = JSON.parse(line);
      if (reply.prompt) {
        assert.match(reply.prompt, /Unfinished tasks and external jobs may continue/);
        await beforeAnswer();
        child.stdin.end(`${JSON.stringify({ answer })}\n`);
      } else result = reply;
    }
    assert.equal((await exited)[0], 0, stderr);
    assert.ok(result, `PTY returned no result: ${stderr}`);
    return result;
  } finally {
    lines.close();
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    await exited;
  }
}

test("profile ensure and individual start use only returned Herdr IDs and join through real Pi SDK startup", { timeout: 60000 }, async t => {
  const f = await fixture(t);
  const { env, config, herdrState, run, owned: readOwned } = f;
  const initial = await readOwned();
  initial.tabs.push({ tab_id: "unrelated-tab", workspace_id: "synthetic-space", pane_id: "unrelated-pane", pane_count: 1, label: "Architect" });
  await writeFile(herdrState, JSON.stringify(initial));
  const ensured = await run("--profile", "team");
  assert.match(ensured.stdout, /Profile team ensured/);
  assert.match((await run("--profile", "team")).stdout, /0 new agent/);
  assert.match((await run("agent", "start", "architect", "--profile", "team")).stdout, /already managed/);
  await assert.rejects(run("--profile", "team", "--jailed"), /profile drift/);
  const startedExtra = await exec(binary, ["--config-dir", config, "agent", "start", "architect", "--profile", "team", "--name", "extra"], { env, timeout: 30000 });
  assert.match(startedExtra.stdout, /team\/architect\/extra/);
  await assert.rejects(run("agent", "start", "architect", "--profile", "team", "--name", "extra"), /already exists/);
  assert.match((await run("agent", "stop", "unrelated")).stdout, /not managed.*nothing to stop/);
  const listed = await exec(binary, ["--config-dir", config, "agent", "list", "--profile", "team"], { env, timeout: 10000 });
  assert.match(listed.stdout, /team\/architect\s+ready\s+synthetic-tab-1\s+synthetic-space:p1/);
  assert.match(listed.stdout, /team\/architect\/extra\s+ready\s+synthetic-tab-2\s+synthetic-space:p2/);
  await assert.rejects(
    exec(binary, ["--config-dir", config, "agent", "stop", "team/architect"], { env, timeout: 10000 }),
    /interactive terminal/,
  );
  const owned = JSON.parse(await readFile(herdrState, "utf8"));
  assert.deepEqual(owned.creates.map(tab => tab.tab_id), ["synthetic-tab-1", "synthetic-tab-2"], "only IDs returned by tab creation are owned");
  assert.equal(owned.runs.length, 2);
  assert.ok(owned.runs.every(run => run.command.includes("__profile_bootstrap") && !run.command.includes("Architect")), "profile values never become shell command text");
  assert.deepEqual(owned.closes, [], "no live or fake tab was closed by this vertical-slice test");

  // A split refuses stop; removing the original root must NOT make the
  // unrelated sole remaining pane eligible for close.
  const tab = owned.tabs.find(tab => tab.tab_id === "synthetic-tab-1");
  tab.pane_count = 2;
  await writeFile(herdrState, JSON.stringify(owned));
  await assert.rejects(run("agent", "stop", "team/architect"), /exactly one pane/);
  tab.pane_count = 1;
  tab.pane_id = "synthetic-space:replacement";
  await writeFile(herdrState, JSON.stringify(owned));
  await assert.rejects(run("agent", "stop", "team/architect"), /recorded root pane/);
  assert.deepEqual((await readOwned()).closes, [], "replacement pane is never closed");

  tab.pane_id = "synthetic-space:p1";
  await writeFile(herdrState, JSON.stringify(owned));
  assert.match((await confirmedStop(f, "no")).output, /Cancelled; no tab closed/);
  assert.deepEqual((await readOwned()).closes, []);
  const changed = await confirmedStop(f, "yes", async () => {
    tab.pane_id = "synthetic-space:replacement";
    await writeFile(herdrState, JSON.stringify(owned));
  });
  assert.notEqual(changed.code, 0, "confirmation cannot override changed ownership");
  assert.deepEqual((await readOwned()).closes, []);
  tab.pane_id = "synthetic-space:p1";
  await writeFile(herdrState, JSON.stringify(owned));
  const closed = await confirmedStop(f, "yes");
  assert.equal(closed.code, 0);
  assert.match(closed.output, /did not stop external jobs/);
  assert.deepEqual((await readOwned()).closes, ["synthetic-tab-1"]);
  assert.ok((await readOwned()).tabs.some(tab => tab.tab_id === "unrelated-tab"));
  assert.doesNotMatch((await run("agent", "list")).stdout, /team\/architect\s/);
  assert.match((await run("agent", "stop", "team/architect")).stdout, /nothing to stop/);
  assert.deepEqual((await readOwned()).closes, ["synthetic-tab-1"]);
});

for (const cleanup of ["list", "stop", "start"]) {
test(`agent ${cleanup} automatically prunes absent duplicate tabs and preserves unresolved creates`, { timeout: 30000 }, async t => {
  const f = await fixture(t);
  await f.run("agent", "start", "architect", "--profile", "team");
  const file = await f.ledgerPath();
  const ledger = JSON.parse(await readFile(file, "utf8"));
  const original = ledger.records[0];
  ledger.records.unshift({ ...original, launch_id: randomUUID(), tab_id: "already-gone-tab", pane_id: "gone-pane" });
  const unresolved = { ...original, instance: "team/unknown", slot: "unknown", launch_id: randomUUID(), tab_id: null, pane_id: null, ready: false };
  ledger.records.push(unresolved);
  await writeFile(file, JSON.stringify(ledger));
  const owned = await f.owned();
  owned.tabs = [];
  await writeFile(f.herdrState, JSON.stringify(owned));
  const result = cleanup === "start"
    ? await f.run("agent", "start", "architect", "--profile", "team")
    : await f.run("agent", cleanup, ...(cleanup === "stop" ? ["team/architect"] : []));
  if (cleanup === "start") {
    const after = JSON.parse(await readFile(file, "utf8"));
    assert.equal(after.records.length, 2);
    assert.equal(after.records.filter(record => record.instance === "team/architect").length, 1);
    assert.ok(after.records.some(record => record.launch_id === unresolved.launch_id));
    assert.equal((await f.owned()).creates.length, 2);
    assert.deepEqual((await f.owned()).closes, []);
    return;
  }
  if (cleanup === "list") {
    assert.doesNotMatch(result.stdout, /team\/architect/);
    assert.match(result.stdout, /team\/unknown\s+unresolved-create/);
  } else assert.match(result.stdout, /already gone.*nothing to stop/);
  assert.deepEqual(JSON.parse(await readFile(file, "utf8")).records, [unresolved]);
  assert.match((await f.run("agent", "stop", "team/architect")).stdout, /nothing to stop/);
  await assert.rejects(f.run("agent", "stop", "team/unknown"), /unresolved tab-create/);
  assert.deepEqual((await f.owned()).closes, []);
  await f.run("agent", "start", "architect", "--profile", "team");
  const after = JSON.parse(await readFile(file, "utf8"));
  assert.equal(after.records.filter(record => record.instance === "team/architect").length, 1);
  assert.equal((await f.owned()).creates.length, 2, "only an explicit start creates a replacement");
});
}

test("cleanup retains moved tabs and all ownership on unavailable or malformed Herdr lists", { timeout: 30000 }, async t => {
  const f = await fixture(t);
  await f.run("agent", "start", "architect", "--profile", "team");
  const file = await f.ledgerPath();
  const before = await readFile(file, "utf8");
  const owned = await f.owned();
  owned.tabs[0].workspace_id = "other-space";
  await writeFile(f.herdrState, JSON.stringify(owned));
  assert.match((await f.run("agent", "list")).stdout, /team\/architect\s+out-of-space/);
  await assert.rejects(f.run("agent", "stop", "team/architect"), /no longer in this space/);
  assert.equal(await readFile(file, "utf8"), before);
  for (const reply of [{ error: "unavailable" }, { tabs: [{}] }, { tabs: [{ tab_id: "some-tab" }] }, { result: { type: "error", tabs: [] } }]) {
    owned.tabListReply = reply;
    await writeFile(f.herdrState, JSON.stringify(owned));
    await assert.rejects(f.run("agent", "list"), /ownership retained/);
    await assert.rejects(f.run("agent", "start", "architect", "--profile", "team"), /ownership retained/);
    await assert.rejects(f.run("agent", "stop", "team/architect"), /ownership retained/);
    assert.equal(await readFile(file, "utf8"), before);
  }
  delete owned.tabListReply;
  owned.failTabList = true;
  await writeFile(f.herdrState, JSON.stringify(owned));
  await assert.rejects(f.run("agent", "list"), /synthetic Herdr unavailable/);
  assert.equal(await readFile(file, "utf8"), before);
  assert.deepEqual((await f.owned()).closes, []);
  assert.equal((await f.owned()).creates.length, 1);
});

test("ensure revalidates a later slot completed by an individual start while its first slot waits", { timeout: 60000 }, async t => {
  const f = await fixture(t, { laterSlot: true, holdFirst: true });
  const ensure = f.run("--profile", "team");
  void ensure.catch(() => {});
  let result;
  try {
    const deadline = Date.now() + 10000;
    while (!(await f.owned()).runs.some(run => run.pane === "synthetic-space:p1")) {
      assert.ok(Date.now() < deadline, "ensure must reach first-slot startup barrier");
      await delay(20);
    }
    // Ensure has already planned both slots and released its ledger lock.
    await f.run("agent", "start", "implementor", "--profile", "team");
    assert.match((await f.run("agent", "list")).stdout, /team\/implementor\s+ready/);
  } finally {
    await f.releaseFirst();
    result = await ensure;
  }
  assert.match(result.stdout, /1 new agent/);
  const owned = await f.owned();
  assert.deepEqual(owned.creates.map(tab => tab.label), ["Architect", "Implementor"]);
  assert.equal(owned.runs.length, 2, "no duplicate later-slot tab or bootstrap");
  assert.match((await f.run("--profile", "team")).stdout, /0 new agent/);
});

for (const [resourceSymlinks, customAgentDir] of [[false, false], [true, false], [false, true]]) {
test(`jailed profile uses ai-jail state defaults (${resourceSymlinks ? "symlink resources" : customAgentDir ? "custom agent directory" : "standard Pi directory"}) and real Pi startup`, { timeout: 60000 }, async t => {
  // Opt-in runs the actual CLI under native ai-jail; normal CI uses the SDK and
  // argument fixture. Neither loads real credentials nor makes provider calls.
  const f = await fixture(t, { nativeJail: process.env.MUSTR_NATIVE_JAIL_TEST === "1", resourceSymlinks, customAgentDir });
  assert.match((await f.run("--profile", "team", "--jailed")).stdout, /1 new agent/);
  assert.match((await f.run("--profile", "team", "--jailed")).stdout, /0 new agent/);
  await assert.rejects(f.run("--profile", "team"), /profile drift/);
  const args = JSON.parse(await readFile(join(f.root, "jail-invocation.json"), "utf8"));
  for (const flag of ["--exec", "--terminal-passthrough", "--private-home", "--no-agent-state", "--agent-state", "--no-inherit-env", "--no-network", "--no-macos-host-ipc", "--no-docker", "--no-toolchains", "--no-mise"]) assert.ok(!args.includes(flag), `policy must belong to ai-jail: ${flag}`);
  assert.equal(args.filter(value => value === "--forward-port").length, 1);
  const passedEnv = args.flatMap((value, i) => value === "--env" ? [args[i + 1]] : []);
  assert.deepEqual(passedEnv.map(value => value.split("=")[0]).sort(), customAgentDir ? ["MUSTR_LAUNCH_DESCRIPTOR", "PI_CODING_AGENT_DIR"] : ["MUSTR_LAUNCH_DESCRIPTOR"]);
  const descriptorPath = passedEnv.find(value => value.startsWith("MUSTR_LAUNCH_DESCRIPTOR=")).split("=").slice(1).join("=");
  const descriptor = JSON.parse(await readFile(descriptorPath, "utf8"));
  assert.equal(descriptor.version, 2);
  assert.equal(args[args.indexOf("--forward-port") + 1], String(descriptor.gateway.port));
  const roMaps = args.flatMap((value, i) => value === "--map" ? [args[i + 1]] : []);
  assert.deepEqual(roMaps, [descriptorPath], "no Mustr-owned Pi resource mounts");
  assert.equal(args.filter(value => value === "--rw-map").length, customAgentDir ? 1 : 0);
  if (customAgentDir) {
    const agentDir = await realpath(f.agentDir);
    assert.equal(args[args.indexOf("--rw-map") + 1], agentDir);
    assert.ok(passedEnv.includes(`PI_CODING_AGENT_DIR=${agentDir}`));
  }
  assert.ok(!args.some(value => value.includes(descriptor.gateway.token)));
  const receipt = JSON.parse(await readFile(join(dirname(dirname(descriptorPath)), "ready.json"), "utf8"));
  assert.equal(receipt.launch_id, descriptor.launch_id);
  assert.equal(receipt.model, descriptor.model);
  assert.equal(receipt.role, "architect");
  assert.match((await f.run("agent", "list")).stdout, /team\/architect\s+ready/);
  await assert.rejects(new Promise((resolve, reject) => {
    const socket = net.createConnection({ host: "127.0.0.1", port: descriptor.gateway.port });
    socket.once("connect", () => { socket.destroy(); resolve(); });
    socket.once("error", reject);
  }), /ECONNREFUSED/, "gateway closes when the fixture-owned Pi exits");
  assert.equal((await f.owned()).creates.length, 1);
  if (process.env.MUSTR_NATIVE_JAIL_TEST === "1") {
    const evidence = JSON.parse(await readFile(join(f.cwd, "native-cli-result.json"), "utf8"));
    assert.equal(evidence.nativePiCli, true);
    assert.equal(evidence.modelMessages, 0);
    assert.equal(evidence.isolated, true);
    assert.equal(evidence.sharedPiState, true);
    assert.equal(await readFile(join(f.agentDir, "state-sharing-result.txt"), "utf8"), "persisted from jail");
    assert.equal(await readFile(join(f.env.HOME, ".pi-lens/state-sharing-result.txt"), "utf8"), "persisted lens state");
    assert.equal(evidence.exited, true);
    t.diagnostic("Actual offline Pi CLI + native ai-jail: shared auth/history/lens state, writable persistence, scoped readiness, forbidden host files/UDS/second port, clean exit verified.");
  }
});
}

test("individual unjailed starts ignore other jailed slots; unsafe jailed cwd fails before creation", { timeout: 30000 }, async t => {
  const f = await fixture(t, { laterSlot: true });
  const path = join(f.config, "profiles.toml");
  const profile = await readFile(path, "utf8");
  await writeFile(path, profile.replace("[profiles.team.agents.implementor]", `[profiles.team.agents.implementor]\njailed = true\ncwd = ${JSON.stringify(f.env.MUSTR_STATE_DIR)}`));
  assert.match((await f.run("agent", "start", "architect", "--profile", "team")).stdout, /team\/architect/);
  await assert.rejects(f.run("--profile", "team"), /jailed cwd must not overlap/);
  await assert.rejects(f.run("agent", "start", "implementor", "--profile", "team"), /jailed cwd must not overlap/);
  assert.equal((await f.owned()).creates.length, 1);
  await writeFile(join(f.root, "bin/ai-jail"), '#!/bin/sh\nprintf "ai-jail 2.11.0\\n"\n', { mode: 0o700 });
  await assert.rejects(f.run("--profile", "team", "--jailed"), /requires ai-jail 2.12.0 or newer/);
  assert.equal((await f.owned()).creates.length, 1);
});

test("installed default profile launches all four high-thinking slots without replacing configuration", { timeout: 60000 }, async t => {
  const f = await fixture(t, { shippedProfile: true });
  const before = await readFile(join(f.config, "profiles.toml"));
  assert.match((await f.run("--profile", "default")).stdout, /4 new agent/);
  assert.match((await f.run("--profile", "default")).stdout, /0 new agent/);
  const owned = await f.owned();
  assert.deepEqual(owned.creates.map(tab => tab.label).sort(), ["arch", "impl", "research", "review"]);
  const cwd = await realpath(f.cwd);
  assert.ok(owned.creates.every(tab => tab.cwd === cwd), "no project path is hard-coded in the shipped profile");
  assert.equal(owned.runs.length, 4);
  assert.deepEqual(owned.closes, []);
  const listed = (await f.run("agent", "list", "--profile", "default")).stdout;
  for (const slot of ["arch", "impl", "review", "research"]) assert.match(listed, new RegExp(String.raw`default/${slot}\s+ready`));
  assert.deepEqual(await readFile(join(f.config, "profiles.toml")), before);
});

for (const newHub of [false, true]) {
test(`acknowledged bootstrap failure returns promptly (${newHub ? "new hub shuts down" : "existing hub"})`, { timeout: 30000 }, async t => {
  const f = await fixture(t, { jailFailure: true, acknowledgeFailure: true });
  if (newHub) await f.stopHub();
  const started = Date.now();
  await assert.rejects(f.run("run", "--headless", "--profile", "team", "--jailed"), /failed during jailed Pi startup.*no retry or cleanup attempted/s);
  assert.ok(Date.now() - started < 8000, "known failure must not wait for the 20-second readiness deadline");
  await assert.rejects(f.run("--profile", "team", "--jailed"), /startup\/join is unconfirmed/);
  const owned = await f.owned();
  assert.equal(owned.creates.length, 1);
  assert.equal(owned.runs.length, 1);
  assert.deepEqual(owned.closes, []);
});
}

test("default profile waits for Herdr to recognize each Pi before joining and launches all four slots", { timeout: 30000 }, async t => {
  const f = await fixture(t, { shippedProfile: true, pendingDetections: 2 });
  assert.match((await f.run("--profile", "default")).stdout, /4 new agent/);
  const owned = await f.owned();
  assert.deepEqual(owned.creates.map(tab => tab.label).sort(), ["arch", "impl", "research", "review"]);
  assert.equal(owned.runs.length, 4, "startup never retries the command that starts Pi");
  const listed = (await f.run("agent", "list", "--profile", "default")).stdout;
  for (const slot of ["arch", "impl", "research", "review"]) assert.match(listed, new RegExp(String.raw`default/${slot}\s+ready`));
  assert.equal(f.detectionChecks.size, 4);
  assert.ok([...f.detectionChecks.values()].every(checks => checks > 2), "each launch survives pending recognition and confirms Pi");
  assert.match((await f.run("--profile", "default")).stdout, /0 new agent/);
  assert.equal((await f.owned()).creates.length, 4);
});

test("jail failure keeps the owned tab unresolved and never falls back or relaunches", { timeout: 30000 }, async t => {
  const f = await fixture(t, { jailFailure: true });
  await assert.rejects(f.run("--profile", "team", "--jailed"), /no retry attempted/);
  await assert.rejects(f.run("--profile", "team", "--jailed"), /startup\/join is unconfirmed/);
  const owned = await f.owned();
  assert.equal(owned.creates.length, 1);
  assert.equal(owned.runs.length, 1);
  assert.deepEqual(owned.closes, []);
});
