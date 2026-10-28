import test from "node:test";
import assert from "node:assert/strict";
import net from "node:net";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { once } from "node:events";
import { Agent } from "@earendil-works/pi-agent-core";
import { InMemoryCredentialStore } from "@earendil-works/pi-ai";
import {
  AgentSession, createAgentSessionRuntime, createExtensionRuntime,
  ModelRuntime, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";

// Actual production adapter + actual pinned Pi lifecycle and replacement. Only the
// Unix socket peer and `mustr context/roles` CLI are fixture implementations.
// No discovery, credentials, provider/model call, saved session, or user hub.
const sentinel = "OLD PRODUCTION ADAPTER CONVERSATION SENTINEL";
const policy = "BASE POLICY: preserve private-path and source-control guards.";
const model = {
  id: "unused", name: "unused", provider: "runtime-test-no-provider", api: "unknown",
  baseUrl: "", reasoning: false, input: ["text"], contextWindow: 100000, maxTokens: 100,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
};
function deferred() {
  let resolve;
  const promise = new Promise(yes => { resolve = yes; });
  return { promise, resolve };
}
async function until(check, description) {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  throw new Error(`Timed out: ${description}`);
}

async function fixture(t, { cancelOrder, rejectReceipt = false } = {}) {
  const directory = await mkdtemp("/tmp/mustr-runtime-");
  const socketPath = `${directory}/peer.sock`;
  const identity = { socket: socketPath, server_key: "fixture-server", space_id: "w1", pane_id: "w1:p1" };
  const cli = `${directory}/mustr.mjs`;
  await writeFile(`${directory}/identity.json`, JSON.stringify(identity));
  await writeFile(cli, `#!${process.execPath}\nimport {readFileSync} from 'node:fs';\nconst args=process.argv.slice(2);\nif(args.includes('context')) console.log(readFileSync(${JSON.stringify(`${directory}/identity.json`)},'utf8'));\nelse if(args.includes('roles')) console.log(JSON.stringify([{name:'researcher',description:'Fixture role',prompt:'APPROVED RESEARCH ROLE'}]));\nelse process.exit(2);\n`, { mode: 0o700 });
  const previousEnv = new Map(["MUSTR_BIN", "MUSTR_CONFIG_DIR", "HERDR_ENV"].map(key => [key, process.env[key]]));
  process.env.MUSTR_BIN = cli; process.env.MUSTR_CONFIG_DIR = directory; process.env.HERDR_ENV = "1";
  const { default: adapter } = await import(`../index.ts?runtime=${directory}`);
  const calls = [], sockets = new Set(), peerErrors = [];
  const freshAttempt = deferred(), releaseReceipt = deferred();
  let oldId, freshId, control, claimed = false, complete = false, streamCalls = 0, snapshotRevision = 0, runtime;
  const token = "FIXTURE-PRIVATE-RESET-TICKET";
  const server = net.createServer(socket => {
    sockets.add(socket); socket.on("close", () => sockets.delete(socket)); socket.on("error", () => {});
    let buffer = "", joined;
    socket.on("data", chunk => {
      buffer += chunk;
      while (buffer.includes("\n")) {
        const end = buffer.indexOf("\n"), line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
        void respond(JSON.parse(line));
      }
    });
    async function respond(request) {
      calls.push({ ...request, sender: joined });
      let result;
      try {
        switch (request.op) {
          case "join":
            if (!request.reset) {
              assert.equal(oldId, undefined, "no automatic ordinary old-ID reconnect");
              oldId = request.agent_id; joined = oldId;
              assert.equal(request.role, "researcher"); assert.equal(request.name, "runtime-researcher");
              result = { delivery_policy: 1, inbox_notifications: 1, reset_protocol: 1 };
            } else {
              assert.equal(claimed, true, "fresh Join follows durable claim");
              assert.notEqual(request.agent_id, oldId);
              assert.deepEqual(request.reset, { plan_id: "fixture-plan", token });
              for (const field of ["role", "name", "pane_id", "space_id"]) assert.equal(request[field], control[field]);
              freshId = request.agent_id; joined = freshId; freshAttempt.resolve();
              await releaseReceipt.promise;
              if (rejectReceipt) {
                socket.write(JSON.stringify({ id: request.id, ok: false, error: "Receipt rejected by fixture authority" }) + "\n");
                return;
              }
              complete = true;
              result = { reset_receipt: true, agent_id: freshId, reset_protocol: 1, delivery_policy: 1, inbox_notifications: 1 };
            }
            break;
          case "reset_capability": result = { reset_protocol: 1 }; break;
          case "subscribe": result = { subscribed: true }; break;
          case "sync":
            result = { members: [{ id: joined, activity: "working", last_seen: ++snapshotRevision }], tasks: [], automation: {}, inbox: null };
            if (control && !complete) {
              result.reset_summary = { state: "incomplete", note: "Fixture history retained" };
              result.reset_control = { ...control };
            } else if (complete) result.reset_summary = { state: "completed" };
            break;
          case "reset_claim":
            assert.equal(joined, oldId); assert.equal(claimed, false);
            assert.equal(request.plan_id, control.plan_id); assert.equal(request.token, token);
            claimed = true; control.state = "claimed"; result = { claimed: true }; break;
          case "reset_status":
            assert.equal(request.token, token); control.state = request.status;
            result = { recorded: true }; break;
          case "leave": result = { left: true }; break;
          default: throw new Error(`Unexpected fixture operation ${request.op}`);
        }
        if (!socket.destroyed) socket.write(JSON.stringify({ id: request.id, ok: true, result }) + "\n");
      } catch (error) { peerErrors.push(error); socket.destroy(); }
    }
  });
  server.listen(socketPath); await once(server, "listening");
  const modelRuntime = await ModelRuntime.create({ credentials: new InMemoryCredentialStore(), modelsPath: null, allowModelNetwork: false, refreshOnCreate: false });
  const states = [], errors = [];

  async function createRuntime({ sessionManager, sessionStartEvent }) {
    const actions = createExtensionRuntime();
    const path = "<inline:production-mustr-runtime-test>";
    const sourceInfo = { path, source: "inline", scope: "temporary", origin: "top-level" };
    const extension = { path, resolvedPath: path, sourceInfo, commands: new Map(), handlers: new Map(), tools: new Map(), flags: new Map(), shortcuts: new Map(), messageRenderers: new Map() };
    const state = { actions, extension, notifications: [], dispatches: [], invocations: [], starts: [], statuses: [], contexts: [], disposed: false };
    states.push(state);
    // Public typed registration proxy: production chooses every handler/tool/command.
    // Action methods delegate to Pi's REAL runtime, not a simulated send/newSession.
    const registration = {
      on(name, handler) {
        const handlers = extension.handlers.get(name) ?? [];
        handlers.push(handler); extension.handlers.set(name, handlers);
        return () => { const i = handlers.indexOf(handler); if (i >= 0) handlers.splice(i, 1); };
      },
      registerFlag() {},
      getFlag() { return undefined; },
      registerTool(definition) { extension.tools.set(definition.name, { definition, sourceInfo }); },
      registerCommand(name, command) {
        extension.commands.set(name, { ...command, name, sourceInfo, async handler(args, ctx) {
          state.contexts.push(ctx); state.invocations.push({ name, args }); return command.handler(args, ctx);
        } });
      },
    };
    const api = new Proxy(registration, { get(target, key) {
      if (key in target) return target[key];
      return (...args) => {
        actions.assertActive();
        if (key === "sendUserMessage") state.dispatches.push(args[0]);
        return actions[key](...args);
      };
    } });
    await adapter(api);
    let promptOptions;
    extension.commands.set("runtime-probe", { name: "runtime-probe", sourceInfo, async handler(_args, ctx) { promptOptions = ctx.getSystemPromptOptions(); } });
    state.prompt = async () => {
      await state.session.sendUserMessage("/runtime-probe", { expandPromptTemplates: true });
      let rendered;
      const observers = extension.handlers.get("before_agent_start") ?? [];
      const capture = event => { rendered = event.systemPrompt; };
      observers.push(capture);
      try {
        const result = await state.session.extensionRunner.emitBeforeAgentStart({ messageType: "custom", message: { customType: "probe", content: "No model", display: false } }, promptOptions);
        return { text: rendered, options: result.systemPromptOptions };
      } finally { observers.splice(observers.indexOf(capture), 1); }
    };
    const observerPath = "<inline:runtime-lifecycle-observer>";
    const observer = { ...extension, path: observerPath, resolvedPath: observerPath, commands: new Map(), tools: new Map(), handlers: new Map([
      ["session_start", [(event, ctx) => { state.starts.push(event.reason); assert.equal("newSession" in ctx, false); }]],
      ["session_shutdown", [() => { state.disposed = true; }]],
      ["session_before_switch", [() => cancelOrder ? { cancel: true } : undefined]],
    ]) };
    const extensions = cancelOrder === "before" ? [observer, extension] : [extension, observer];
    const resourceLoader = {
      getExtensions: () => ({ extensions, errors: [], runtime: actions }),
      getSkills: () => ({ skills: [], diagnostics: [] }), getPrompts: () => ({ prompts: [], diagnostics: [] }),
      getThemes: () => ({ themes: [], diagnostics: [] }), getAgentsFiles: () => ({ agentsFiles: [] }),
      getSystemPrompt: () => policy, getSystemPromptSource: () => undefined,
      getAppendSystemPrompt: () => [], getAppendSystemPromptSources: () => [],
      extendResources() { throw new Error("Unexpected discovery"); }, async reload() { throw new Error("Unexpected discovery"); },
    };
    const settingsManager = SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: false }, cacheWarming: "off" });
    const agent = new Agent({ initialState: { model }, prepareRequest() { throw new Error("Provider requests forbidden"); }, streamFn() { streamCalls++; throw new Error("No model calls"); } });
    const session = new AgentSession({
      agent, sessionManager, settingsManager, resourceLoader, modelRuntime, cwd: directory,
      sessionStartEvent, initialActiveToolNames: ["policy-tool"], baseToolsOverride: {
        "policy-tool": { name: "policy-tool", label: "policy-tool", description: "Preserved policy guard", parameters: { type: "object", properties: {} }, async execute() { throw new Error("No policy tool execution"); } },
      },
    });
    session.refreshContext(); state.session = session;
    state.roster = () => extension.tools.get("mustr_roster").definition.execute("runtime-roster", {}, undefined);
    return { session, extensionsResult: resourceLoader.getExtensions(), diagnostics: [], services: { cwd: directory, agentDir: directory, modelRuntime, settingsManager, resourceLoader, diagnostics: [] } };
  }
  const manager = SessionManager.inMemory(directory);
  manager.appendMessage({ role: "user", content: sentinel, timestamp: Date.now() });
  manager.appendCustomMessageEntry("prior-mustr", sentinel, true);
  runtime = await createAgentSessionRuntime(createRuntime, { cwd: directory, agentDir: directory, sessionManager: manager });
  async function bind(session) {
    const state = states.at(-1);
    await session.bindExtensions({ mode: "tui", onError: error => errors.push(error),
      uiContext: { getEditorText: () => "", setStatus: (_key, value) => state.statuses.push(value), notify: message => state.notifications.push(message) },
      commandContextActions: {
        waitForIdle: () => session.waitForIdle(), newSession: options => runtime.newSession(options),
        fork: (id, options) => runtime.fork(id, options), switchSession: (path, options) => runtime.switchSession(path, options),
        async navigateTree() { throw new Error("Unused tree"); }, async reload() { throw new Error("Unused reload"); },
      },
    });
  }
  runtime.setRebindSession(bind); await bind(runtime.session);
  t.after(async () => {
    releaseReceipt.resolve();
    await runtime.dispose();
    for (const socket of sockets) socket.destroy();
    await new Promise(resolve => server.close(resolve));
    for (const [key, value] of previousEnv) { if (value === undefined) delete process.env[key]; else process.env[key] = value; }
    await rm(directory, { recursive: true, force: true });
    assert.equal(streamCalls, 0); assert.deepEqual(peerErrors, []); assert.deepEqual(errors, []);
  });
  await runtime.session.sendUserMessage("/role researcher runtime-researcher", { expandPromptTemplates: true });
  await until(() => calls.some(call => call.op === "sync"), "production adapter initial sync");
  return {
    runtime, states, manager, calls, token, errors, peerErrors,
    async start() {
      control = { plan_id: "fixture-plan", token, old_agent_id: oldId, name: "runtime-researcher", role: "researcher", space_id: "w1", pane_id: "w1:p1", state: "pending" };
      const roster = await states[0].roster();
      assert.equal(JSON.stringify(roster).includes(token), false, "raw roster control token stripped");
      assert.equal(JSON.stringify(roster).includes("reset_control"), false);
      return roster;
    },
    async awaitFreshAttempt() { await until(() => !!freshId, "fresh validated Join attempt"); await freshAttempt.promise; },
    releaseReceipt: () => releaseReceipt.resolve(),
  };
}

