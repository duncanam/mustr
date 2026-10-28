import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import net from "node:net";
import { once } from "node:events";
import { Agent } from "@earendil-works/pi-agent-core";
import { InMemoryCredentialStore } from "@earendil-works/pi-ai";
import {
  AgentSession, createAgentSessionRuntime, createAgentSessionServices,
  ModelRuntime, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";
import { HubClient, HubRemoteError } from "../transport.mjs";
import { harness } from "./adapter-harness.mjs";
import { delay, eventually } from "../../tests/support.mjs";

const token = "PRIVATE-STARTUP-CREDENTIAL-DO-NOT-DISPLAY";
const grant = { launch_id: "0123456789abcdef0123456789abcdef", role: "custom", token };
const normalHello = { delivery_policy: 1, inbox_notifications: 1, reset_protocol: 1 };
const startupHello = { ...normalHello, startup_grant_protocol: 1 };

function fakeHub(t, onConnect, onRequest) {
  const original = {
    connect: HubClient.prototype.connect,
    request: HubClient.prototype.request,
    close: HubClient.prototype.close,
  };
  const state = { hellos: [], requests: [], clients: [] };
  HubClient.prototype.connect = async function(path, hello) {
    this.connected = true;
    state.clients.push(this);
    state.hellos.push({ path, hello: { ...hello } });
    if (onConnect) return onConnect.call(this, path, hello, state);
    return hello.startup_grant ? startupHello : normalHello;
  };
  HubClient.prototype.request = async function(command) {
    state.requests.push({ ...command });
    if (!this.connected) throw new Error("fixture hub disconnected");
    if (onRequest) return onRequest.call(this, command, state);
    if (command.op === "reset_capability") return { reset_protocol: 1 };
    if (command.op === "subscribe") return { subscribed: true };
    if (command.op === "sync") return { members: [], tasks: [], automation: {}, inbox: null, insertion_policy: "idle_only" };
    if (command.op === "leave") return { left: true };
    return {};
  };
  HubClient.prototype.close = function() {
    const wasConnected = this.connected;
    this.connected = false;
    if (wasConnected) this.onClose?.();
  };
  // setup() registers its session cleanup later, so it runs before this restore.
  t.after(() => {
    HubClient.prototype.connect = original.connect;
    HubClient.prototype.request = original.request;
    HubClient.prototype.close = original.close;
  });
  return state;
}

function flagAPI(h, value, { throwOnRead = false } = {}) {
  return {
    registered: h.root.registeredFlags,
    reads: h.root.flagReads,
    api: throwOnRead ? {
      ...h.api,
      getFlag(name) { h.root.flagReads.push(name); throw new Error(String(value)); },
    } : h.api,
  };
}

function isolateProcessRegistry(t) {
  const key = Symbol.for("mustr.reset.bridge.v1");
  const descriptor = Object.getOwnPropertyDescriptor(globalThis, key);
  delete globalThis[key];
  t.after(() => {
    if (descriptor) Object.defineProperty(globalThis, key, descriptor);
    else delete globalThis[key];
  });
}

async function setup(t, { value = JSON.stringify(grant), throwOnRead = false, agent = "pi", roles = [{ name: "custom", description: "Custom role", prompt: "Approved custom role body." }], sessionId = "startup-session-1234" } = {}) {
  isolateProcessRegistry(t);
  const directory = await mkdtemp("/tmp/mustr-startup-adapter-");
  const identityPath = `${directory}/identity.json`;
  const rolesPath = `${directory}/roles.json`;
  const roleLog = `${directory}/role-lookups.jsonl`;
  const contextLog = `${directory}/context-lookups.jsonl`;
  const cli = `${directory}/mustr-fixture.mjs`;
  const identity = { protocol: 1, server_key: "fixture-server", socket: `${directory}/hub.sock`, pane_id: "w1:p1", space_id: "w1", agent };
  await writeFile(identityPath, JSON.stringify(identity));
  await writeFile(rolesPath, JSON.stringify(roles));
  await writeFile(cli, `#!${process.execPath}\nimport { appendFileSync, readFileSync } from "node:fs";\nconst args = process.argv.slice(2);\nif (args.includes("context")) { appendFileSync(${JSON.stringify(contextLog)}, "context\\n"); console.log(readFileSync(${JSON.stringify(identityPath)}, "utf8")); }\nelse if (args.includes("roles")) { appendFileSync(${JSON.stringify(roleLog)}, JSON.stringify(args) + "\\n"); const all = JSON.parse(readFileSync(${JSON.stringify(rolesPath)}, "utf8")); const requested = args[args.indexOf("roles") + 1]; console.log(JSON.stringify(all.filter(role => !requested || role.name === requested))); }\nelse process.exit(2);\n`, { mode: 0o700 });

  const envKeys = ["MUSTR_BIN", "MUSTR_CONFIG_DIR", "HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"];
  const previousEnv = new Map(envKeys.map(key => [key, process.env[key]]));
  Object.assign(process.env, {
    MUSTR_BIN: cli,
    MUSTR_CONFIG_DIR: directory,
    HERDR_ENV: "1",
    HERDR_PANE_ID: "w1:p1",
    HERDR_SOCKET_PATH: `${directory}/herdr.sock`,
  });

  const h = harness(directory, { sessionId, flagValues: new Map([["mustr-startup", value]]) });
  const flags = flagAPI(h, value, { throwOnRead });
  const { default: extension } = await import(`../index.ts?startup-adapter=${Date.now()}-${Math.random()}`);
  h.root.factory = extension;
  await extension(flags.api);
  t.after(async () => {
    await h.emit("session_shutdown", { reason: "quit" });
    for (const [key, oldValue] of previousEnv) oldValue === undefined ? delete process.env[key] : process.env[key] = oldValue;
    await rm(directory, { recursive: true, force: true });
  });

  return {
    directory, identityPath, rolesPath, roleLog, contextLog, h, flags,
    async setIdentity(next) { await writeFile(identityPath, JSON.stringify(next)); },
    async contextLookups() {
      try { return (await readFile(contextLog, "utf8")).trim().split("\n").filter(Boolean).length; }
      catch (error) { if (error.code === "ENOENT") return 0; throw error; }
    },
    async roleLookups() {
      try { return (await readFile(roleLog, "utf8")).trim().split("\n").filter(Boolean).map(line => JSON.parse(line)); }
      catch (error) { if (error.code === "ENOENT") return []; throw error; }
    },
  };
}

function visibleUI(h) {
  return JSON.stringify({ notifications: h.notifications, statuses: h.statuses, messages: h.messages, entries: h.entries });
}

async function withStartupDeadlineClock(run) {
  const realNow = Date.now;
  const realSetTimeout = globalThis.setTimeout;
  let now = realNow();
  Date.now = () => now;
  globalThis.setTimeout = (callback, delayMs, ...args) => {
    if (delayMs === 250) {
      now += 10_000;
      return realSetTimeout(callback, 0, ...args);
    }
    return realSetTimeout(callback, delayMs, ...args);
  };
  try { return await run(() => now); }
  finally { Date.now = realNow; globalThis.setTimeout = realSetTimeout; }
}

test("startup Join waits for live Pi readiness and sends one acknowledged grant before role resolution", async t => {
  let fixture;
  const hub = fakeHub(t, async function(path, hello) {
    assert.equal((await fixture.roleLookups()).length, 0, "local role resolution waits for the grant acknowledgement");
    return hello.startup_grant ? startupHello : normalHello;
  });
  fixture = await setup(t, { agent: "claude" });
  assert.deepEqual(fixture.flags.registered.get("mustr-startup"), { type: "string", description: "Internal one-use profile startup handoff" });

  const starting = fixture.h.emit("session_start", { reason: "startup" });
  let settled = false;
  starting.then(() => { settled = true; });
  await delay(300);
  assert.equal(settled, false, "the startup handler remains readiness-gated");
  assert.equal(hub.hellos.length, 0, "a non-Pi pane is never joined");

  await fixture.setIdentity({ protocol: 1, server_key: "fixture-server", socket: `${fixture.directory}/hub.sock`, pane_id: "w1:p1", space_id: "w1", agent: "pi" });
  await starting;
  assert.equal(hub.hellos.length, 1);
  assert.equal(hub.hellos[0].path, `${fixture.directory}/hub.sock`);
  assert.equal(hub.hellos[0].hello.agent_id, "startup-session-1234");
  assert.equal(hub.hellos[0].hello.name, "custom-startup-");
  assert.equal(hub.hellos[0].hello.role, "custom");
  assert.deepEqual(hub.hellos[0].hello.startup_grant, { version: 1, launch_id: grant.launch_id, token });
  assert.equal((await fixture.roleLookups()).length, 1);
  assert.ok(fixture.h.active().includes("mustr_roster"));
  assert.match((await fixture.h.prompt("Policy")).text, /Approved custom role body\./);
  assert.equal(visibleUI(fixture.h).includes(token), false);
  assert.deepEqual(fixture.flags.reads, ["mustr-startup"]);
});

test("startup flag persists across repeated starts and Pi lifecycle replacements without reuse", async t => {
  const hub = fakeHub(t);
  const fixture = await setup(t, { agent: "pi" });
  assert.ok(fixture.h.root.flagValues instanceof Map, "the harness retains the original Pi flag value");
  assert.equal(fixture.h.root.flagValues.get("mustr-startup"), JSON.stringify(grant));

  await fixture.h.emit("session_start", { reason: "startup" });
  const registry = globalThis[Symbol.for("mustr.reset.bridge.v1")];
  assert.equal(registry.startupFlagAttempted, true, "the startup guard is process-wide");
  assert.deepEqual(Object.keys(registry).sort(), ["ready", "restores", "startupFlagAttempted"], "the shared registry stores only its existing maps and a noncredential latch");
  assert.equal(hub.hellos.length, 1);
  assert.ok(fixture.h.active().includes("mustr_roster"), "the initial acknowledged Join activates the role");
  assert.equal(fixture.flags.reads.length, 1);

  await fixture.h.emit("session_start", { reason: "startup" });
  assert.equal(fixture.flags.reads.length, 1, "a repeated startup in the same factory does not reread the flag");
  assert.equal(hub.hellos.length, 1, "a repeated startup does not Join again");
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);

  for (const reason of ["reload", "new", "resume", "fork"]) {
    const replacement = await fixture.h.root.replace(reason);
    assert.equal(fixture.h.root.flagValues.get("mustr-startup"), JSON.stringify(grant), `${reason} retains the exact flag value`);
    assert.equal(fixture.flags.reads.length, 1, `${reason} does not read the startup flag`);
    assert.equal(hub.hellos.length, 1, `${reason} does not Join`);
    assert.deepEqual(replacement.active(), ["read", "bash", "policy-tool"], `${reason} leaves role/tools inactive`);
  }

  await fixture.h.root.current.emit("session_start", { reason: "startup" });
  assert.equal(fixture.flags.reads.length, 1, "a fresh factory cannot present a spent flag on startup");
  assert.equal(hub.hellos.length, 1);
  assert.deepEqual(fixture.h.root.current.active(), ["read", "bash", "policy-tool"]);
  assert.deepEqual(fixture.h.root.dispatches, [], "startup never synthesizes sendUserMessage or /role");
});

