import test from "node:test";
import assert from "node:assert/strict";
import net from "node:net";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { Agent } from "@earendil-works/pi-agent-core";
import { createAssistantMessageEventStream, InMemoryCredentialStore } from "@earendil-works/pi-ai";
import {
  AgentSession, convertToLlm, createExtensionRuntime, ModelRuntime, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";

// Real production adapter, AgentSession, ExtensionRunner, model/tool loop and
// retry/abort/settlement. Only the CLI, Unix hub peer and provider stream are fake.
// No resource discovery, saved sessions, live hub, provider request or model cost.
// The peer controls envelopes/state for adapter assertions, not Rust hub policy.
const model = {
  id: "mustr-liveness-fixture", name: "mustr-liveness-fixture",
  provider: "anthropic", api: "anthropic-messages", baseUrl: "",
  reasoning: false, input: ["text"], contextWindow: 100000, maxTokens: 128,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
};
const zeroUsage = {
  input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};
function deferred() {
  let resolve;
  const promise = new Promise(yes => { resolve = yes; });
  return { promise, resolve };
}
class Journal {
  entries = [];
  listeners = new Set();
  push(value) {
    this.entries.push(value);
    for (const listener of [...this.listeners]) listener();
    return value;
  }
  waitFor(predicate, description, after = 0) {
    return new Promise((resolve, reject) => {
      const finish = value => { clearTimeout(timer); this.listeners.delete(check); resolve(value); };
      const check = () => {
        const found = this.entries.slice(after).find(predicate);
        if (found !== undefined) finish(found);
      };
      const timer = setTimeout(() => {
        this.listeners.delete(check);
        reject(new Error(`Timed out: ${description}\n${JSON.stringify(this.entries.slice(-12), null, 2)}${this.diagnostics ? `\nRuntime diagnostics: ${JSON.stringify(this.diagnostics())}` : ""}`));
      }, 5000);
      this.listeners.add(check);
      check();
    });
  }
}
function assistant(content = [], stopReason = "stop", errorMessage) {
  const stream = createAssistantMessageEventStream();
  const message = {
    role: "assistant", content: [], api: model.api, provider: model.provider,
    model: model.id, usage: structuredClone(zeroUsage), stopReason: "pending", timestamp: Date.now(),
  };
  if (stopReason === "error" || stopReason === "aborted") {
    Object.assign(message, { stopReason, errorMessage });
    stream.push({ type: "error", reason: stopReason, error: message });
    return stream;
  }
  // Follow the real stream contract, including balanced content events. These
  // events are consumed by Pi, not emitted directly to mustr's registered hooks.
  stream.push({ type: "start", partial: message });
  for (const block of content) {
    const contentIndex = message.content.length;
    if (block.type === "toolCall") {
      const call = { ...block, arguments: {} };
      message.content.push(call);
      stream.push({ type: "toolcall_start", contentIndex, partial: message });
      call.arguments = block.arguments;
      stream.push({ type: "toolcall_delta", contentIndex, delta: JSON.stringify(block.arguments), partial: message });
      stream.push({ type: "toolcall_end", contentIndex, toolCall: call, partial: message });
    } else {
      const text = { type: "text", text: "" };
      message.content.push(text);
      stream.push({ type: "text_start", contentIndex, partial: message });
      text.text = block.text;
      stream.push({ type: "text_delta", contentIndex, delta: block.text, partial: message });
      stream.push({ type: "text_end", contentIndex, content: block.text, partial: message });
    }
  }
  message.stopReason = stopReason;
  stream.push({ type: "done", reason: stopReason, message });
  return stream;
}
const completed = () => assistant([{ type: "text", text: "Synthetic completion." }]);
const unavailable = () => assistant([], "error", "503 service unavailable");
const callTool = (name, args = {}, id = `fixture-${name}`) =>
  () => assistant([{ type: "toolCall", id, name, arguments: args }], "toolUse");
const report = (taskId, kind = "done", revision = 1) => callTool("mustr_report", {
  task_id: taskId, revision, kind, text: "Isolated fixture evidence; no external work.",
});
function blockedProvider() {
  const started = deferred();
  return {
    started: started.promise,
    step({ options }) {
      const stream = createAssistantMessageEventStream();
      const finish = () => {
        options.signal.removeEventListener("abort", finish);
        const message = {
          role: "assistant", content: [], api: model.api, provider: model.provider,
          model: model.id, usage: structuredClone(zeroUsage), stopReason: "aborted",
          errorMessage: "Synthetic request cancelled", timestamp: Date.now(),
        };
        stream.push({ type: "error", reason: "aborted", error: message });
      };
      if (options.signal.aborted) finish();
      else options.signal.addEventListener("abort", finish, { once: true });
      started.resolve();
      return stream;
    },
  };
}
function gatedProvider(content) {
  const started = deferred(), release = deferred();
  return {
    started: started.promise,
    release: () => release.resolve(),
    step({ options }) {
      const stream = createAssistantMessageEventStream();
      let finished = false;
      const abort = () => {
        if (finished) return;
        finished = true;
        options.signal.removeEventListener("abort", abort);
        const message = {
          role: "assistant", content: [], api: model.api, provider: model.provider,
          model: model.id, usage: structuredClone(zeroUsage), stopReason: "aborted",
          errorMessage: "Synthetic gated request cancelled", timestamp: Date.now(),
        };
        stream.push({ type: "error", reason: "aborted", error: message });
      };
      if (options.signal.aborted) abort();
      else options.signal.addEventListener("abort", abort, { once: true });
      started.resolve();
      void release.promise.then(async () => {
        if (finished) return;
        finished = true;
        options.signal.removeEventListener("abort", abort);
        for await (const event of assistant(content, "toolUse")) stream.push(event);
      });
      return stream;
    },
  };
}

async function fixture(t, { retryDelay = 1, profileLaunch = false, profileModel = model, requestedModel = `${profileModel.provider}/${profileModel.id}`, requestedThinking = "off", expectRefusal = false } = {}) {
  const directory = await mkdtemp(join(tmpdir(), "mustr-liveness-"));
  const identity = {
    protocol: 1, socket: join(directory, "peer.sock"), server_key: "synthetic-server",
    space_id: "fixture-space", pane_id: "fixture-pane",
  };
  let launchDescriptorPath, launchReadyPath;
  if (profileLaunch) {
    const launchDir = join(directory, "launch");
    await mkdir(launchDir, { mode: 0o700 });
    launchDescriptorPath = join(launchDir, "descriptor.json");
    launchReadyPath = join(launchDir, "ready.json");
    const descriptor = {
      version: 1, launch_id: "00000000-0000-4000-8000-000000000001", server_key: identity.server_key,
      space_id: identity.space_id, pane_id: identity.pane_id, tab_id: "fixture-tab", socket: identity.socket,
      profile: "team", slot: "researcher", instance: "team/researcher", tab_name: "Researcher",
      role: { name: "researcher", description: "Synthetic role", prompt: "Use only the synthetic fixture.", manager: false },
      model: requestedModel, thinking: requestedThinking, cwd: directory, config_dir: directory,
      state_dir: join(directory, "state"), binary: join(directory, "mustr"), descriptor_path: launchDescriptorPath,
      ready_path: launchReadyPath, spec_digest: "0123456789abcdef01234567",
    };
    await writeFile(launchDescriptorPath, JSON.stringify(descriptor), { mode: 0o400 });
  }
  const cli = join(directory, "mustr-fixture.mjs");
  await writeFile(cli, `#!${process.execPath}\nconst args = process.argv.slice(2);\nif (args.includes('context')) console.log(${JSON.stringify(JSON.stringify(identity))});\nelse if (args.includes('roles')) console.log(JSON.stringify([{ name: 'researcher', description: 'Isolated fixture', prompt: 'Operate only this synthetic fixture.' }]));\nelse process.exit(2);\n`, { mode: 0o700 });
  const envKeys = ["MUSTR_BIN", "MUSTR_CONFIG_DIR", "HERDR_ENV", "MUSTR_LAUNCH_DESCRIPTOR"];
  const previousEnv = new Map(envKeys.map(key => [key, process.env[key]]));
  Object.assign(process.env, { MUSTR_BIN: cli, MUSTR_CONFIG_DIR: directory, HERDR_ENV: "1" });
  if (launchDescriptorPath) process.env.MUSTR_LAUNCH_DESCRIPTOR = launchDescriptorPath;
  const calls = new Journal(), events = new Journal(), statuses = new Journal(), modelRequests = new Journal();
  const peerErrors = [], extensionErrors = [], unexpectedRequests = [], streamErrors = [], notices = [], operations = [];
  calls.diagnostics = () => ({ streamErrors, events: events.entries, status: statuses.entries.at(-1) });
  const tasks = new Map(), messages = [], sockets = new Set(), steps = [], lifecycleGates = new Set();
  const control = { handleInput: false, transformInput: undefined, rewriteMessageID: false, shuttingDown: false, settlementGate: undefined, retryStartGate: undefined, queueAtBeforeSettle: undefined, rejectBeforeExecution: new Map() };
  let session, runner, agentId, messageSequence = 0, syncSequence = 0;
  let nextAbortToolResult = false, abortPromise;
  const server = net.createServer(socket => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => {});
    let buffer = "", joined;
    socket.on("data", chunk => {
      buffer += chunk;
      while (buffer.includes("\n")) {
        const end = buffer.indexOf("\n"), line = buffer.slice(0, end);
        buffer = buffer.slice(end + 1);
        try {
          const request = JSON.parse(line);
          const record = { ...request, sender: joined, settled: events.entries.filter(event => event.type === "agent_settled").length };
          const rejections = control.rejectBeforeExecution.get(request.op) ?? 0;
          if (rejections > 0) {
            assert.ok(joined && request.op !== "join", "identity_unavailable only covers authenticated pre-execution requests");
            control.rejectBeforeExecution.set(request.op, rejections - 1);
            calls.push({ ...record, rejectedBeforeExecution: true });
            socket.write(`${JSON.stringify({ id: request.id, ok: false, error: "Synthetic unavailable topology; request was not executed", error_code: "identity_unavailable" })}\n`);
            continue;
          }
          let result;
          switch (request.op) {
            case "join":
              agentId = request.agent_id; joined = agentId;
              result = { delivery_policy: 1, inbox_notifications: 1, context_selection: 1, context_modes: ["reuse", "fresh"] };
              break;
            case "subscribe": result = { subscribed: true }; break;
            case "sync": {
              record.syncSequence = ++syncSequence;
              const inbox = request.receive && !control.shuttingDown ? messages.find(message => !message.received && !message.withdrawn
                && (!["assignment", "guide"].includes(message.kind) || request.activity === "idle")) : undefined;
              if (inbox) inbox.offered = true;
              const memberIDs = [...new Set([agentId, ...[...tasks.values()].flatMap(task => [task.owner, task.requester])])];
              result = {
                members: memberIDs.map(id => ({ id, name: id === agentId ? "fixture-participant" : id, role: "researcher", pane_id: id === agentId ? identity.pane_id : `fixture-${id}`, space_id: identity.space_id, activity: id === agentId ? request.activity : "idle" })),
                tasks: [...tasks.values()], tasks_truncated: false, automation: {},
                inbox: inbox ? envelope(inbox) : null,
                insertion_policy: inbox && ["assignment", "guide"].includes(inbox.kind) ? "idle_only" : "live",
              };
              break;
            }
            case "ack": {
              const message = messages.find(message => message.id === request.message_id);
              assert.ok(message?.offered && !message.received, "ack must follow this fixture's offer exactly once");
              message.received = true;
              if (["assignment", "guide"].includes(message.kind) && tasks.has(message.task_id)) tasks.get(message.task_id).state = "active";
              result = { received: true }; break;
            }
            case "defer": {
              const message = messages.find(message => message.id === request.message_id);
              assert.ok(message?.offered && !message.received);
              message.offered = false; result = { deferred: true }; break;
            }
            case "report": {
              const task = tasks.get(request.task_id);
              assert.ok(task, "report has a fixture task");
              assert.equal(task.owner, agentId);
              if (["done", "blocked", "waiting"].includes(request.kind)) task.state = request.kind === "done" ? "review" : request.kind;
              result = { recorded: true }; break;
            }
            case "accept":
            case "guide":
            case "cancel": {
              const task = tasks.get(request.task_id);
              assert.ok(task, "coordination has a fixture task");
              assert.equal(task.requester, agentId);
              task.state = request.op === "accept" ? "accepted" : request.op === "cancel" ? "cancelled" : "assigned";
              result = { recorded: true }; break;
            }
            case "send": result = { queued: true }; break;
            case "leave": result = { left: true }; break;
            default: throw new Error(`Unexpected fixture operation: ${request.op}`);
          }
          calls.push(record);
          socket.write(`${JSON.stringify({ id: request.id, ok: true, result })}\n`);
        } catch (error) { peerErrors.push(error); socket.destroy(); }
      }
    });
  });
  // Register cleanup before imports/runtime construction can fail. Adapter
  // shutdown must finish before its CLI/config/peer disappear; it awaits every
  // execFile discovery promise and therefore reaps those test-owned children.
  t.after(async () => {
    control.shuttingDown = true;
    for (const gate of lifecycleGates) gate.release.resolve();
    const cleanupErrors = [];
    const attempt = async action => { try { await action(); } catch (error) { cleanupErrors.push(error); } };
    try {
      if (session) await attempt(() => session.abort());
      if (abortPromise) await attempt(() => abortPromise);
      if (runner) await attempt(() => runner.emit({ type: "session_shutdown", reason: "shutdown" }));
      await attempt(() => session?.dispose());
    } finally {
      // Even a failed setup/abort cannot skip adapter shutdown or leave its
      // fake peer open. Never remove paths before those awaited boundaries.
      for (const socket of sockets) socket.destroy();
      if (server.listening) await attempt(() => new Promise(resolve => server.close(resolve)));
      for (const [key, value] of previousEnv) value === undefined ? delete process.env[key] : process.env[key] = value;
      await rm(directory, { recursive: true, force: true });
    }
    assert.deepEqual(cleanupErrors, [], "fixture shutdown errors");
    assert.deepEqual(peerErrors, [], "fixture protocol errors");
    assert.deepEqual(extensionErrors, [], "real extension runtime errors");
    assert.deepEqual(unexpectedRequests, [], "no adapter-generated retry/replay or unplanned model turn");
    assert.deepEqual(streamErrors, [], "synthetic provider assertion errors");
  });
  function envelope(message) {
    const { offered: _offered, received: _received, withdrawn: _withdrawn, ...value } = message;
    return value;
  }
  function hint() { for (const socket of sockets) socket.write('{"event":"inbox_ready"}\n'); }
  server.listen(identity.socket);
  await once(server, "listening");
  const runtime = createExtensionRuntime();
  const path = "<inline:production-mustr-liveness>";
  const sourceInfo = { path, source: "inline", scope: "temporary", origin: "top-level" };
  const extension = {
    path, resolvedPath: path, sourceInfo, handlers: new Map(), commands: new Map(), tools: new Map(),
    flags: new Map(), shortcuts: new Map(), messageRenderers: new Map(),
  };
  const registration = {
    on(name, handler) {
      const handlers = extension.handlers.get(name) ?? [];
      handlers.push(handler); extension.handlers.set(name, handlers);
      return () => { const index = handlers.indexOf(handler); if (index >= 0) handlers.splice(index, 1); };
    },
    registerTool(definition) { extension.tools.set(definition.name, { definition, sourceInfo }); },
    registerCommand(name, command) { extension.commands.set(name, { ...command, name, sourceInfo }); },
    registerFlag(name, options) { extension.flags.set(name, options); },
    getFlag(name) { return name === "mustr-launch" ? launchDescriptorPath : undefined; },
  };
  const api = new Proxy(registration, { get(target, key) {
    if (key in target) return target[key];
    return (...args) => {
      runtime.assertActive();
      const action = runtime[key];
      if (typeof action !== "function") throw new Error(`Unexpected adapter API: ${String(key)}`);
      return action(...args);
    };
  } });
  const { default: adapter } = await import(`../index.ts?liveness=${encodeURIComponent(directory)}`);
  await adapter(api);
  const observer = {
    ...extension, path: "<inline:liveness-observer>", resolvedPath: "<inline:liveness-observer>",
    tools: new Map(), commands: new Map(), handlers: new Map([
      ["input", [event => {
        events.push({ type: "input", source: event.source, text: event.text, streamingBehavior: event.streamingBehavior });
        if (control.handleInput) return { action: "handled" };
        if (control.transformInput) return { action: "transform", text: control.transformInput(event.text) };
      }]],
      ["message_end", [event => {
        if (control.rewriteMessageID && event.message.role === "custom" && event.message.customType === "mustr") {
          const original = event.message.details?.id;
          const stored = `rewritten-${original}`;
          events.push({ type: "custom_rewritten", original, stored });
          return { message: { ...event.message, details: { ...event.message.details, id: stored } } };
        }
      }]],
      ["agent_start", [async () => {
        const gate = control.retryStartGate;
        const errorCount = events.entries.filter(event => event.type === "assistant" && event.stopReason === "error").length;
        if (gate && errorCount > gate.afterErrorCount) {
          control.retryStartGate = undefined;
          gate.entered.resolve();
          await gate.release.promise;
        }
      }]],
      ["agent_before_settle", [async event => {
        events.push({ type: "agent_before_settle", outcome: event.outcome });
        if (event.outcome === "completed" && control.queueAtBeforeSettle) {
          const text = control.queueAtBeforeSettle; control.queueAtBeforeSettle = undefined;
          const result = await session.steer(text);
          events.push({ type: "boundary_steer_queued", text, result });
        }
        if (event.outcome === "completed" && control.settlementGate) {
          const gate = control.settlementGate; control.settlementGate = undefined;
          gate.entered.resolve();
          await gate.release.promise;
        }
      }]],
    ]),
  };
  const resourceLoader = {
    getExtensions: () => ({ extensions: [extension, observer], errors: [], runtime }),
    getSkills: () => ({ skills: [], diagnostics: [] }), getPrompts: () => ({ prompts: [], diagnostics: [] }),
    getThemes: () => ({ themes: [], diagnostics: [] }), getAgentsFiles: () => ({ agentsFiles: [] }),
    getSystemPrompt: () => "Isolated liveness fixture. No real files, network or external jobs.",
    getSystemPromptSource: () => undefined, getAppendSystemPrompt: () => [], getAppendSystemPromptSources: () => [],
    extendResources() { throw new Error("Resource discovery forbidden"); },
    async reload() { throw new Error("Resource reload forbidden"); },
  };
  const credentials = new InMemoryCredentialStore();
  // Auth is preflight-only. This value is never sent; streamFn is the only provider path.
  await credentials.modify("anthropic", async () => ({ type: "api_key", key: "synthetic-never-sent" }));
  const modelRuntime = await ModelRuntime.create({ credentials, modelsPath: null, allowModelNetwork: false, refreshOnCreate: false });
  if (profileLaunch) modelRuntime.registerNativeProvider({
    id: profileModel.provider, name: "Synthetic provider", auth: { apiKey: { name: "Synthetic key", resolve: async () => undefined } },
    getModels: () => [profileModel], stream: () => { throw new Error("Profile startup must not call a provider"); },
    streamSimple: () => { throw new Error("Profile startup must not call a provider"); },
  });
  const sessionManager = SessionManager.inMemory(directory);
  // Mirror createAgentSession's coding-agent message conversion. Agent-core's
  // default drops custom messages, which would make addressed wakes invisible
  // to the synthetic model despite a real SDK insertion/receipt.
  const agent = new Agent({ initialState: { model: profileModel }, sessionId: sessionManager.getSessionId(), convertToLlm, streamFn(_model, context, options) {
    const request = { index: modelRequests.entries.length, messages: structuredClone(context.messages) };
    modelRequests.push(request);
    const step = steps.shift();
    if (!step) { unexpectedRequests.push(request); return completed(); }
    try { return step({ context, options, session, request }); }
    catch (error) { streamErrors.push({ message: error.message, stack: error.stack }); throw error; }
  } });
  session = new AgentSession({
    agent, sessionManager,
    settingsManager: SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: true, maxRetries: 1, baseDelayMs: retryDelay }, cacheWarming: "off" }),
    resourceLoader, modelRuntime, cwd: directory, initialActiveToolNames: ["read", "bash"],
    baseToolsOverride: Object.fromEntries(["read", "bash"].map(name => [name, {
      name, label: name, description: "Synthetic review/read/test probe; no actual file access or process",
      parameters: { type: "object", properties: { scope: { type: "string" } }, required: ["scope"] },
      async execute(_id, args) {
        operations.push({ tool: name, scope: args.scope });
        return { content: [{ type: "text", text: `Verified fixture scope ${args.scope}` }], details: args };
      },
    }])),
  });
  session.refreshContext();
  await session.bindExtensions({
    mode: "tui", onError: error => extensionErrors.push(error),
    uiContext: {
      getEditorText: () => "",
      setStatus: (_key, text) => statuses.push({ text, syncSequence }),
      notify: (text, level) => notices.push({ text, level }),
    },
  });
  runner = session.extensionRunner;
  session.subscribe(event => {
    if (event.type === "message_start" && event.message.role === "custom" && event.message.customType === "mustr") {
      events.push({ type: "custom_started", id: event.message.details?.id });
    }
    if (event.type === "message_start" && event.message.role === "user") {
      const content = event.message.content;
      const text = typeof content === "string" ? content : content.filter(block => block.type === "text").map(block => block.text).join("\n");
      events.push({ type: "accepted_user", text });
    }
    if (event.type === "message_end" && event.message.role === "assistant") events.push({ type: "assistant", stopReason: event.message.stopReason });
    if (event.type === "message_end" && event.message.role === "toolResult") {
      events.push({ type: "tool_result", tool: event.message.toolName, isError: event.message.isError, text: event.message.content[0]?.text });
      if (nextAbortToolResult && event.message.toolName === "mustr_report") {
        nextAbortToolResult = false;
        abortPromise = session.abort();
      }
    }
    if (event.type === "agent_settled") events.push({ type: event.type, aborted: event.aborted });
    if (event.type === "auto_retry_start" || event.type === "auto_retry_end") events.push({ ...event });
  });
  if (!profileLaunch) await session.sendUserMessage("/role researcher fixture-participant", { expandPromptTemplates: true });
  if (!expectRefusal) await calls.waitFor(call => call.op === "sync", "production adapter admission").catch(error => {
    error.message += `\nAdapter diagnostics: ${JSON.stringify({ notices, statuses: statuses.entries, extensionErrors })}`;
    throw error;
  });
  return {
    session, calls, events, statuses, modelRequests, notices, operations, tasks, control,
    launchDescriptorPath, launchReadyPath,
    get id() { return agentId; },
    plan(...next) { steps.push(...next); },
    rejectOnceBeforeExecution(op) {
      control.rejectBeforeExecution.set(op, (control.rejectBeforeExecution.get(op) ?? 0) + 1);
    },
    withdrawUnstartedSteps(...expected) {
      assert.deepEqual(steps.splice(0, expected.length), expected, "only explicitly unstarted fixture steps may be withdrawn");
    },
    withdrawEnvelope(message) { message.withdrawn = true; },
    discardOptionalCompletion(step) {
      // A blocked tool may terminate its batch or get one normal explanatory
      // assistant reply. Both are safe; never discard an unexpected work step.
      if (steps[0] === step) steps.shift();
      else assert.equal(steps.includes(step), false);
    },
    command(text) { return session.sendUserMessage(text, { expandPromptTemplates: true }); },
    async prompt(text = "Synthetic human instruction.", options) {
      await session.prompt(text, options);
      await session.waitForIdle();
    },
    blockNextCompletedSettlement() {
      const gate = { entered: deferred(), release: deferred() };
      lifecycleGates.add(gate); control.settlementGate = gate;
      return gate;
    },
    blockNextAgentStartAfterError() {
      const gate = {
        entered: deferred(), release: deferred(),
        afterErrorCount: events.entries.filter(event => event.type === "assistant" && event.stopReason === "error").length,
      };
      lifecycleGates.add(gate); control.retryStartGate = gate;
      return gate;
    },
    abortNextReportResult() { nextAbortToolResult = true; },
    async finishAbort() { if (abortPromise) await abortPromise; },
    ownTask(id, state = "active", context = undefined) {
      const task = { id, requester: "fixture-manager", owner: agentId, space_id: identity.space_id, state, revision: 1, title: id, ...(context ? { context } : {}) };
      tasks.set(id, task); return task;
    },
    enqueue(value) {
      const message = { id: `fixture-message-${++messageSequence}`, space_id: identity.space_id, to: agentId, from: "fixture-manager", kind: "info", text: `Fixture message ${messageSequence}`, wake: true, ...value };
      messages.push(message); hint(); return message;
    },
    inserted(message) { return session.sessionManager.getBranch().some(entry => entry.type === "custom_message" && entry.customType === "mustr" && entry.details?.id === message.id); },
    ack(message) { return calls.waitFor(call => call.op === "ack" && call.message_id === message.id, `actual insertion ACK ${message.id}`); },
    async checkpoint(expectedActivity) {
      const after = calls.entries.length;
      hint();
      const sync = await calls.waitFor(call => call.op === "sync" && (!expectedActivity || call.activity === expectedActivity), "post-event adapter pump", after);
      await statuses.waitFor(status => status.syncSequence >= sync.syncSequence, "adapter pump status after sync reply");
      return sync;
    },
  };
}
const reports = (f, taskId) => f.calls.entries.filter(call => call.op === "report" && call.task_id === taskId);
const settled = f => f.events.entries.filter(event => event.type === "agent_settled");
const toolResults = (f, name) => f.events.entries.filter(event => event.type === "tool_result" && event.tool === name);
const currentStatus = f => f.statuses.entries.at(-1)?.text ?? "";
function timedTrace(f, t) {
  const startedAt = performance.now();
  const trace = [];
  const listener = () => {
    const event = f.events.entries.at(-1);
    if (!event || !["input", "accepted_user", "tool_result", "agent_settled"].includes(event.type)) return;
    trace.push({
      atMs: Number((performance.now() - startedAt).toFixed(1)),
      type: event.type, source: event.source, tool: event.tool,
      isError: event.isError, aborted: event.aborted,
    });
  };
  f.events.listeners.add(listener);
  t.after(() => f.events.listeners.delete(listener));
  return trace;
}

