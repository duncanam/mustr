import test from "node:test";
import assert from "node:assert/strict";
import net from "node:net";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { once } from "node:events";
import { Agent } from "@earendil-works/pi-agent-core";
import { InMemoryCredentialStore, createAssistantMessageEventStream, fauxAssistantMessage, fauxToolCall } from "@earendil-works/pi-ai";
import {
  AgentSession, convertToLlm, createAgentSessionRuntime, createExtensionRuntime,
  ModelRuntime, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";
import { FRESH_ASSIGNMENT_MARKER_KEY, FRESH_ASSIGNMENT_MARKER_SCHEMA, MUSTR_CONTEXT_CUSTOM_TYPE } from "../context-window.ts";

// Actual production adapter + actual pinned Pi lifecycle and replacement. Only the
// Unix socket peer and `mustr context/roles` CLI are fixture implementations.
// No discovery, credentials, provider/model call, saved session, or user hub.
const sentinel = "OLD PRODUCTION ADAPTER CONVERSATION SENTINEL";
const policy = "BASE POLICY: preserve private-path and source-control guards.";
const model = {
  id: "unused", name: "unused", provider: "openai", api: "openai-completions",
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

async function fixture(t, { cancelOrder, rejectReceipt = false, fakeProvider = false, allowPolicyTool = false, forceCustomTimestampGap = false, compactable = false } = {}) {
  const directory = await mkdtemp("/tmp/mustr-runtime-");
  const socketPath = `${directory}/peer.sock`;
  const identity = { protocol: 1, socket: socketPath, server_key: "fixture-server", space_id: "w1", pane_id: "w1:p1" };
  const cli = `${directory}/mustr.mjs`;
  await writeFile(`${directory}/identity.json`, JSON.stringify(identity));
  await writeFile(cli, `#!${process.execPath}\nimport {readFileSync} from 'node:fs';\nconst args=process.argv.slice(2);\nif(args.includes('context')) console.log(readFileSync(${JSON.stringify(`${directory}/identity.json`)},'utf8'));\nelse if(args.includes('roles')) console.log(JSON.stringify([{name:'researcher',description:'Fixture role',prompt:'APPROVED RESEARCH ROLE'}]));\nelse process.exit(2);\n`, { mode: 0o700 });
  const previousEnv = new Map(["MUSTR_BIN", "MUSTR_CONFIG_DIR", "HERDR_ENV"].map(key => [key, process.env[key]]));
  process.env.MUSTR_BIN = cli; process.env.MUSTR_CONFIG_DIR = directory; process.env.HERDR_ENV = "1";
  const { default: adapter } = await import(`../index.ts?runtime=${directory}`);
  const calls = [], sockets = new Set(), peerErrors = [];
  const freshAttempt = deferred(), releaseReceipt = deferred();
  let oldId, freshId, control, claimed = false, complete = false, streamCalls = 0, snapshotRevision = 0, runtime, tasks = [], inboxMessage, offeredMessage, policyToolCalls = 0, timestampGapApplied = false;
  const providerRequests = [], fakeResponses = [];
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
              result = { delivery_policy: 1, inbox_notifications: 1, reset_protocol: 1, context_selection: 1, context_modes: ["reuse", "fresh"] };
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
              result = { reset_receipt: true, agent_id: freshId, reset_protocol: 1, delivery_policy: 1, inbox_notifications: 1, context_selection: 1, context_modes: ["reuse", "fresh"] };
            }
            break;
          case "reset_capability": result = { reset_protocol: 1 }; break;
          case "subscribe": result = { subscribed: true }; break;
          case "sync": {
            const inbox = request.receive && inboxMessage ? inboxMessage : null;
            if (inbox) { offeredMessage = inboxMessage; inboxMessage = undefined; }
            result = { members: [{ id: joined, activity: "working", last_seen: ++snapshotRevision }], tasks: structuredClone(tasks), automation: {}, inbox: inbox ? structuredClone(inbox) : null, ...(inbox ? { insertion_policy: "idle_only" } : {}) };
            if (control && !complete) {
              result.reset_summary = { state: "incomplete", note: "Fixture history retained" };
              result.reset_control = { ...control };
            } else if (complete) result.reset_summary = { state: "completed" };
            break;
          }
          case "ack":
            assert.equal(offeredMessage?.id, request.message_id, "ACK follows the one actual insertion offer"); offeredMessage = undefined; result = { acked: true }; break;
          case "defer":
            assert.equal(offeredMessage?.id, request.message_id); inboxMessage = offeredMessage; offeredMessage = undefined; result = { deferred: true }; break;
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
  const credentials = new InMemoryCredentialStore();
  if (fakeProvider) await credentials.modify(model.provider, async () => ({ type: "api_key", key: "fixture-only-not-sent" }));
  const modelRuntime = await ModelRuntime.create({ credentials, modelsPath: null, allowModelNetwork: false, refreshOnCreate: false });
  const states = [], errors = [];

  async function createRuntime({ sessionManager, sessionStartEvent }) {
    const actions = createExtensionRuntime();
    const path = "<inline:production-mustr-runtime-test>";
    const sourceInfo = { path, source: "inline", scope: "temporary", origin: "top-level" };
    const extension = { path, resolvedPath: path, sourceInfo, commands: new Map(), handlers: new Map(), tools: new Map(), flags: new Map(), shortcuts: new Map(), messageRenderers: new Map() };
    const state = { actions, extension, notifications: [], dispatches: [], invocations: [], starts: [], statuses: [], contexts: [], contextInputs: [], messageStarts: [], disposed: false };
    states.push(state);
    // Public typed registration proxy: production chooses every handler/tool/command.
    // Action methods delegate to Pi's REAL runtime, not a simulated send/newSession.
    const registration = {
      on(name, handler) {
        const handlers = extension.handlers.get(name) ?? [];
        handlers.push(handler); extension.handlers.set(name, handlers);
        return () => { const i = handlers.indexOf(handler); if (i >= 0) handlers.splice(i, 1); };
      },
      registerFlag(name, options) { extension.flags.set(name, options); },
      getFlag() { return undefined; },
      registerTool(definition) { extension.tools.set(definition.name, { definition, sourceInfo }); },
      registerCommand(name, command) {
        extension.commands.set(name, { ...command, name, sourceInfo, async handler(args, ctx) {
          state.contexts.push(ctx); state.invocations.push({ name, args }); return command.handler(args, ctx);
        } });
      },
    };
    if (fakeProvider) {
      registration.on("context", event => state.contextInputs.push(structuredClone(event.messages)));
      registration.on("message_start", event => state.messageStarts.push(structuredClone(event.message)));
    }
    const api = new Proxy(registration, { get(target, key) {
      if (key in target) return target[key];
      return (...args) => {
        actions.assertActive();
        if (key === "sendUserMessage") state.dispatches.push(args[0]);
        if (key === "sendMessage" && forceCustomTimestampGap && args[0]?.customType === MUSTR_CONTEXT_CUSTOM_TYPE) {
          const realNow = Date.now;
          Date.now = () => realNow() - 60_000;
          timestampGapApplied = true;
          try { return actions[key](...args); }
          finally { Date.now = realNow; }
        }
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
    const settingsManager = SettingsManager.inMemory({ compaction: { enabled: false, ...(compactable ? { keepRecentTokens: 1 } : {}) }, retry: { enabled: false }, cacheWarming: "off" });
    const extensionRunnerRef = { current: undefined };
    const agent = new Agent({
      initialState: { model },
      convertToLlm,
      prepareRequest() {
        if (!fakeProvider) throw new Error("Provider requests forbidden");
      },
      transformContext: async messages => {
        const runner = extensionRunnerRef.current;
        return runner ? runner.emitContext(messages) : messages;
      },
      streamFn(_model, context) {
        streamCalls++;
        if (!fakeProvider) throw new Error("No model calls");
        providerRequests.push({
          messages: structuredClone(context.messages),
          tools: context.messages.flatMap(message => message.role === "system" ? (message.toolsAdded ?? []).map(tool => tool.name) : []),
        });
        const stream = createAssistantMessageEventStream();
        const response = fakeResponses.shift() ?? fauxAssistantMessage("Fixture provider response.");
        setTimeout(() => stream.end(response), 0);
        return stream;
      },
    });
    const session = new AgentSession({
      agent, sessionManager, settingsManager, resourceLoader, modelRuntime, cwd: directory, extensionRunnerRef,
      sessionStartEvent, initialActiveToolNames: ["policy-tool"], baseToolsOverride: {
        "policy-tool": { name: "policy-tool", label: "policy-tool", description: "Preserved policy guard", parameters: { type: "object", properties: {} }, async execute() {
          if (!allowPolicyTool) throw new Error("No policy tool execution");
          policyToolCalls++;
          return { content: [{ type: "text", text: "Host policy tool ran." }], details: {} };
        } },
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
    if (!fakeProvider) assert.equal(streamCalls, 0);
    assert.deepEqual(peerErrors, []); assert.deepEqual(errors, []);
  });
  await runtime.session.sendUserMessage("/role researcher runtime-researcher", { expandPromptTemplates: true });
  await until(() => calls.some(call => call.op === "sync"), "production adapter initial sync");
  return {
    runtime, states, manager, calls, token, errors, peerErrors, providerRequests, modelRuntime,
    async setTasks(value) { tasks = structuredClone(value); await states.at(-1).roster(); },
    async deliverFresh(taskId = "fresh-delivered-task") {
      const owner = runtime.session.sessionManager.getSessionId();
      tasks = [{ id: taskId, requester: "fixture-manager", owner, space_id: "w1", state: "assigned", revision: 1, title: taskId, context: "fresh" }];
      await states.at(-1).roster();
      inboxMessage = { id: `message-${taskId}`, kind: "assignment", from: "fixture-manager", to: owner, task_id: taskId, revision: 1, space_id: "w1", context: "fresh", wake: true, text: `Fresh assignment body ${taskId}.` };
      for (const socket of sockets) socket.write('{"event":"inbox_ready"}\n');
      await until(() => calls.some(call => call.op === "ack" && call.message_id === `message-${taskId}`), "actual Fresh assignment insertion ACK");
      return { owner, entry: runtime.session.sessionManager.getBranch().find(entry => entry.type === "custom_message" && entry.details?.id === `message-${taskId}`) };
    },
    enqueueFakeResponse(response) { fakeResponses.push(response); },
    policyToolCalls: () => policyToolCalls,
    timestampGapApplied: () => timestampGapApplied,
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

test("production adapter + real Pi runtime: Fresh replaces the native prefix, Reuse retains that window, and later Fresh replaces it", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const session = f.runtime.session, manager = session.sessionManager, owner = manager.getSessionId();
  manager.appendMessage({ role: "user", content: "PRIVATE BEFORE FRESH ASSIGNMENT", timestamp: Date.now() });
  const addFresh = (taskId, messageId) => {
    const marker = {
      schema: FRESH_ASSIGNMENT_MARKER_SCHEMA, authority: "authenticated-hub-assignment", kind: "assignment", mode: "fresh",
      taskId, revision: 1, messageId, nativeOwnerId: owner, nativeSessionId: owner, originSpaceId: "w1",
    };
    manager.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, `Fresh assignment ${taskId}`, true, {
      id: messageId, kind: "assignment", task_id: taskId, revision: 1, context: "fresh",
      [FRESH_ASSIGNMENT_MARKER_KEY]: marker,
    });
  };
  const task = (id, context) => ({ id, requester: "fixture-manager", owner, space_id: "w1", state: "active", revision: 1, title: id, context });

  await f.setTasks([task("fresh-A", "fresh")]);
  addFresh("fresh-A", "message-A"); session.refreshContext();
  const freshA = await session.extensionRunner.emitContext(session.messages);
  assert.doesNotMatch(JSON.stringify(freshA), /OLD PRODUCTION ADAPTER|PRIVATE BEFORE FRESH ASSIGNMENT/);
  assert.match(JSON.stringify(freshA), /Fresh assignment fresh-A/);

  await f.setTasks([task("reuse-B", "reuse")]);
  manager.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, "Reuse assignment reuse-B", true, {
    id: "message-B", kind: "assignment", task_id: "reuse-B", revision: 1, context: "reuse",
  });
  session.refreshContext();
  const reuseB = await session.extensionRunner.emitContext(session.messages);
  assert.doesNotMatch(JSON.stringify(reuseB), /OLD PRODUCTION ADAPTER|PRIVATE BEFORE FRESH ASSIGNMENT/);
  assert.match(JSON.stringify(reuseB), /Fresh assignment fresh-A/);
  assert.match(JSON.stringify(reuseB), /Reuse assignment reuse-B/);

  await f.setTasks([task("fresh-C", "fresh")]);
  addFresh("fresh-C", "message-C"); session.refreshContext();
  const freshC = await session.extensionRunner.emitContext(session.messages);
  assert.doesNotMatch(JSON.stringify(freshC), /OLD PRODUCTION ADAPTER|PRIVATE BEFORE FRESH ASSIGNMENT|Fresh assignment fresh-A|Reuse assignment reuse-B/);
  assert.match(JSON.stringify(freshC), /Fresh assignment fresh-C/);
});

test("production adapter + real Pi runtime: actual Fresh delivery exercises Pi's live/persisted custom timestamps", { timeout: 20000 }, async t => {
  const f = await fixture(t, { fakeProvider: true, forceCustomTimestampGap: true });
  const session = f.runtime.session, manager = session.sessionManager;
  manager.appendMessage({ role: "user", content: "PRIVATE_BEFORE_ACTUAL_FRESH_DELIVERY", timestamp: Date.now() });
  session.refreshContext();
  const delivered = await f.deliverFresh("actual-fresh-delivery");
  assert.ok(delivered.entry, "actual adapter insertion is present in the native branch");
  await session.waitForIdle();
  assert.equal(f.timestampGapApplied(), true, "test deterministically delayed the SDK AppMessage timestamp");
  assert.equal(f.calls.filter(call => call.op === "ack" && call.message_id === "message-actual-fresh-delivery").length, 1);
  assert.ok(f.providerRequests.length >= 1, "the actionable Fresh assignment reached the fake provider stream");
  const initial = f.providerRequests[0];
  assert.ok(Array.isArray(initial.messages));
  const persistedTimestamp = Date.parse(delivered.entry.timestamp);
  assert.ok(Number.isFinite(persistedTimestamp));
  const observed = f.states[0].contextInputs.find(messages => messages.some(message => message.role === "custom" && message.details?.id === "message-actual-fresh-delivery"));
  assert.ok(observed, "the real registered context hook ran on the actual provider path");
  const canonicalAssignment = observed.find(message => message.role === "custom" && message.details?.id === "message-actual-fresh-delivery");
  assert.equal(canonicalAssignment.timestamp, persistedTimestamp, "AgentSession request projection reaches context with the canonical persisted timestamp");
  const liveEventMessage = f.states[0].messageStarts.find(message => message.role === "custom" && message.details?.id === "message-actual-fresh-delivery");
  assert.ok(liveEventMessage, "the real AgentSession emitted message_start for the delivered Fresh message");
  assert.ok(Math.abs(liveEventMessage.timestamp - persistedTimestamp) >= 55_000, "the actual pre-persistence AppMessage timestamp differs, but is not the projected context source");
  const finalInput = JSON.stringify(initial.messages);
  assert.doesNotMatch(finalInput, /PRIVATE_BEFORE_ACTUAL_FRESH_DELIVERY/, "old conversation is excluded from the final fake-provider input");
  assert.match(finalInput, /Fresh assignment body actual-fresh-delivery/);
  assert.ok(finalInput.includes("BASE POLICY"), "current host system prompt remains in the final request");
  assert.ok(initial.tools.includes("policy-tool"), "host tools remain available in the final request");
  assert.ok(initial.tools.includes("mustr_roster"), "the selected Mustr role tools remain available");
  assert.equal(session.sessionManager.getSessionId(), delivered.owner, "Fresh preserves native session identity");
  assert.match(JSON.stringify(session.sessionManager.getEntries()), /PRIVATE_BEFORE_ACTUAL_FRESH_DELIVERY/, "raw native archive remains intact");
  assert.match(JSON.stringify(session.messages), /Fixture provider response/);
  assert.equal(f.providerRequests.length, 1, "one legitimate task turn executed without a retry/provider request");
});

test("production adapter + real Pi runtime: idle manual Fresh compaction failure cancels without poisoning retry or host tools", { timeout: 25000 }, async t => {
  const f = await fixture(t, { fakeProvider: true, allowPolicyTool: true, compactable: true });
  const session = f.runtime.session;
  session.sessionManager.appendMessage({ role: "user", content: "PRIVATE_BEFORE_MANUAL_COMPACT", timestamp: Date.now() });
  session.refreshContext();
  await f.deliverFresh("manual-compact-fresh");
  await session.waitForIdle();
  const initialCompactions = session.sessionManager.getBranch().filter(entry => entry.type === "compaction").length;

  await assert.rejects(session.compact(), /Compaction cancelled/);
  assert.equal(session.sessionManager.getBranch().filter(entry => entry.type === "compaction").length, initialCompactions,
    "a failed scoped manual summary is cancelled rather than falling through to native full-history compaction");
  assert.ok(f.states[0].notifications.some(note => /Fresh-scoped compaction was cancelled/.test(note)), "idle cancellation is visible without a staged task report");
  assert.equal(f.calls.some(call => call.op === "report"), false, "idle manual compaction does not fabricate a blocked report");
  const contextEventsBeforeRetry = f.states[0].contextInputs.length;
  f.enqueueFakeResponse(fauxAssistantMessage("Fresh context re-evaluated after cancelled idle compaction."));
  await session.sendUserMessage("Re-evaluate the active Fresh task after the cancelled manual compaction.");
  await session.waitForIdle();
  assert.ok(f.states[0].contextInputs.length > contextEventsBeforeRetry, "the actual public context hook reruns for the new provider request");
  const reevaluated = f.providerRequests.at(-1);
  assert.doesNotMatch(JSON.stringify(reevaluated.messages), /PRIVATE_BEFORE_MANUAL_COMPACT/);
  assert.match(JSON.stringify(reevaluated.messages), /Fresh assignment body manual-compact-fresh/);
  assert.match(JSON.stringify(reevaluated.messages), /Re-evaluate the active Fresh task/);
  assert.match(JSON.stringify(session.messages), /Fresh context re-evaluated after cancelled idle compaction/);

  // Retry the real public manual entry point with a deterministic local summary stream.
  f.modelRuntime.streamSimple = () => {
    const stream = createAssistantMessageEventStream();
    setTimeout(() => stream.end(fauxAssistantMessage("Fresh-scoped retry summary.")), 0);
    return stream;
  };
  await session.compact();
  assert.equal(session.sessionManager.getBranch().filter(entry => entry.type === "compaction").length, initialCompactions + 1,
    "valid re-evaluation can retry and commit a boundary-tagged scoped summary");
  const compactEntry = session.sessionManager.getBranch().find(entry => entry.type === "compaction");
  assert.match(JSON.stringify(compactEntry?.details), /mustrContextWindow/);

  await session.sendUserMessage("/mustr leave", { expandPromptTemplates: true });
  assert.equal(session.getActiveToolNames().includes("mustr_roster"), false);
  f.enqueueFakeResponse(fauxAssistantMessage(fauxToolCall("policy-tool", {})));
  f.enqueueFakeResponse(fauxAssistantMessage("Host turn completed after Mustr leave."));
  const requestsBeforeHost = f.providerRequests.length;
  await session.sendUserMessage("HOST request after Mustr leave");
  await session.waitForIdle();
  assert.equal(f.policyToolCalls(), 1, "ordinary host tools remain available after idle Fresh-compaction failure and leave");
  assert.ok(f.providerRequests.length >= requestsBeforeHost + 2, "the actual host turn and tool-result continuation reached the fake provider");
  assert.ok(f.providerRequests.slice(requestsBeforeHost).every(request => request.tools.includes("policy-tool")));
  assert.match(JSON.stringify(session.messages), /Host turn completed after Mustr leave/);
});

test("production adapter + real Pi runtime: missing Fresh marker stays fail-closed through leave until native settlement", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const session = f.runtime.session, manager = session.sessionManager, owner = manager.getSessionId();
  await f.setTasks([{ id: "fresh-without-marker", requester: "fixture-manager", owner, space_id: "w1", state: "active", revision: 1, title: "Missing marker", context: "fresh" }]);
  const runner = session.extensionRunner;
  await runner.emit({ type: "agent_start" });
  const blocked = await runner.emitContext(session.messages);
  assert.equal(blocked.length, 1);
  assert.match(JSON.stringify(blocked), /could not safely select this assignment context/);
  assert.doesNotMatch(JSON.stringify(blocked), /OLD PRODUCTION ADAPTER/);

  await session.sendUserMessage("/mustr leave", { expandPromptTemplates: true });
  const stillBlocked = await runner.emitContext(session.messages);
  assert.match(JSON.stringify(stillBlocked), /could not safely select this assignment context/);
  const toolCall = await runner.emitToolCall({ type: "tool_call", toolCallId: "fresh-fence", toolName: "read", input: { path: "unused" } });
  assert.equal(toolCall?.block, true);
  assert.equal(toolCall?.terminate, true);

  await runner.emit({ type: "agent_settled", aborted: false });
  assert.equal(await runner.emitToolCall({ type: "tool_call", toolCallId: "after-settlement", toolName: "read", input: { path: "unused" } }), undefined);
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