test("startup intent is consumed once and ordinary reconnect omits its credential", async t => {
  const hub = fakeHub(t);
  const fixture = await setup(t);
  await fixture.h.emit("session_start", { reason: "startup" });
  assert.equal(hub.hellos.length, 1);
  assert.equal(hub.hellos[0].hello.startup_grant.token, token);

  const first = hub.clients[0];
  first.connected = false;
  first.onClose?.();
  await new Promise((resolve, reject) => {
    const deadline = Date.now() + 4000;
    const check = () => {
      if (hub.hellos.length === 2) return resolve();
      if (Date.now() >= deadline) return reject(new Error("ordinary reconnect did not occur"));
      setTimeout(check, 20);
    };
    check();
  });
  assert.equal(Object.hasOwn(hub.hellos[1].hello, "startup_grant"), false);
  assert.equal(hub.hellos.length, 2);
  assert.equal(visibleUI(fixture.h).includes(token), false);
});

for (const failure of ["rejection", "missing acknowledgement", "ambiguous connection"]) {
  test(`startup ${failure} closes cleanly without retry or credential disclosure`, async t => {
    const hub = fakeHub(t, async function(_path, hello) {
      if (failure === "rejection") throw new HubRemoteError(`rejected ${token}`);
      if (failure === "ambiguous connection") throw new Error(`acknowledgement lost: ${token}`);
      return normalHello;
    });
    const fixture = await setup(t, { roles: [] });
    await fixture.h.emit("session_start", { reason: "startup" });
    assert.equal(hub.hellos.length, 1);
    assert.equal(hub.hellos[0].hello.startup_grant.token, token);
    assert.equal(hub.requests.length, 0, "no post-Join negotiation follows an unconfirmed grant");
    assert.equal(hub.clients[0].connected, false, "failure cleanup closes the uncertain connection");
    assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
    assert.equal(await fixture.roleLookups().then(calls => calls.length), 0, "no local role is resolved before a valid acknowledgement");
    assert.equal(visibleUI(fixture.h).includes(token), false);

    await fixture.h.emit("turn_end");
    await delay(100);
    assert.equal(hub.hellos.length, 1, "the spent startup grant is never retried or replaced by a manual Join");
  });
}