test("real Pi SDK profile startup joins from an explicit descriptor and writes a readiness receipt", { timeout: 15000 }, async t => {
  const f = await fixture(t, { profileLaunch: true });
  const join = await f.calls.waitFor(call => call.op === "join", "profile startup join");
  assert.equal(join.role, "researcher");
  assert.equal(join.pane_id, "fixture-pane");
  assert.equal(join.space_id, "fixture-space");
  assert.equal(f.calls.entries.some(call => call.op === "context"), false, "profile identity comes only from the validated launch descriptor");
  assert.equal(f.modelRequests.entries.length, 0, "startup/join makes no model/provider request");
  await f.calls.waitFor(call => call.op === "sync", "first profile roster sync after admission");
  const ready = JSON.parse(await readFile(f.launchReadyPath, "utf8"));
  assert.deepEqual(ready, {
    version: 1, launch_id: "00000000-0000-4000-8000-000000000001", server_key: "synthetic-server",
    space_id: "fixture-space", pane_id: "fixture-pane", agent_id: f.session.sessionManager.getSessionId(),
    role: "researcher", model: "anthropic/mustr-liveness-fixture", thinking: "off",
  });
});

for (const id of ["vendor/claude-sonnet-4", "us.anthropic.claude-sonnet-4-v1:0", "region/vendor/claude:0"]) {
  test(`profile startup preserves exact model suffix ${id}`, { timeout: 15000 }, async t => {
    const f = await fixture(t, { profileLaunch: true, profileModel: { ...model, id } });
    const ready = JSON.parse(await readFile(f.launchReadyPath, "utf8"));
    assert.equal(ready.model, `anthropic/${id}`);
    assert.equal(f.session.model.id, id);
    assert.equal(f.modelRequests.entries.length, 0);
  });
}