async function assertNoToken(f) {
  for (const state of f.states) {
    const context = await state.session.extensionRunner.emitContext(state.session.messages);
    const visible = { context, branch: state.session.sessionManager.getBranch(), dispatches: state.dispatches, invocations: state.invocations, notifications: state.notifications, statuses: state.statuses };
    assert.equal(JSON.stringify(visible).includes(f.token), false, "ticket never enters context/transcript/dispatch/UI");
  }
}

test("production adapter + real Pi runtime: receipt-gated fresh factory restores role and drops old context", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const original = f.runtime.session;
  await f.start();
  await f.awaitFreshAttempt();
  const fresh = f.runtime.session;
  assert.notEqual(fresh, original);
  assert.notEqual(fresh.sessionManager.getSessionId(), original.sessionManager.getSessionId());
  assert.equal(f.states.length, 2);
  assert.deepEqual(f.states.map(s => s.starts), [["startup"], ["new"]]);
  assert.equal(f.states[0].disposed, true);
  assert.deepEqual(fresh.getActiveToolNames(), ["policy-tool"], "no mustr role/tools before authoritative receipt");
  assert.equal((await fresh.extensionRunner.emitContext([])).some(message => message.customType === "mustr-state"), false);
  assert.equal(JSON.stringify(fresh.messages).includes(sentinel), false);
  assert.equal(JSON.stringify(fresh.sessionManager.getBranch()).includes(sentinel), false);
  assert.throws(() => f.states[0].actions.assertActive(), /stale/);
  assert.throws(() => f.states[0].contexts.at(-1).sessionManager, /stale/);
  // Old runtime must not be queried again; its saved branch remains a separate history.
  assert.match(JSON.stringify(f.manager.getBranch()), /OLD PRODUCTION ADAPTER/);
  f.releaseReceipt();
  await until(() => f.states[1].notifications.some(note => note.includes("fresh session verified")), "fresh role activation");
  assert.ok(fresh.getActiveToolNames().includes("policy-tool"));
  assert.ok(fresh.getActiveToolNames().includes("mustr_roster"));
  assert.match(fresh.systemPrompt, /BASE POLICY/);
  const promptResult = await f.states[1].prompt();
  assert.ok(promptResult.text.startsWith(policy));
  assert.match(promptResult.text, /Role: researcher/); assert.match(promptResult.text, /APPROVED RESEARCH ROLE/);
  assert.equal(promptResult.options.forceSystemPrompt, undefined, "mustr composes a named prompt section without flattening the base");
  assert.match(promptResult.options.sections.mustr_role, /explicitly joined mustr participant/);
  assert.match(promptResult.options.sections.mustr_role, /Role: researcher/);
  assert.match(promptResult.options.sections.mustr_role, /APPROVED RESEARCH ROLE/);
  const promptAgain = await f.states[1].prompt();
  assert.equal(promptAgain.options.sections.mustr_role, promptResult.options.sections.mustr_role, "role/common section stays static across runs");
  const firstRoster = await f.states[1].roster();
  const contextAfterFirstRoster = await fresh.extensionRunner.emitContext(fresh.messages);
  const secondRoster = await f.states[1].roster();
  const contextAfterSecondRoster = await fresh.extensionRunner.emitContext(fresh.messages);
  assert.notEqual(firstRoster.details.members[0].last_seen, secondRoster.details.members[0].last_seen, "fixture snapshot changes between syncs");
  assert.deepEqual(contextAfterFirstRoster, contextAfterSecondRoster, "transient roster snapshots never enter each-request context");
  assert.equal(contextAfterSecondRoster.some(message => message.customType === "mustr-state"), false);
  assert.equal(f.calls.filter(call => call.op === "reset_claim").length, 1);
  assert.equal(f.calls.filter(call => call.op === "join" && call.reset).length, 1);
  assert.equal(f.calls.some(call => call.op === "leave"), false, "authorized old shutdown never sends ordinary leave");
  assert.equal(f.calls.some(call => call.op === "reset_status"), false, "successful fresh receipt has no stale old-factory outcome");
  assert.equal(f.calls.filter(call => call.op === "reset_capability").length, 2);
  const roster = await f.states[1].roster();
  assert.equal(JSON.stringify(roster).includes(f.token), false);
  // Check fresh state only: asking Pi to transform context on a stale old runner
  // would itself violate the production contract under test.
  const freshContext = await fresh.extensionRunner.emitContext(fresh.messages);
  const visible = { freshContext, branch: fresh.sessionManager.getBranch(), dispatches: f.states.flatMap(s => s.dispatches), invocations: f.states.flatMap(s => s.invocations), notices: f.states.flatMap(s => s.notifications), statuses: f.states.flatMap(s => s.statuses) };
  assert.equal(JSON.stringify(visible).includes(f.token), false);
  assert.equal(JSON.stringify(visible).includes(sentinel), false);
  assert.deepEqual(f.errors, []);
});