test("startup flag is read only for startup and latches before a failing read", async t => {
  const hub = fakeHub(t);
  const fixture = await setup(t, { value: token, throwOnRead: true });
  await fixture.h.emit("session_start", { reason: "reload" });
  assert.deepEqual(fixture.flags.reads, []);
  await fixture.h.emit("session_start", { reason: "startup" });
  await fixture.h.emit("session_start", { reason: "startup" });
  assert.deepEqual(fixture.flags.reads, ["mustr-startup"]);
  assert.equal(hub.hellos.length, 0);
  assert.equal(visibleUI(fixture.h).includes(token), false);
});

function startupPayloadAt149Bytes(secret) {
  const emptyId = JSON.stringify({ launch_id: "", role: "custom", token: secret });
  const launchId = "l".repeat(149 - Buffer.byteLength(emptyId, "utf8"));
  return JSON.stringify({ launch_id: launchId, role: "custom", token: secret });
}

test("startup payload enforces compact UTF-8 byte limit and exact fields", async t => {
  const atLimit = startupPayloadAt149Bytes(token);
  assert.equal(Buffer.byteLength(atLimit, "utf8"), 149);
  const hub = fakeHub(t);
  const fixture = await setup(t, { value: atLimit });
  await fixture.h.emit("session_start", { reason: "startup" });
  assert.equal(hub.hellos.length, 1, "the documented maximum payload is accepted");
});