for (const mismatch of [
  { requestedModel: "anthropic/mustr-liveness-fixture/nested:0", error: /exact requested model/ },
  { requestedThinking: "high", error: /thinking .*not requested high/ },
]) {
  test(`profile startup refuses a mismatched ${mismatch.requestedModel ? "model" : "thinking level"}`, { timeout: 15000 }, async t => {
    const f = await fixture(t, { profileLaunch: true, expectRefusal: true, ...mismatch });
    assert.ok(f.notices.some(notice => notice.level === "error" && mismatch.error.test(notice.text)));
    assert.equal(f.calls.entries.some(call => call.op === "join"), false);
    await assert.rejects(readFile(f.launchReadyPath), { code: "ENOENT" });
    assert.equal(f.modelRequests.entries.length, 0);
  });
}

// Publication must cross REAL Pi settlement, not a hand-written settle(false).
test("real SDK: terminal report publishes once after settlement without pausing",  { timeout: 15000 }, async t => {
  const f = await fixture(t);
  f.ownTask("first-task");
  f.plan(report("first-task"));
  await f.prompt();
  const published = await f.calls.waitFor(call => call.op === "report", "settled terminal report publication");
  assert.equal(published.settled, 1);
  assert.deepEqual(settled(f), [{ type: "agent_settled", aborted: false }]);
  assert.deepEqual(toolResults(f, "mustr_report").map(result => result.isError), [false]);
  assert.equal(f.modelRequests.entries.length, 1, "terminate:true suppresses the automatic tool follow-up");
  assert.equal(f.tasks.get("first-task").state, "review");
  await f.checkpoint("idle");
  assert.equal(reports(f, "first-task").length, 1);
  assert.doesNotMatch(currentStatus(f), /manual pause|interrupted|awaiting human decision|unresolved error/);
});