for (const cancelOrder of ["before", "after"]) test(`production adapter + real Pi runtime: ${cancelOrder}-ordered veto preserves original context`, { timeout: 15000 }, async t => {
  const f = await fixture(t, { cancelOrder });
  const original = f.runtime.session, branch = original.sessionManager.getBranch();
  await f.start();
  await until(() => f.calls.some(call => call.op === "reset_status" && call.status === "cancelled"), "cancelled outcome");
  assert.equal(f.runtime.session, original);
  assert.deepEqual(original.sessionManager.getBranch(), branch);
  assert.equal(f.states.length, 1); assert.equal(f.states[0].disposed, false);
  assert.match(JSON.stringify(original.messages), /OLD PRODUCTION ADAPTER/);
  assert.equal(f.calls.filter(call => call.op === "reset_claim").length, 1);
  assert.equal(f.calls.some(call => call.op === "join" && call.reset), false);
  assert.equal(f.calls.some(call => call.op === "leave"), false);
  await f.states[0].roster();
  await original.sendUserMessage("/mustr-reset-internal invented-nonce", { expandPromptTemplates: true });
  assert.equal(f.calls.filter(call => call.op === "reset_claim").length, 1, "terminal cancellation never repeats replacement");
  await assertNoToken(f);
});

test("production adapter + real Pi runtime: denied fresh receipt never activates role/tools", { timeout: 15000 }, async t => {
  const f = await fixture(t, { rejectReceipt: true });
  await f.start(); await f.awaitFreshAttempt();
  f.releaseReceipt();
  await until(() => f.states[1].notifications.some(note => note.includes("suspended or unconfirmed")), "receipt rejection notice");
  const fresh = f.runtime.session;
  assert.deepEqual(fresh.getActiveToolNames(), ["policy-tool"]);
  assert.equal((await fresh.extensionRunner.emitContext([])).some(message => message.customType === "mustr-state"), false);
  const prompt = await f.states[1].prompt();
  assert.equal(prompt.text.includes("APPROVED RESEARCH ROLE"), false);
  assert.equal(prompt.options.sections.mustr_role, undefined);
  assert.equal(f.calls.filter(call => call.op === "reset_claim").length, 1);
  assert.equal(f.calls.filter(call => call.op === "join" && call.reset).length, 1);
  assert.equal(JSON.stringify({ messages: fresh.messages, branch: fresh.sessionManager.getBranch(), notices: f.states[1].notifications }).includes(f.token), false);
});