test("150-byte parseable startup payload fails closed without disclosing its credential", async t => {
  const atLimit = startupPayloadAt149Bytes(token);
  const oversized = `${atLimit} `;
  assert.equal(Buffer.byteLength(oversized, "utf8"), 150);
  assert.doesNotThrow(() => JSON.parse(oversized), "the extra byte is valid JSON whitespace");

  const hub = fakeHub(t);
  const fixture = await setup(t, { value: oversized });
  await fixture.h.emit("session_start", { reason: "startup" });
  assert.equal(hub.hellos.length, 0, "a 150-byte handoff cannot Join");
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
  assert.ok(fixture.h.notifications.some(item => /Invalid profile startup handoff/.test(item.message)), "the failure is reported without payload details");
  assert.equal(visibleUI(fixture.h).includes(token), false, "the credential is not echoed in UI");
});

for (const [label, value] of [
  ["over the byte limit", JSON.stringify({ launch_id: "l".repeat(106), role: "custom", token })],
  ["multibyte UTF-8 over the byte limit", JSON.stringify({ launch_id: "é".repeat(55), role: "custom", token: "t" })],
  ["non-compact JSON", ` ${JSON.stringify(grant)}`],
  ["unknown field", JSON.stringify({ ...grant, unexpected: true })],
]) {
  test(`invalid startup payload (${label}) fails closed without displaying its credential`, async t => {
    const hub = fakeHub(t);
    const fixture = await setup(t, { value });
    await fixture.h.emit("session_start", { reason: "startup" });
    assert.deepEqual(fixture.flags.reads, ["mustr-startup"]);
    assert.equal(hub.hellos.length, 0);
    assert.equal(visibleUI(fixture.h).includes(token), false);
  });
}

test("identity drift during readiness stops startup before Join", async t => {
  const hub = fakeHub(t);
  const fixture = await setup(t, { agent: "claude" });
  const starting = fixture.h.emit("session_start", { reason: "startup" });
  let settled = false;
  starting.then(() => { settled = true; });
  await eventually(async () => (await fixture.contextLookups()) >= 2, 2000);
  assert.equal(settled, false, "startup is polling the original pane identity");
  await fixture.setIdentity({ protocol: 1, server_key: "fixture-server", socket: `${fixture.directory}/other.sock`, pane_id: "w1:p1", space_id: "w2", agent: "pi" });
  await starting;
  assert.equal(hub.hellos.length, 0);
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
  assert.equal(visibleUI(fixture.h).includes(token), false);
});

test("session cancellation during readiness prevents Join", async t => {
  const hub = fakeHub(t);
  const fixture = await setup(t, { agent: "claude" });
  const starting = fixture.h.emit("session_start", { reason: "startup" });
  let settled = false;
  starting.then(() => { settled = true; });
  await delay(50);
  assert.equal(settled, false, "readiness polling is still in flight before cancellation");
  await fixture.h.emit("session_shutdown", { reason: "quit" });
  await starting;
  assert.equal(hub.hellos.length, 0);
});