test("real SDK: terminal report fences a native boundary continuation but releases later host tools", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const task = f.ownTask("same-native-run-report-fence");
  const queuedText = "SAME_NATIVE_RUN_CONTINUATION_AFTER_REPORT";
  f.control.queueAtBeforeSettle = queuedText;
  const optional = completed;
  f.plan(report(task.id), ({ context }) => {
    assert.match(JSON.stringify(context.messages), new RegExp(queuedText), "Pi actually processes the queued boundary continuation");
    return callTool("read", { scope: "same-native-run-after-report" })();
  }, optional);
  await f.prompt();
  const published = await f.calls.waitFor(call => call.op === "report" && call.task_id === task.id, "terminal report publishes after the fenced native continuation settles");
  await f.session.waitForIdle();
  assert.equal(published.settled, 1);
  assert.ok(f.events.entries.some(event => event.type === "boundary_steer_queued" && event.text === queuedText));
  assert.equal(f.modelRequests.entries.length, 2, "the same AgentSession run continued after the report result");
  assert.equal(toolResults(f, "read").at(-1)?.isError, true, "the continuation cannot execute productive task work");
  assert.deepEqual(f.operations, []);
  assert.equal(task.state, "review");
  f.discardOptionalCompletion(optional);

  f.plan(callTool("read", { scope: "separate-host-run-after-report-settlement" }), completed);
  await f.prompt("A separate later host turn can use ordinary Pi tools.");
  assert.deepEqual(f.operations, [{ tool: "read", scope: "separate-host-run-after-report-settlement" }]);
  assert.equal(toolResults(f, "read").at(-1)?.isError, false);
  assert.equal(task.state, "review", "later host work does not reopen the reported task revision");
});

test("real SDK: a later failed continuation invalidates an earlier successful report candidate", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const task = f.ownTask("candidate-report-failure");
  const queuedText = "REPORT_CANDIDATE_THEN_CONTINUATION_FAILURE";
  f.control.queueAtBeforeSettle = queuedText;
  f.plan(report(task.id), ({ context }) => {
    assert.match(JSON.stringify(context.messages), new RegExp(queuedText), "the run advances after its successful report candidate");
    return unavailable();
  }, unavailable);
  await f.prompt();
  await f.checkpoint("idle");
  assert.ok(f.events.entries.some(event => event.type === "agent_before_settle" && event.outcome === "completed"));
  assert.equal(f.events.entries.at(-1)?.type, "agent_settled");
  assert.deepEqual(settled(f), [{ type: "agent_settled", aborted: false }]);
  assert.ok(f.events.entries.filter(event => event.type === "agent_before_settle").some(event => event.outcome === "error"), "the later continuation's final candidate is failure");
  assert.equal(f.modelRequests.entries.length, 3, "one report response, one failed continuation, and Pi's one native retry");
  assert.equal(reports(f, task.id).length, 0, "candidate completion is not final publication evidence");
  assert.equal(task.state, "active");
  assert.doesNotMatch(currentStatus(f), /awaiting guidance/);
});

test("real SDK: pre-execution ACK rejection retains the persisted message without reinsertion or another model run", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  f.rejectOnceBeforeExecution("ack");
  f.plan(({ context }) => {
    assert.match(JSON.stringify(context.messages), /PERSIST_ONCE_THROUGH_TYPED_ACK_REJECTION/);
    return completed();
  });
  const message = f.enqueue({ text: "PERSIST_ONCE_THROUGH_TYPED_ACK_REJECTION" });
  await f.calls.waitFor(call => call.op === "ack" && call.message_id === message.id && call.rejectedBeforeExecution, "typed ACK rejection after actual SDK persistence");
  assert.equal(f.inserted(message), true);
  assert.equal(message.received, undefined, "the rejected ACK did not execute in the controlled peer");
  await f.session.waitForIdle();
  await f.calls.waitFor(call => call.op === "ack" && call.message_id === message.id && !call.rejectedBeforeExecution, "same-connection ACK after fresh identity validation");
  await f.checkpoint("idle");
  assert.equal(message.received, true);
  assert.equal(f.calls.entries.filter(call => call.op === "join").length, 1);
  assert.equal(f.calls.entries.filter(call => call.op === "ack" && call.message_id === message.id).length, 2, "only a definite pre-execution rejection permits the retained receipt to be dispatched again");
  assert.equal(f.session.sessionManager.getBranch().filter(entry => entry.type === "custom_message" && entry.customType === "mustr" && entry.details?.id === message.id).length, 1);
  assert.equal(f.events.entries.filter(event => event.type === "custom_started" && event.id === message.id).length, 1);
  assert.equal(f.modelRequests.entries.length, 1, "retaining the ACK never reinjects or reruns the addressed work");
  assert.deepEqual(settled(f), [{ type: "agent_settled", aborted: false }]);
});