test("readiness timeout never submits a Join", { timeout: 15000 }, async t => {
  const hub = fakeHub(t);
  const fixture = await setup(t, { agent: null });
  const elapsed = await withStartupDeadlineClock(async clock => {
    const startedAt = clock();
    await fixture.h.emit("session_start", { reason: "startup" });
    return clock() - startedAt;
  });
  assert.ok(elapsed >= 10_000, "the private clock crosses the unchanged 10-second readiness deadline");
  assert.equal(hub.hellos.length, 0);
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
  assert.equal(visibleUI(fixture.h).includes(token), false);
});

const sdkPolicy = "BASE SDK POLICY: preserve the caller's system instructions.";
const sdkFlagValue = JSON.stringify({
  launch_id: "abcdef0123456789abcdef0123456789",
  role: "custom",
  token: "PRIVATE-PINNED-SDK-STARTUP-CREDENTIAL-DO-NOT-LOG",
});
const sdkModel = {
  id: "unused", name: "unused", provider: "startup-sdk-test-no-provider", api: "unknown",
  baseUrl: "", reasoning: false, input: ["text"], contextWindow: 100000, maxTokens: 100,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
};

async function sdkFixture(t) {
  isolateProcessRegistry(t);
  const directory = await mkdtemp("/tmp/mustr-startup-sdk-");
  const socketPath = `${directory}/hub.sock`;
  const identityPath = `${directory}/identity.json`;
  const cli = `${directory}/mustr-fixture.mjs`;
  await writeFile(identityPath, JSON.stringify({
    protocol: 1, socket: socketPath, server_key: "sdk-fixture-server", space_id: "w1", pane_id: "w1:p1", agent: "pi",
  }));
  await writeFile(cli, `#!${process.execPath}\nimport {readFileSync} from "node:fs";\nconst args=process.argv.slice(2);\nif(args.includes("context")) console.log(readFileSync(${JSON.stringify(identityPath)}, "utf8"));\nelse if(args.includes("roles")) console.log(JSON.stringify([{name:"custom",description:"SDK fixture role",prompt:"APPROVED SDK ROLE BODY"}]));\nelse process.exit(2);\n`, { mode: 0o700 });

  const envKeys = ["MUSTR_BIN", "MUSTR_CONFIG_DIR", "HERDR_ENV", "HERDR_PANE_ID", "HERDR_SOCKET_PATH"];
  const previousEnv = new Map(envKeys.map(key => [key, process.env[key]]));
  Object.assign(process.env, {
    MUSTR_BIN: cli, MUSTR_CONFIG_DIR: directory, HERDR_ENV: "1", HERDR_PANE_ID: "w1:p1", HERDR_SOCKET_PATH: `${directory}/herdr.sock`,
  });
  const { default: adapter } = await import(`../index.ts?startup-sdk=${Date.now()}-${Math.random()}`);
  const calls = [], sockets = new Set(), peerErrors = [], states = [], errors = [];
  const members = new Map();
  let runtime, streamCalls = 0, leaveAcks = 0;
  const server = net.createServer(socket => {
    sockets.add(socket); socket.on("close", () => sockets.delete(socket)); socket.on("error", () => {});
    let buffer = "", joinedId;
    socket.on("data", chunk => {
      buffer += chunk;
      while (buffer.includes("\n")) {
        const end = buffer.indexOf("\n"), line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
        void respond(JSON.parse(line));
      }
    });
    async function respond(request) {
      calls.push({ op: request.op, agent_id: request.agent_id, role: request.role, hasStartupGrant: !!request.startup_grant });
      let result;
      try {
        switch (request.op) {
          case "join":
            joinedId = request.agent_id;
            members.set(joinedId, { id: joinedId, role: request.role, name: request.name, pane_id: request.pane_id, space_id: request.space_id, activity: "idle" });
            result = request.startup_grant
              ? { delivery_policy: 1, inbox_notifications: 1, reset_protocol: 1, startup_grant_protocol: 1 }
              : { delivery_policy: 1, inbox_notifications: 1, reset_protocol: 1 };
            break;
          case "reset_capability": result = { reset_protocol: 1 }; break;
          case "subscribe": result = { subscribed: true }; break;
          case "sync": result = { members: [...members.values()], tasks: [], automation: {}, inbox: null, insertion_policy: "idle_only" }; break;
          case "leave":
            leaveAcks++;
            if (joinedId && members.has(joinedId)) members.set(joinedId, { ...members.get(joinedId), activity: "offline" });
            result = { left: true };
            break;
          default: throw new Error(`Unexpected hub operation ${request.op}`);
        }
        if (!socket.destroyed) socket.write(JSON.stringify({ id: request.id, ok: true, result }) + "\n");
      } catch (error) { peerErrors.push(error); socket.destroy(); }
    }
  });
  server.listen(socketPath); await once(server, "listening");
  const modelRuntime = await ModelRuntime.create({ credentials: new InMemoryCredentialStore(), modelsPath: null, allowModelNetwork: false, refreshOnCreate: false });
  const startupFlags = new Map([["mustr-startup", sdkFlagValue]]);

  async function createRuntime({ sessionManager, sessionStartEvent }) {
    const state = {
      session: undefined, services: undefined, starts: [], activeAtStart: [], flagReads: [], commandCalls: [], userMessages: [],
      currentReason: undefined, disposed: false,
    };
    states.push(state);
    const instrumentedAdapter = api => adapter(new Proxy(api, { get(target, key) {
      const value = Reflect.get(target, key, target);
      if (key === "on") return (eventName, handler) => value.call(target, eventName, eventName === "session_start"
        ? async (event, ...args) => {
          state.currentReason = event.reason;
          try { return await handler(event, ...args); }
          finally { state.currentReason = undefined; }
        }
        : handler);
      if (key === "getFlag") return name => {
        const flagValue = value.call(target, name);
        state.flagReads.push({ name, value: flagValue, reason: state.currentReason });
        return flagValue;
      };
      if (key === "sendUserMessage") return (...args) => {
        state.userMessages.push(args[0]);
        return value.apply(target, args);
      };
      if (key === "registerCommand") return (name, definition) => value.call(target, name, {
        ...definition,
        async handler(...args) { state.commandCalls.push(name); return definition.handler(...args); },
      });
      return typeof value === "function" ? value.bind(target) : value;
    } }));
    const observerFactory = api => api.on("session_start", event => {
      state.starts.push(event.reason);
      state.activeAtStart.push(state.session?.getActiveToolNames() ?? []);
    });
    const services = await createAgentSessionServices({
      cwd: directory,
      agentDir: directory,
      modelRuntime,
      settingsManager: SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: false }, cacheWarming: "off" }),
      extensionFlagValues: startupFlags,
      resourceLoaderOptions: {
        extensionFactories: [instrumentedAdapter, observerFactory], noSkills: true, noPromptTemplates: true, noThemes: true, noContextFiles: true,
        systemPrompt: sdkPolicy,
      },
    });
    state.services = services;
    const agent = new Agent({ initialState: { model: sdkModel }, prepareRequest() { throw new Error("Provider requests forbidden in startup lifecycle test"); }, streamFn() { streamCalls++; throw new Error("No provider call is permitted"); } });
    const session = new AgentSession({
      agent, sessionManager, settingsManager: services.settingsManager, resourceLoader: services.resourceLoader, modelRuntime,
      cwd: directory, sessionStartEvent, initialActiveToolNames: ["policy-tool"], baseToolsOverride: {
        "policy-tool": { name: "policy-tool", label: "policy-tool", description: "Preserved policy guard", parameters: { type: "object", properties: {} }, async execute() { throw new Error("No tool execution is permitted"); } },
      },
    });
    session.refreshContext(); state.session = session;
    return { session, services, diagnostics: services.diagnostics };
  }

  const sessionDir = `${directory}/sessions`;
  const initialManager = SessionManager.create(directory, sessionDir);
  initialManager.appendMessage({ role: "user", content: "Local SDK lifecycle fixture seed", timestamp: Date.now() });
  const resumeManager = SessionManager.create(directory, sessionDir);
  const resumeEntry = resumeManager.appendMessage({ role: "user", content: "Local SDK resume target", timestamp: Date.now() });
  const resumeFile = resumeManager.getSessionFile();
  runtime = await createAgentSessionRuntime(createRuntime, { cwd: directory, agentDir: directory, sessionManager: initialManager });
  async function bind(session) {
    await session.bindExtensions({
      mode: "tui", onError: error => errors.push(error),
      uiContext: { getEditorText: () => "", setStatus() {}, notify() {} },
      commandContextActions: {
        waitForIdle: () => session.waitForIdle(), newSession: options => runtime.newSession(options),
        fork: (id, options) => runtime.fork(id, options), switchSession: (path, options) => runtime.switchSession(path, options),
        async navigateTree() { throw new Error("Unused tree navigation"); }, async reload() { throw new Error("Unused bound reload"); },
      },
    });
  }
  runtime.setRebindSession(bind); await bind(runtime.session);
  t.after(async () => {
    try { await runtime.dispose(); }
    finally {
      for (const socket of sockets) socket.destroy();
      await new Promise(resolve => server.close(resolve));
      for (const [key, value] of previousEnv) { if (value === undefined) delete process.env[key]; else process.env[key] = value; }
      await rm(directory, { recursive: true, force: true });
    }
    assert.equal(streamCalls, 0, "the fixture never invokes a provider");
    assert.deepEqual(peerErrors, [], "the local peer saw no unexpected operation");
    assert.deepEqual(errors, [], "Pi reported no extension errors");
  });
  return { runtime, states, calls, leaveAcks: () => leaveAcks, resumeFile, resumeEntry, peerErrors, errors, startupFlags };
}