test("real SDK: settled report survives typed pre-execution rejection and a later unrelated host abort", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const task = f.ownTask("typed-rejected-report");
  f.rejectOnceBeforeExecution("report");
  f.plan(report(task.id));
  await f.prompt();
  const rejected = await f.calls.waitFor(call => call.op === "report" && call.task_id === task.id && call.rejectedBeforeExecution, "typed rejection of the real settled report");
  assert.equal(rejected.settled, 1, "even the rejected publication waits for actual settlement");
  assert.equal(task.state, "active");
  assert.deepEqual(toolResults(f, "mustr_report").map(result => result.isError), [false]);

  const unrelatedProvider = blockedProvider();
  f.plan(unrelatedProvider.step);
  const unrelatedRun = f.session.prompt("An unrelated host turn starts while the settled report awaits retry.", { source: "extension" });
  void unrelatedRun.catch(() => {});
  await unrelatedProvider.started;
  await f.session.abort();
  await unrelatedRun;
  assert.equal(task.state, "active", "the unrelated host abort does not publish or discard the retained report");
  assert.equal(reports(f, task.id).filter(call => !call.rejectedBeforeExecution).length, 0, "the typed rejection is still the only report attempt before recovery");

  const published = await f.calls.waitFor(call => call.op === "report" && call.task_id === task.id && !call.rejectedBeforeExecution, "retained settled report publishes after fresh validation");
  await f.checkpoint("idle");
  assert.equal(published.settled, 2, "publication follows its successful report settlement and the later unrelated abort");
  for (const key of ["task_id", "revision", "kind", "text"]) assert.deepEqual(published[key], rejected[key], `retained report preserves ${key}`);
  assert.equal(task.state, "review");
  assert.equal(reports(f, task.id).length, 2);
  assert.equal(reports(f, task.id).filter(call => !call.rejectedBeforeExecution).length, 1, "the definitely rejected report was never published twice");
  assert.equal(f.calls.entries.filter(call => call.op === "join").length, 1);
  assert.equal(f.modelRequests.entries.length, 2, "the unrelated abort runs once; report publication does not rerun either model turn");
  assert.deepEqual(settled(f), [{ type: "agent_settled", aborted: false }, { type: "agent_settled", aborted: true }]);
});

test("real SDK: native 503 recovery remains reachable to the next addressed wake", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const boundary = f.blockNextCompletedSettlement();
  f.plan(unavailable, completed);
  const recovering = f.prompt();
  void recovering.catch(() => {}); // Await below; keep setup failures from becoming unhandled rejections.
  const finalOutcome = await f.events.waitFor(event => event.type === "agent_before_settle", "native retry reaches its final boundary");
  assert.equal(finalOutcome.outcome, "completed");
  await boundary.entered.promise;
  assert.deepEqual(f.events.entries.filter(event => event.type === "assistant").map(event => event.stopReason), ["error", "stop"]);
  assert.ok(f.events.entries.some(event => event.type === "auto_retry_end" && event.success));
  assert.deepEqual(settled(f), [], "a successful assistant reply is not final settlement yet");
  assert.equal(f.modelRequests.entries.length, 2, "only Pi's one native retry ran");
  f.plan(({ context }) => {
    assert.match(JSON.stringify(context.messages), /NEXT_AUTHORIZED_WAKE/);
    return completed();
  });
  const message = f.enqueue({ text: "NEXT_AUTHORIZED_WAKE" });
  const stillHeld = await f.checkpoint("working");
  assert.equal(stillHeld.receive, false, "fault clears only at final completed/nonaborted settlement");
  assert.equal(f.inserted(message), false);
  boundary.release.resolve();
  await recovering;
  const beforeWake = f.events.entries.findIndex(event => event.type === "agent_settled") + 1;
  assert.equal(settled(f)[0]?.aborted, false);
  await f.ack(message);
  await f.events.waitFor(event => event.type === "agent_settled", "wake-triggered run settles", beforeWake);
  await f.session.waitForIdle();
  assert.equal(f.inserted(message), true);
  assert.equal(f.modelRequests.entries.length, 3);
  await f.checkpoint("idle");
  assert.doesNotMatch(currentStatus(f), /unresolved error|interrupted|manual pause/);
});

test("real SDK: manual pause survives retry success and ordinary accepted input", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  await f.command("/mustr pause");
  f.plan(unavailable, completed);
  await f.prompt();
  assert.ok(f.events.entries.some(event => event.type === "auto_retry_end" && event.success));
  const message = f.enqueue({ text: "MANUAL_PAUSE_MUST_BLOCK" });
  const held = await f.checkpoint("paused");
  assert.equal(held.receive, false);
  assert.match(currentStatus(f), /manual pause/);
  assert.equal(f.inserted(message), false);
  f.plan(callTool("read", { scope: "manual-human-inspection" }), callTool("bash", { scope: "manual-human-tests" }), completed);
  await f.prompt("Accepted human input does not clear an explicit /mustr pause, but can do manual work.");
  assert.deepEqual(f.operations, [{ tool: "read", scope: "manual-human-inspection" }, { tool: "bash", scope: "manual-human-tests" }]);
  assert.equal(toolResults(f, "read").at(-1).isError, false);
  assert.equal(toolResults(f, "bash").at(-1).isError, false);
  await f.checkpoint("paused");
  assert.match(currentStatus(f), /manual pause/);
  assert.equal(f.inserted(message), false);
  f.plan(completed);
  await f.command("/mustr resume"); // Test-owned session only; intentional manual recovery.
  await f.ack(message);
  await f.session.waitForIdle();
  assert.equal(f.inserted(message), true);
});

test("real SDK: abort after a report discards its stage without locking later host work", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  f.ownTask("interrupted-report");
  f.abortNextReportResult();
  f.plan(report("interrupted-report"));
  await f.prompt();
  await f.finishAbort();
  assert.deepEqual(toolResults(f, "mustr_report").map(result => result.isError), [false]);
  assert.deepEqual(settled(f), [{ type: "agent_settled", aborted: true }]);
  assert.deepEqual(f.events.entries.filter(event => event.type === "assistant").map(event => event.stopReason), ["toolUse"]);
  await f.checkpoint("idle");
  assert.doesNotMatch(currentStatus(f), /interrupted|awaiting guidance/);
  assert.equal(reports(f, "interrupted-report").length, 0);

  f.plan(callTool("read", { scope: "ordinary-after-aborted-report" }), completed);
  const message = f.enqueue({ text: "AVAILABLE_AFTER_REPORT_ABORT" });
  await f.ack(message);
  await f.session.waitForIdle();
  await f.checkpoint("idle");
  assert.deepEqual(f.operations, [{ tool: "read", scope: "ordinary-after-aborted-report" }]);
  assert.equal(reports(f, "interrupted-report").length, 0, "the aborted terminal report is never replayed");
  assert.equal(f.tasks.get("interrupted-report").state, "active");
});

test("real SDK: explicit Mustr pause is delivery-only; input and abort are Pi-owned", { timeout: 20000 }, async t => {
  const f = await fixture(t);
  f.plan(callTool("mustr_pause", { reason: "Synthetic human decision needed" }));
  await f.prompt();
  await f.checkpoint("paused");
  assert.match(currentStatus(f), /awaiting human decision/);
  const message = f.enqueue({ text: "WAITING_FOR_EXPLICIT_MUSTR_RESUME" });
  const held = await f.checkpoint("paused");
  assert.equal(held.receive, false);
  assert.equal(f.inserted(message), false);

  f.plan(callTool("read", { scope: "ordinary-extension-turn-during-pause" }), completed);
  await f.prompt("Extension-origin user start does not resume Mustr delivery.", { source: "extension" });
  assert.deepEqual(f.operations, [{ tool: "read", scope: "ordinary-extension-turn-during-pause" }]);
  await f.checkpoint("paused");
  assert.match(currentStatus(f), /awaiting human decision/);
  assert.equal(f.inserted(message), false);

  f.plan(completed);
  const acceptedText = "Accepted human input also leaves the explicit Mustr pause in force.";
  await f.prompt(acceptedText);
  assert.ok(f.events.entries.some(event => event.type === "input" && event.source === "interactive" && event.text === acceptedText));
  assert.ok(f.events.entries.some(event => event.type === "accepted_user" && event.text === acceptedText));
  await f.checkpoint("paused");
  assert.match(currentStatus(f), /awaiting human decision/);
  assert.equal(f.inserted(message), false);

  await f.command("/mustr resume");
  f.plan(completed);
  await f.ack(message);
  await f.session.waitForIdle();
  await f.checkpoint("idle");
  assert.equal(f.inserted(message), true, "only the explicit Mustr command resumes coordination delivery");

  const provider = blockedProvider();
  f.plan(provider.step);
  const running = f.session.prompt("Start a stream that Pi will abort.", { source: "extension" });
  void running.catch(() => {});
  await provider.started;
  await f.session.abort();
  await running;
  await f.checkpoint("idle");
  assert.doesNotMatch(currentStatus(f), /interrupted/);
  f.plan(callTool("read", { scope: "ordinary-after-abort" }), completed);
  const afterAbort = f.enqueue({ text: "AVAILABLE_AFTER_ABORT" });
  await f.ack(afterAbort);
  await f.session.waitForIdle();
  await f.checkpoint("idle");
  assert.equal(f.inserted(afterAbort), true);
  assert.deepEqual(f.operations, [
    { tool: "read", scope: "ordinary-extension-turn-during-pause" },
    { tool: "read", scope: "ordinary-after-abort" },
  ]);
});