test("pinned Pi 1.2.0 retains the valid CLI flag across startup, reload, new, resume and fork", { timeout: 20000 }, async t => {
  const fixture = await sdkFixture(t);
  const baseTools = ["policy-tool"];
  const joins = () => fixture.calls.filter(call => call.op === "join");
  const assertRetained = session => assert.ok(session.extensionRunner.getFlagValues().get("mustr-startup") === sdkFlagValue, "the original CLI flag remains in Pi's real flag store");
  const assertInactive = async session => {
    assert.deepEqual(session.getActiveToolNames(), baseTools, "non-startup lifecycle leaves role tools inactive");
    const prompt = await session.extensionRunner.emitBeforeAgentStart(
      { messageType: "custom", message: { customType: "probe", content: "No model", display: false } },
      { sections: { preamble: sdkPolicy } },
    );
    assert.equal(Object.hasOwn(prompt.systemPromptOptions.sections, "mustr_role"), false, "non-startup lifecycle cannot reactivate the role prompt");
  };
  const original = fixture.runtime.session;
  const firstState = fixture.states.at(-1);
  assertRetained(original);
  assert.equal(joins().length, 1, "initial Pi startup sent exactly one grant-bearing Join");
  assert.equal(joins()[0].hasStartupGrant, true);
  assert.ok(original.getActiveToolNames().includes("mustr_roster"), "the acknowledged startup Join activates tools");
  const activePrompt = await original.extensionRunner.emitBeforeAgentStart(
    { messageType: "custom", message: { customType: "probe", content: "No model", display: false } },
    { sections: { preamble: sdkPolicy } },
  );
  assert.match(activePrompt.systemPromptOptions.sections.mustr_role, /APPROVED SDK ROLE BODY/);

  await original.extensionRunner.emit({ type: "session_start", reason: "startup" });
  await assertInactive(original);
  assert.equal(joins().length, 1, "a repeated startup does not replay Join");
  assert.equal(fixture.leaveAcks(), 1, "the old acknowledged participant is explicitly left once");
  await original.reload();
  assertRetained(original);
  await assertInactive(original);
  assert.deepEqual(firstState.starts, ["startup", "startup", "reload"]);

  assert.equal((await fixture.runtime.newSession()).cancelled, false);
  let current = fixture.runtime.session;
  const newState = fixture.states.at(-1);
  assertRetained(current);
  await assertInactive(current);
  await current.extensionRunner.emit({ type: "session_start", reason: "startup" });
  assertRetained(current);
  await assertInactive(current);
  assert.deepEqual(newState.starts, ["new", "startup"]);

  assert.equal((await fixture.runtime.switchSession(fixture.resumeFile)).cancelled, false);
  current = fixture.runtime.session;
  const resumeState = fixture.states.at(-1);
  assertRetained(current);
  await assertInactive(current);
  assert.deepEqual(resumeState.starts, ["resume"]);

  await fixture.runtime.fork(fixture.resumeEntry, { position: "before" });
  current = fixture.runtime.session;
  const forkState = fixture.states.at(-1);
  assertRetained(current);
  await assertInactive(current);
  assert.deepEqual(forkState.starts, ["fork"]);

  assert.equal(fixture.states.flatMap(state => state.flagReads).length, 1, "the adapter performs exactly one real getFlag read process-wide");
  const read = fixture.states.flatMap(state => state.flagReads)[0];
  assert.equal(read.name, "mustr-startup");
  assert.equal(read.reason, "startup", "the only read occurs during initial startup");
  assert.ok(read.value === sdkFlagValue, "the actual Pi API returns the unchanged valid flag value");
  assert.equal(joins().length, 1, "reload/new/resume/fork and a fresh-factory startup never Join again");
  assert.ok(joins().every(call => call.hasStartupGrant), "no grant-free Join fallback is attempted");
  assert.deepEqual(fixture.states.flatMap(state => state.commandCalls), [], "no /role command is invoked");
  assert.deepEqual(fixture.states.flatMap(state => state.userMessages), [], "startup never calls sendUserMessage");
  assert.equal(fixture.peerErrors.length, 0);
  assert.equal(fixture.errors.length, 0);
});