test("real SDK: Pi owns queued steering and abort state; Mustr does not gate ordinary tools", { timeout: 20000 }, async t => {
  const f = await fixture(t);
  const abortingProvider = blockedProvider();
  f.plan(abortingProvider.step);
  const abortingRun = f.session.prompt("Extension-origin run that Pi will abort.", { source: "extension" });
  void abortingRun.catch(() => {});
  await abortingProvider.started;
  await f.session.abort();
  await abortingRun;
  await f.checkpoint("idle");
  assert.doesNotMatch(currentStatus(f), /interrupted/);

  const trace = timedTrace(f, t);
  const oldTurn = gatedProvider([{ type: "toolCall", id: "old-turn-read", name: "read", arguments: { scope: "before-real-human-start" } }]);
  f.plan(oldTurn.step, callTool("bash", { scope: "after-real-human-start" }), completed);
  const running = f.session.prompt("Extension-origin host turn after abort.", { source: "extension" });
  void running.catch(() => {});
  await oldTurn.started;

  const steerText = "REAL_INTERACTIVE_STEER_FOR_INPUT_BOUNDARY";
  assert.equal(await f.session.steer(steerText), "queued");
  assert.equal(f.session.pendingMessageCount, 1);
  const pending = await f.checkpoint("working");
  assert.equal(pending.receive, false, "Pi-visible queued input still defers autonomous Mustr delivery");
  assert.equal(f.events.entries.some(event => event.type === "accepted_user" && event.text === steerText), false, "Pi has not emitted the queued user message start yet");

  oldTurn.release();
  await running;
  assert.equal(toolResults(f, "read").at(-1)?.isError, false, "an old-turn ordinary tool remains Pi-owned before input acceptance");
  assert.deepEqual(f.operations[0], { tool: "read", scope: "before-real-human-start" });

  // A terminated old turn may leave the steer queued. A later prompt lets Pi,
  // not mustr, decide when that text becomes a user message.
  if (!f.events.entries.some(event => event.type === "accepted_user" && event.text === steerText)) {
    await f.prompt("A later genuine human response follows the queued steer.");
  }
  await f.session.waitForIdle();
  await f.checkpoint("idle");
  assert.ok(f.events.entries.some(event => event.type === "accepted_user" && event.text === steerText));
  assert.deepEqual(f.operations, [
    { tool: "read", scope: "before-real-human-start" },
    { tool: "bash", scope: "after-real-human-start" },
  ]);
  assert.equal(toolResults(f, "bash").at(-1)?.isError, false);
  assert.doesNotMatch(currentStatus(f), /interrupted|input awaiting acceptance/);

  const interactiveInput = trace.findIndex(event => event.type === "input" && event.source === "interactive");
  const oldToolResult = trace.findIndex(event => event.type === "tool_result" && event.tool === "read");
  const acceptedUser = trace.findIndex((event, index) => index > interactiveInput && event.type === "accepted_user");
  const newToolResult = trace.findIndex(event => event.type === "tool_result" && event.tool === "bash");
  assert.ok(interactiveInput >= 0 && interactiveInput < oldToolResult);
  assert.ok(oldToolResult < acceptedUser && acceptedUser < newToolResult, "Pi accepts steering between host-owned tool work, without Mustr input attribution");
});

test("real SDK: handled input preflight does not poison a later ordinary host turn", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const trace = timedTrace(f, t);
  f.control.handleInput = true;
  const modelRequestsBeforeHandled = f.modelRequests.entries.length;
  await f.prompt("HANDLED_PREFLIGHT_WITHOUT_USER_MESSAGE");
  assert.equal(f.modelRequests.entries.length, modelRequestsBeforeHandled, "handled preflight ends before any agent turn or user message_start");
  assert.equal(f.events.entries.some(event => event.type === "accepted_user" && event.text === "HANDLED_PREFLIGHT_WITHOUT_USER_MESSAGE"), false);
  f.control.handleInput = false;

  f.plan(callTool("read", { scope: "after-handled-preflight" }), completed);
  await f.prompt("GENUINE_HUMAN_INPUT_AFTER_HANDLED_PREFLIGHT");
  await f.checkpoint("idle");

  const acceptedUser = trace.findIndex(event => event.type === "accepted_user");
  const readResult = trace.findIndex(event => event.type === "tool_result" && event.tool === "read");
  assert.ok(acceptedUser >= 0 && acceptedUser < readResult, "the later real user message precedes its ordinary tool result");
  assert.deepEqual(f.operations, [{ tool: "read", scope: "after-handled-preflight" }]);
  assert.equal(toolResults(f, "read").at(-1)?.isError, false);
  assert.doesNotMatch(currentStatus(f), /awaiting human decision|input awaiting acceptance|unresolved error/);
});

test("real SDK: clearQueue editor restoration does not poison the next ordinary host turn", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const provider = blockedProvider();
  f.plan(provider.step);
  const running = f.session.prompt("Extension-origin run to exercise queue restoration.", { source: "extension" });
  void running.catch(() => {});
  await provider.started;

  const queuedText = "QUEUED_HUMAN_STEER_RETURNED_TO_EDITOR";
  assert.equal(await f.session.steer(queuedText), "queued");
  assert.equal(f.session.pendingMessageCount, 1);
  const returnedToEditor = f.session.clearQueue();
  assert.deepEqual(returnedToEditor, { steering: [queuedText], followUp: [] });
  assert.equal(f.session.pendingMessageCount, 0);
  assert.equal(f.events.entries.some(event => event.type === "accepted_user" && event.text === queuedText), false, "clearQueue returns the text; it does not emit an accepted user message");

  await f.session.abort();
  await running;
  await f.checkpoint("idle");
  assert.doesNotMatch(currentStatus(f), /interrupted/);

  const trace = timedTrace(f, t);
  const completion = completed;
  f.plan(callTool("read", { scope: "after-editor-restored-human-input" }), completion);
  await f.prompt(queuedText);
  await f.session.waitForIdle();
  await f.checkpoint();
  f.discardOptionalCompletion(completion);

  const acceptedUser = trace.findIndex(event => event.type === "accepted_user");
  const readResult = trace.findIndex(event => event.type === "tool_result" && event.tool === "read");
  assert.ok(acceptedUser >= 0 && acceptedUser < readResult, "the restored text is accepted by Pi before ordinary tool execution");
  assert.deepEqual(f.operations, [{ tool: "read", scope: "after-editor-restored-human-input" }]);
  assert.equal(toolResults(f, "read").at(-1)?.isError, false);
  assert.doesNotMatch(currentStatus(f), /interrupted|input awaiting acceptance|unresolved error/);
});

test("real SDK: abort leaves queued steer pending until clearQueue explicitly restores it", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const provider = blockedProvider();
  f.plan(provider.step);
  const running = f.session.prompt("Extension-origin run to verify abort queue semantics.", { source: "extension" });
  void running.catch(() => {});
  await provider.started;
  const queuedText = "ABORT_MUST_NOT_CLEAR_THIS_QUEUED_STEER";
  assert.equal(await f.session.steer(queuedText), "queued");
  assert.equal(f.session.pendingMessageCount, 1);

  await f.session.abort();
  await running;
  assert.equal(f.session.pendingMessageCount, 1, "abort does not clear AgentSession's steering queue");
  assert.deepEqual(f.session.getSteeringMessages(), [queuedText]);
  assert.equal(f.events.entries.some(event => event.type === "accepted_user" && event.text === queuedText), false);

  const restored = f.session.clearQueue();
  assert.deepEqual(restored, { steering: [queuedText], followUp: [] }, "clearQueue returns queued text for the caller to restore to the editor");
  assert.equal(f.session.pendingMessageCount, 0);
});

test("real SDK: transformed human input leaves explicit Mustr pause intact while ordinary tools run", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  f.plan(callTool("mustr_pause", { reason: "Synthetic human decision before transformed input" }));
  await f.prompt();
  await f.checkpoint("paused");
  assert.match(currentStatus(f), /awaiting human decision/);
  const original = "A valid human prompt that a later extension transforms.";
  const transformed = "TRANSFORMED_VALID_HUMAN_RECOVERY";
  f.control.transformInput = text => text === original ? transformed : text;
  f.plan(({ context }) => {
    assert.match(JSON.stringify(context.messages), /TRANSFORMED_VALID_HUMAN_RECOVERY/);
    return callTool("read", { scope: "ordinary-tool-during-explicit-pause" })();
  }, completed);
  await f.prompt(original);
  assert.ok(f.events.entries.some(event => event.type === "input" && event.source === "interactive" && event.text === original));
  assert.ok(f.events.entries.some(event => event.type === "accepted_user" && event.text === transformed));
  assert.equal(f.events.entries.some(event => event.type === "accepted_user" && event.text === original), false, "the actual SDK accepted transformed text, not a matching copy of the input hook");
  await f.checkpoint("paused");
  assert.match(currentStatus(f), /awaiting human decision/);
  assert.deepEqual(f.operations, [{ tool: "read", scope: "ordinary-tool-during-explicit-pause" }]);
  assert.equal(toolResults(f, "read").at(-1)?.isError, false);
  const wake = f.enqueue({ text: "TRANSFORMED_RECOVERY_MAILBOX" });
  const held = await f.checkpoint("paused");
  assert.equal(held.receive, false);
  assert.equal(f.inserted(wake), false);
  await f.command("/mustr resume");
  f.plan(({ context }) => {
    assert.match(JSON.stringify(context.messages), /TRANSFORMED_RECOVERY_MAILBOX/);
    return completed();
  });
  await f.ack(wake);
  await f.session.waitForIdle();
  await f.checkpoint("idle");
  assert.equal(f.inserted(wake), true);
});

for (const cancel of [false, true]) test(`real SDK: ${cancel ? "retry cancellation" : "retry exhaustion"} clears its transient gate at final settlement`, { timeout: 15000 }, async t => {
  const f = await fixture(t, { retryDelay: cancel ? 1000 : 1 });
  if (cancel) {
    f.session.subscribe(event => {
      if (event.type === "auto_retry_start") {
        // The TUI uses abortRetry for Escape during backoff. A microtask waits
        // until Pi installs its abort controller, without a wall-clock sleep.
        queueMicrotask(() => f.session.abortRetry());
      }
    });
  }
  f.plan(unavailable, ...(!cancel ? [unavailable] : []));
  await f.prompt();
  const attempts = cancel ? 1 : 2;
  assert.equal(f.modelRequests.entries.length, attempts);
  assert.deepEqual(settled(f), [{ type: "agent_settled", aborted: false }]);
  assert.equal(f.events.entries.find(event => event.type === "auto_retry_end").success, false);
  assert.equal(f.events.entries.find(event => event.type === "agent_before_settle").outcome, "error");
  await f.checkpoint("idle");
  assert.doesNotMatch(currentStatus(f), /retrying after provider error|unresolved error/);
  assert.equal(f.modelRequests.entries.length, attempts, "mustr adds no retry after native settlement");

  f.plan(callTool("read", { scope: "ordinary-after-final-native-error" }), completed);
  const message = f.enqueue({ text: "WAKE_AFTER_FINAL_NATIVE_ERROR" });
  await f.ack(message);
  await f.session.waitForIdle();
  await f.checkpoint("idle");
  assert.equal(f.inserted(message), true, "autonomous coordination delivery resumes after the native final event");
  assert.deepEqual(f.operations, [{ tool: "read", scope: "ordinary-after-final-native-error" }]);
  assert.equal(f.modelRequests.entries.length, attempts + 2, "the addressed wake starts one tool call and one tool-result follow-up; mustr adds no recovery run");
  // Exhaustion and retry cancellation expose the same final event; the adapter
  // neither parses finalError nor intercepts raw Escape.
});





test("real SDK: child completion permits read/test review and acceptance without reopening reported work", { timeout: 20000 }, async t => {
  const f = await fixture(t);
  const parent = f.ownTask("reported-parent");
  f.plan(report(parent.id));
  await f.prompt();
  await f.calls.waitFor(call => call.op === "report" && call.task_id === parent.id, "parent report publication");
  assert.equal(parent.state, "review");
  const child = { id: "review-child", requester: f.id, owner: "fixture-child-worker", state: "review", revision: 1, title: "Review the synthetic child artifact" };
  f.tasks.set(child.id, child);
  const optionalExplanation = completed;
  f.plan(
    callTool("read", { scope: child.id }),
    callTool("bash", { scope: `${child.id}-tests` }),
    callTool("mustr_accept", { task_id: child.id, revision: 1, text: "Independent synthetic read/test probes checked." }),
    report(parent.id, "progress"),
    optionalExplanation,
  );
  const beforeChild = f.events.entries.length;
  const childDone = f.enqueue({ kind: "done", from: child.owner, task_id: child.id, revision: child.revision, text: "CHILD_ARTIFACT_READY_FOR_INDEPENDENT_REVIEW" });
  await f.ack(childDone);
  await f.calls.waitFor(call => call.op === "accept" && call.task_id === child.id, "requester accepts inspected child");
  await f.events.waitFor(event => event.type === "tool_result" && event.tool === "mustr_report" && event.isError, "old parent work claim is blocked", beforeChild);
  await f.session.waitForIdle();
  f.discardOptionalCompletion(optionalExplanation);
  assert.deepEqual(f.operations, [{ tool: "read", scope: child.id }, { tool: "bash", scope: `${child.id}-tests` }]);
  assert.equal(child.state, "accepted");
  assert.equal(parent.state, "review", "reviewing a child does not resume or accept the old assignment");
  assert.deepEqual(reports(f, parent.id).map(call => call.kind), ["done"], "old progress claim is rejected before any hub request");
  assert.equal(toolResults(f, "read").at(-1).isError, false);
  assert.equal(toolResults(f, "bash").at(-1).isError, false);

  // This is an explicit synthetic manager decision, not inferred completion.
  // Rust owns and separately tests acceptance/assignment authority.
  parent.state = "accepted";
  const next = f.ownTask("next-authorized-task", "assigned");
  f.plan(callTool("read", { scope: next.id }), report(next.id));
  const assignment = f.enqueue({ kind: "assignment", task_id: next.id, revision: 1, text: "NEXT_ASSIGNMENT_AFTER_SCOPED_CHILD_REVIEW" });
  await f.ack(assignment);
  await f.calls.waitFor(call => call.op === "report" && call.task_id === next.id, "next assignment completes without manual resume");
  await f.session.waitForIdle();
  await f.checkpoint("idle");
  assert.equal(f.inserted(assignment), true);
  assert.equal(next.state, "review");
  assert.equal(reports(f, next.id).length, 1);
  assert.deepEqual(reports(f, parent.id).map(call => call.kind), ["done"]);
  assert.deepEqual(f.operations.at(-1), { tool: "read", scope: next.id });
});

test("real SDK: exact taskless hub checkpoint permits reconciliation but preserves the old task fence", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const parent = f.ownTask("checkpoint-parent");
  f.plan(report(parent.id));
  await f.prompt();
  await f.calls.waitFor(call => call.op === "report", "parent stops before manager checkpoint");
  // A task-linked legacy worker check-in is not a manager checkpoint, even
  // though both arrive over the trusted peer and request a model wake.
  const optionalLinkedReply = completed;
  const linkedProbe = callTool("mustr_report", { task_id: parent.id, revision: 1, kind: "progress", text: "TASK_LINKED_CHECKIN_CANNOT_REOPEN_PARENT" });
  const beforeLinked = f.events.entries.length;
  f.plan(linkedProbe, optionalLinkedReply);
  const linked = f.enqueue({ kind: "checkin", from: null, task_id: parent.id, revision: 1, wake: true, text: "TASK_LINKED_WORKER_CHECKIN_IS_NOT_A_SCOPE" });
  const disposition = await f.calls.waitFor(call => ["ack", "defer"].includes(call.op) && call.message_id === linked.id, "task-linked check-in is deferred or constrained");
  if (disposition.op === "defer") {
    assert.equal(f.inserted(linked), false);
    // The controlled peer retires this stale check-in, not by fabricating an
    // insertion ACK. A later valid manager checkpoint can then be offered.
    f.withdrawEnvelope(linked);
    f.withdrawUnstartedSteps(linkedProbe, optionalLinkedReply);
  } else {
    await f.events.waitFor(event => event.type === "tool_result" && event.tool === "mustr_report" && event.isError, "task-linked check-in cannot reopen the stopped parent task", beforeLinked);
    await f.session.waitForIdle();
    f.discardOptionalCompletion(optionalLinkedReply);
  }
  assert.deepEqual(f.operations, []);
  assert.equal(parent.state, "review");

  const optionalExplanation = completed;
  f.plan(callTool("read", { scope: "manager-reconciliation" }), callTool("bash", { scope: "manager-reconciliation-tests" }), report(parent.id, "progress"), optionalExplanation);
  const afterParent = f.events.entries.length;
  const checkpoint = f.enqueue({ kind: "checkin", from: null, task_id: null, revision: null, wake: true, text: "TASKLESS_TRUSTED_HUB_MANAGER_CHECKPOINT" });
  await f.ack(checkpoint);
  await f.events.waitFor(event => event.type === "tool_result" && event.tool === "mustr_report" && event.isError, "checkpoint cannot reopen old assignment", afterParent);
  await f.session.waitForIdle();
  f.discardOptionalCompletion(optionalExplanation);
  assert.deepEqual(f.operations, [{ tool: "read", scope: "manager-reconciliation" }, { tool: "bash", scope: "manager-reconciliation-tests" }]);
  assert.equal(parent.state, "review");
  assert.deepEqual(reports(f, parent.id).map(call => call.kind), ["done"]);
});