test("post-ack context drift reports admission and attempts one authenticated Leave", async t => {
  let fixture;
  const hub = fakeHub(t, async () => {
    await fixture.setIdentity({ protocol: 1, server_key: "fixture-server", socket: `${fixture.directory}/hub.sock`, pane_id: "w1:p1", space_id: "w1", agent: "claude" });
    return startupHello;
  });
  fixture = await setup(t, { agent: "pi" });
  await fixture.h.emit("session_start", { reason: "startup" });

  assert.equal(hub.hellos.length, 1, "the grant-bearing Join is attempted once");
  assert.deepEqual(hub.requests.map(request => request.op), ["leave"], "local failure after admission attempts exactly one Leave and skips negotiation");
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
  assert.equal((await fixture.roleLookups()).length, 0, "identity is rechecked before local role resolution");
  const message = fixture.h.notifications.map(item => item.message).join("\n");
  assert.match(message, /admitted/i);
  assert.match(message, /hub acknowledged Leave/i);
  assert.equal(message.includes(token), false, "the credential is not reflected in cleanup status");
});

test("post-ack role resolution failure is cleaned up without retry", async t => {
  const hub = fakeHub(t);
  const fixture = await setup(t, { roles: [] });
  await fixture.h.emit("session_start", { reason: "startup" });

  assert.equal(hub.hellos.length, 1);
  assert.deepEqual(hub.requests.map(request => request.op), ["leave"], "an admitted participant is left exactly once after local role failure");
  assert.equal((await fixture.roleLookups()).length, 1);
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
  const message = fixture.h.notifications.map(item => item.message).join("\n");
  assert.match(message, /admitted/i);
  assert.match(message, /hub acknowledged Leave/i);
  assert.equal(message.includes(token), false);
});

test("post-ack negotiation failure is cleaned up without retry", async t => {
  const hub = fakeHub(t, undefined, async command => {
    if (command.op === "reset_capability") return { reset_protocol: 1 };
    if (command.op === "subscribe") throw new Error("fixture subscription rejected");
    if (command.op === "leave") return { left: true };
    return {};
  });
  const fixture = await setup(t);
  await fixture.h.emit("session_start", { reason: "startup" });

  assert.equal(hub.hellos.length, 1);
  assert.deepEqual(hub.requests.map(request => request.op), ["reset_capability", "subscribe", "leave"]);
  assert.equal((await fixture.roleLookups()).length, 1, "role resolution completed after the grant acknowledgement");
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
  const message = fixture.h.notifications.map(item => item.message).join("\n");
  assert.match(message, /admitted/i);
  assert.match(message, /hub acknowledged Leave/i);
  assert.equal(message.includes(token), false);
});

test("lost post-ack Leave is reported as uncertain and is never retried", async t => {
  const hub = fakeHub(t, undefined, async command => {
    if (command.op === "leave") throw new Error("fixture lost Leave acknowledgement");
    if (command.op === "reset_capability") return { reset_protocol: 1 };
    if (command.op === "subscribe") return { subscribed: true };
    return {};
  });
  const fixture = await setup(t, { roles: [] });
  await fixture.h.emit("session_start", { reason: "startup" });
  await delay(40);

  assert.equal(hub.hellos.length, 1);
  assert.deepEqual(hub.requests.map(request => request.op), ["leave"], "no negotiation or second cleanup attempt follows local role failure");
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
  const message = fixture.h.notifications.map(item => item.message).join("\n");
  assert.match(message, /admitted/i);
  assert.match(message, /cleanup could not be confirmed|inspect hub membership/i);
  assert.equal(message.includes(token), false);
});

test("startup does not resolve a role when the successful Join lacks its grant acknowledgement", async t => {
  const hub = fakeHub(t, async () => normalHello);
  const fixture = await setup(t, { roles: [] });
  await fixture.h.emit("session_start", { reason: "startup" });
  assert.equal(hub.hellos.length, 1, "the Join is attempted before local role lookup");
  assert.equal((await fixture.roleLookups()).length, 0);
  assert.deepEqual(fixture.h.active(), ["read", "bash", "policy-tool"]);
});