test("real SDK: successful retry ending in toolUse can publish a terminal report and accept later guidance", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const task = f.ownTask("retry-to-report");
  f.plan(unavailable, report(task.id));
  await f.prompt();
  await f.calls.waitFor(call => call.op === "report" && call.task_id === task.id, "toolUse recovery publishes its terminal report");
  assert.deepEqual(f.events.entries.filter(event => event.type === "assistant").map(event => event.stopReason), ["error", "toolUse"]);
  assert.ok(f.events.entries.some(event => event.type === "auto_retry_end" && event.success));
  assert.equal(f.events.entries.find(event => event.type === "agent_before_settle").outcome, "completed");
  assert.equal(toolResults(f, "mustr_report").at(-1).isError, false);
  assert.deepEqual(settled(f), [{ type: "agent_settled", aborted: false }]);
  assert.equal(reports(f, task.id).length, 1);
  assert.equal(f.modelRequests.entries.length, 2);
  await f.checkpoint("idle");
  assert.doesNotMatch(currentStatus(f), /unresolved error/);

  // Explicit controlled-peer guidance revises this task; neither idle nor the
  // successful retry alone is authorization to resume the reported revision.
  task.state = "assigned"; task.revision = 2;
  f.plan(callTool("read", { scope: "authorized-revision-two" }), report(task.id, "done", 2));
  const guide = f.enqueue({ kind: "guide", task_id: task.id, revision: 2, text: "AUTHORIZED_GUIDANCE_AFTER_TOOL_USE_RECOVERY" });
  await f.ack(guide);
  await f.calls.waitFor(call => call.op === "report" && call.task_id === task.id && call.revision === 2, "guided revision completes without manual resume");
  await f.session.waitForIdle();
  assert.deepEqual(reports(f, task.id).map(call => [call.revision, call.kind]), [[1, "done"], [2, "done"]]);
  assert.deepEqual(f.operations, [{ tool: "read", scope: "authorized-revision-two" }]);
});

test("real SDK: ordinary waking request is acknowledged and communication stays live before a later guide", { timeout: 20000 }, async t => {
  const f = await fixture(t);
  const task = f.ownTask("request-parent");
  f.plan(report(task.id));
  await f.prompt();
  await f.calls.waitFor(call => call.op === "report", "parent is reported before ordinary request");
  f.plan(callTool("read", { scope: "ordinary-user-turn-after-terminal-report" }), completed);
  await f.prompt("A later ordinary host turn may use Pi tools without Mustr guidance.");
  assert.deepEqual(f.operations, [{ tool: "read", scope: "ordinary-user-turn-after-terminal-report" }]);
  await f.command("/mustr resume");
  assert.equal(task.state, "review", "resume clears only explicit pause state, not a published task revision");
  const optionalExplanation = completed;
  const beforeRequest = f.events.entries.length;
  f.plan(
    callTool("mustr_roster"),
    callTool("mustr_send", { to: "fixture-manager", text: "Request acknowledged; old task stays reported.", wake: false }),
    report(task.id, "status"),
    callTool("mustr_report", { task_id: task.id, revision: 1, kind: "progress", text: "ORDINARY_REQUEST_DOES_NOT_REOPEN_OLD_WORK" }),
    optionalExplanation,
  );
  const request = f.enqueue({ kind: "request", task_id: null, revision: null, text: "ORDINARY_ADDRESSED_REQUEST_IS_NOT_NEW_WORK_AUTHORIZATION" });
  await f.ack(request);
  await f.calls.waitFor(call => call.op === "report" && call.kind === "status", "communication-only status after request");
  await f.events.waitFor(event => event.type === "tool_result" && event.tool === "mustr_report" && event.isError, "ordinary request does not reopen the stopped task", beforeRequest);
  await f.session.waitForIdle();
  f.discardOptionalCompletion(optionalExplanation);
  assert.equal(f.inserted(request), true);
  assert.equal(f.calls.entries.filter(call => call.op === "send").length, 1);
  assert.equal(toolResults(f, "mustr_roster").at(-1).isError, false);
  assert.deepEqual(f.operations, [{ tool: "read", scope: "ordinary-user-turn-after-terminal-report" }]);
  assert.equal(task.state, "review");
  assert.deepEqual(reports(f, task.id).map(call => call.kind), ["done", "status"]);

  task.state = "assigned"; task.revision = 2;
  f.plan(callTool("read", { scope: "later-authorized-guide" }), report(task.id, "progress", 2), completed);
  const guide = f.enqueue({ kind: "guide", task_id: task.id, revision: 2, text: "LATER_GUIDE_MUST_NOT_BE_HEAD_OF_LINE_BLOCKED_BY_REQUEST" });
  await f.ack(guide);
  await f.calls.waitFor(call => call.op === "report" && call.task_id === task.id && call.revision === 2, "later authorized guidance remains reachable");
  await f.session.waitForIdle();
  assert.equal(f.inserted(guide), true);
  assert.deepEqual(f.operations, [
    { tool: "read", scope: "ordinary-user-turn-after-terminal-report" },
    { tool: "read", scope: "later-authorized-guide" },
  ]);
  assert.deepEqual(reports(f, task.id).map(call => [call.revision, call.kind]), [[1, "done"], [1, "status"], [2, "progress"]]);
});

test("real SDK: message_start cannot grant a review permit without matching persisted custom_message", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const parent = f.ownTask("persistence-fenced-parent");
  f.plan(report(parent.id));
  await f.prompt();
  await f.calls.waitFor(call => call.op === "report", "old task reported before scoped offer");
  const child = { id: "unconfirmed-child", requester: f.id, owner: "fixture-child-worker", state: "review", revision: 1, title: "Synthetic child review" };
  f.tasks.set(child.id, child);
  f.control.rewriteMessageID = true;
  const optionalExplanation = completed;
  f.plan(callTool("read", { scope: "ordinary-tool-without-task-permit" }), optionalExplanation);
  const beforeOffer = f.events.entries.length;
  const message = f.enqueue({ kind: "done", from: child.owner, task_id: child.id, revision: 1, text: "VALIDATED_BUT_NOT_MATCHINGLY_PERSISTED_SCOPE" });
  await f.events.waitFor(event => event.type === "custom_rewritten" && event.original === message.id, "later extension rewrites final custom-message identity", beforeOffer);
  await f.events.waitFor(event => event.type === "tool_result" && event.tool === "read" && !event.isError, "ordinary host tools do not require a task permit", beforeOffer);
  await f.session.waitForIdle();
  f.discardOptionalCompletion(optionalExplanation);
  assert.ok(f.events.entries.some(event => event.type === "custom_started" && event.id === message.id), "the real SDK did emit the tempting original message_start");
  assert.equal(f.inserted(message), false);
  assert.ok(f.session.sessionManager.getBranch().some(entry => entry.type === "custom_message" && entry.details?.id === `rewritten-${message.id}`));
  assert.equal(f.calls.entries.some(call => call.op === "ack" && call.message_id === message.id), false);
  assert.deepEqual(f.operations, [{ tool: "read", scope: "ordinary-tool-without-task-permit" }]);
  assert.match(currentStatus(f), /awaiting guidance/);
  assert.doesNotMatch(currentStatus(f), /reviewing delegated task/);
  assert.equal(parent.state, "review");
  assert.deepEqual(reports(f, parent.id).map(call => call.kind), ["done"]);
});
