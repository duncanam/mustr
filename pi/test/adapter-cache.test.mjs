import test from "node:test";
import assert from "node:assert/strict";
import net from "node:net";
import { once } from "node:events";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Agent } from "@earendil-works/pi-agent-core";
import {
  createAssistantMessageEventStream,
  getCurrentSystemPrompt,
  getCurrentTools,
  InMemoryCredentialStore,
} from "@earendil-works/pi-ai";
import {
  AgentSession,
  createExtensionRuntime,
  ModelRuntime,
  SessionManager,
  SettingsManager,
} from "@earendil-works/pi-coding-agent";

const basePolicy = "BASE POLICY: preserve the host's approved constraints.";
const appendPolicy = "APPENDED POLICY: keep the test session isolated.";
const hostSection = "HOST SECTION: remain within the synthetic fixture.";
const rolePrompt = "FIXTURE ROLE: follow the assigned role for this isolated test.";
const fakeModel = {
  id: "mustr-cache-context-fixture",
  name: "mustr-cache-context-fixture",
  provider: "anthropic",
  api: "anthropic-messages",
  baseUrl: "",
  reasoning: false,
  input: ["text"],
  contextWindow: 100000,
  maxTokens: 128,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
};
const zeroUsage = {
  input: 0,
  output: 0,
  cacheRead: 0,
  cacheWrite: 0,
  totalTokens: 0,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

function makeExtension(path) {
  const sourceInfo = { path, source: "inline", scope: "temporary", origin: "top-level" };
  return {
    path,
    resolvedPath: path,
    sourceInfo,
    commands: new Map(),
    handlers: new Map(),
    tools: new Map(),
    flags: new Map(),
    shortcuts: new Map(),
    messageRenderers: new Map(),
  };
}

// Capture at pi-agent-core's StreamFn boundary, after Pi's context transform and
// AgentMessage-to-Message conversion. This is SDK model context, not a serialized
// provider HTTP payload or evidence of cache accounting.
function captureModelContext(context) {
  return {
    system: getCurrentSystemPrompt(context.messages),
    tools: getCurrentTools(context.messages).map(({ name, description, parameters }) => ({
      name,
      description,
      parameters: structuredClone(parameters),
    })),
    messages: structuredClone(context.messages),
  };
}

function syntheticResponse(model, content, stopReason) {
  const message = {
    role: "assistant",
    content,
    api: model.api,
    provider: model.provider,
    model: model.id,
    usage: structuredClone(zeroUsage),
    stopReason,
    timestamp: Date.now(),
  };
  const stream = createAssistantMessageEventStream();
  stream.push({ type: "done", reason: stopReason, message });
  return stream;
}

function rosterSnapshotFromResult(message) {
  assert.equal(message.role, "toolResult");
  assert.equal(message.toolName, "mustr_roster");
  const text = message.content.find(block => block.type === "text")?.text;
  assert.equal(typeof text, "string");
  return JSON.parse(text);
}

async function until(check, description) {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  throw new Error(`Timed out waiting for ${description}`);
}

test("production mustr adapter preserves real Pi model-context prefixes across tool continuations and runs", { timeout: 15000 }, async t => {
  const directory = await mkdtemp(join(tmpdir(), "mustr-cache-context-"));
  const socketPath = join(directory, "hub.sock");
  const identityPath = join(directory, "identity.json");
  const cliPath = join(directory, "mustr-fixture.mjs");
  const identity = {
    protocol: 1,
    socket: socketPath,
    server_key: "synthetic-server",
    space_id: "synthetic-space",
    pane_id: "synthetic-pane",
  };
  await writeFile(identityPath, JSON.stringify(identity));
  await writeFile(cliPath, `#!${process.execPath}\nimport { readFileSync } from 'node:fs';\nconst args = process.argv.slice(2);\nif (args.includes('context')) console.log(readFileSync(${JSON.stringify(identityPath)}, 'utf8'));\nelse if (args.includes('roles')) console.log(JSON.stringify([{ name: 'researcher', description: 'Synthetic fixture role', prompt: ${JSON.stringify(rolePrompt)} }]));\nelse process.exit(2);\n`, { mode: 0o700 });

  const envKeys = ["MUSTR_BIN", "MUSTR_CONFIG_DIR", "HERDR_ENV"];
  const previousEnv = new Map(envKeys.map(key => [key, process.env[key]]));
  process.env.MUSTR_BIN = cliPath;
  process.env.MUSTR_CONFIG_DIR = directory;
  process.env.HERDR_ENV = "1";

  const calls = [];
  const syncSnapshots = [];
  const sockets = new Set();
  const hubErrors = [];
  let revision = 0;
  const server = net.createServer(socket => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => {});
    let buffer = "";
    socket.on("data", chunk => {
      buffer += chunk;
      while (buffer.includes("\n")) {
        const end = buffer.indexOf("\n");
        const line = buffer.slice(0, end);
        buffer = buffer.slice(end + 1);
        let request;
        try { request = JSON.parse(line); }
        catch (error) { hubErrors.push(error); socket.destroy(); return; }
        calls.push(request);
        let result;
        switch (request.op) {
          case "join":
            result = { delivery_policy: 1, inbox_notifications: 1, context_selection: 1, context_modes: ["reuse", "fresh"] };
            break;
          case "subscribe":
            result = { subscribed: true };
            break;
          case "sync": {
            const snapshot = {
              members: [{
                id: "synthetic-member",
                name: "synthetic-peer",
                role: "researcher",
                space_id: "synthetic-space",
                pane_id: "synthetic-pane",
                activity: "idle",
                last_seen: ++revision,
              }],
              tasks: [],
              automation: {},
              inbox: null,
            };
            syncSnapshots.push(structuredClone(snapshot));
            result = snapshot;
            break;
          }
          case "leave":
            result = { left: true };
            break;
          default:
            hubErrors.push(new Error(`Unexpected synthetic hub operation: ${request.op}`));
            socket.destroy();
            return;
        }
        if (!socket.destroyed) socket.write(`${JSON.stringify({ id: request.id, ok: true, result })}\n`);
      }
    });
  });
  server.listen(socketPath);
  await once(server, "listening");

  let session;
  let modelRuntime;
  let extensionRunner;
  let adapterExtension;
  let adapterApi;
  const views = [];
  const promptObservations = [];
  const sessionErrors = [];
  const statuses = [];
  const notices = [];

  t.after(async () => {
    if (extensionRunner) {
      try { await extensionRunner.emit({ type: "session_shutdown", reason: "shutdown" }); }
      catch { /* Cleanup remains best-effort if setup failed partway through. */ }
    }
    session?.dispose();
    for (const socket of sockets) socket.destroy();
    await new Promise(resolve => server.close(resolve));
    for (const [key, value] of previousEnv) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    await rm(directory, { recursive: true, force: true });
  });

  const { default: mustrAdapter } = await import(`../index.ts?cache-context=${encodeURIComponent(directory)}`);
  const extensionRuntime = createExtensionRuntime();
  adapterExtension = makeExtension("<inline:mustr-cache-context>");
  const registration = {
    on(name, handler) {
      const handlers = adapterExtension.handlers.get(name) ?? [];
      handlers.push(handler);
      adapterExtension.handlers.set(name, handlers);
    },
    registerTool(definition) {
      adapterExtension.tools.set(definition.name, { definition, sourceInfo: adapterExtension.sourceInfo });
    },
    registerCommand(name, command) {
      adapterExtension.commands.set(name, { ...command, name, sourceInfo: adapterExtension.sourceInfo });
    },
    registerFlag(name, options) {
      adapterExtension.flags.set(name, options);
    },
    getFlag() { return undefined; },
  };
  adapterApi = new Proxy(registration, {
    get(target, key) {
      if (key in target) return target[key];
      return (...args) => {
        extensionRuntime.assertActive();
        const action = extensionRuntime[key];
        if (typeof action !== "function") throw new Error(`Unexpected adapter API call: ${String(key)}`);
        return action(...args);
      };
    },
  });
  await mustrAdapter(adapterApi);
  assert.equal((adapterExtension.handlers.get("context") ?? []).length, 1, "production adapter projects only the task-authorized native context window");

  const observerExtension = makeExtension("<inline:cache-context-observer>");
  observerExtension.handlers.set("before_agent_start", [event => {
    event.systemPromptOptions.sections.host_policy = hostSection;
    promptObservations.push({
      forceSystemPrompt: event.systemPromptOptions.forceSystemPrompt,
      sections: structuredClone(event.systemPromptOptions.sections),
      rendered: event.systemPrompt,
    });
  }]);

  const resourceLoader = {
    getExtensions: () => ({ extensions: [adapterExtension, observerExtension], errors: [], runtime: extensionRuntime }),
    getSkills: () => ({ skills: [], diagnostics: [] }),
    getPrompts: () => ({ prompts: [], diagnostics: [] }),
    getThemes: () => ({ themes: [], diagnostics: [] }),
    getAgentsFiles: () => ({ agentsFiles: [] }),
    getSystemPrompt: () => basePolicy,
    getSystemPromptSource: () => undefined,
    getAppendSystemPrompt: () => [appendPolicy],
    getAppendSystemPromptSources: () => [],
    extendResources() { throw new Error("Unexpected resource discovery in synthetic test"); },
    async reload() { throw new Error("Unexpected resource reload in synthetic test"); },
  };
  // AgentSession preflight needs configured auth; this fake in-memory value is
  // never used by a provider because Agent.streamFn below is the only stream path.
  const credentials = new InMemoryCredentialStore();
  await credentials.modify("anthropic", async () => ({ type: "api_key", key: "synthetic-test-only" }));
  modelRuntime = await ModelRuntime.create({
    credentials,
    modelsPath: null,
    allowModelNetwork: false,
    refreshOnCreate: true,
  });
  const settingsManager = SettingsManager.inMemory({
    compaction: { enabled: false },
    retry: { enabled: false },
    cacheWarming: "off",
  });
  const agent = new Agent({
    initialState: { model: fakeModel },
    streamFn(model, context) {
      const call = views.length + 1;
      const view = captureModelContext(context);
      views.push(view);
      const shouldListRoster = call === 1 || call === 3;
      if (shouldListRoster) {
        assert.ok(view.tools.some(tool => tool.name === "mustr_roster"), "joined role tool is declared to the model");
        return syntheticResponse(model, [{ type: "toolCall", id: `fixture-roster-${call}`, name: "mustr_roster", arguments: {} }], "toolUse");
      }
      return syntheticResponse(model, [{ type: "text", text: `synthetic completion ${call}` }], "stop");
    },
  });
  session = new AgentSession({
    agent,
    sessionManager: SessionManager.inMemory(directory),
    settingsManager,
    resourceLoader,
    modelRuntime,
    cwd: directory,
    initialActiveToolNames: [],
  });
  session.refreshContext();
  await session.bindExtensions({
    mode: "tui",
    onError: error => sessionErrors.push(error),
    uiContext: {
      getEditorText: () => "",
      setStatus: (_key, value) => statuses.push(value),
      notify: (message, level) => notices.push({ message, level }),
    },
  });
  extensionRunner = session.extensionRunner;

  await session.sendUserMessage("/role researcher cache-prefix-fixture", { expandPromptTemplates: true });
  await until(() => calls.some(call => call.op === "sync") && session.getActiveToolNames().includes("mustr_roster"), "synthetic role admission and first roster sync");

  await session.prompt("First ordinary prompt for the SDK context-prefix test.");
  await session.prompt("Second ordinary prompt for the SDK context-prefix test.");

  assert.equal(views.length, 4, "each of two ordinary runs includes one real tool continuation");
  assert.equal(promptObservations.length, 2, "before_agent_start runs once at each accepted run boundary");
  const activeRoleSection = promptObservations[0].sections.mustr_role;
  assert.match(activeRoleSection, /explicitly joined mustr participant/);
  assert.match(activeRoleSection, /Role: researcher/);
  assert.match(activeRoleSection, new RegExp(rolePrompt.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  assert.equal(promptObservations[0].forceSystemPrompt, undefined, "adapter composes structured sections rather than forcing a full prompt");
  assert.equal(promptObservations[1].forceSystemPrompt, undefined);
  assert.equal(promptObservations[1].sections.mustr_role, activeRoleSection, "stable role section is unchanged across runs");
  assert.match(promptObservations[0].rendered, /BASE POLICY/);
  assert.match(promptObservations[0].rendered, /APPENDED POLICY/);
  assert.match(promptObservations[0].rendered, /HOST SECTION/);

  for (const view of views.slice(0, 4)) {
    assert.match(view.system, /BASE POLICY/);
    assert.match(view.system, /APPENDED POLICY/);
    assert.match(view.system, /HOST SECTION/);
    assert.match(view.system, /Role: researcher/);
    assert.match(view.system, /FIXTURE ROLE/);
    assert.ok(view.tools.some(tool => tool.name === "mustr_roster"));
    assert.equal(view.messages.some(message => JSON.stringify(message).includes("mustr-state")), false);
    assert.equal(view.messages.some(message => JSON.stringify(message).includes("Current mustr state:")), false);
  }
  assert.equal(views[0].system, views[1].system);
  assert.equal(views[0].system, views[2].system);
  assert.equal(views[0].system, views[3].system);
  for (const view of views.slice(1, 4)) assert.deepEqual(view.tools, views[0].tools, "active schemas stay stable across continuations and runs");

  for (let index = 0; index < 3; index++) {
    assert.deepEqual(
      views[index + 1].messages.slice(0, views[index].messages.length),
      views[index].messages,
      `SDK context ${index + 2} retains context ${index + 1} as an exact ordered prefix`,
    );
  }
  const firstRosterResult = views[1].messages.find(message => message.role === "toolResult" && message.toolName === "mustr_roster");
  const secondRosterResults = views[3].messages.filter(message => message.role === "toolResult" && message.toolName === "mustr_roster");
  assert.equal(secondRosterResults.length, 2, "the second run retains the first explicit roster result and adds its own");
  const firstRoster = rosterSnapshotFromResult(firstRosterResult);
  const retainedRoster = rosterSnapshotFromResult(secondRosterResults[0]);
  const refreshedRoster = rosterSnapshotFromResult(secondRosterResults[1]);
  assert.equal(retainedRoster.members[0].last_seen, firstRoster.members[0].last_seen, "prior roster result remains durable and unchanged");
  assert.ok(refreshedRoster.members[0].last_seen > firstRoster.members[0].last_seen, "synthetic hub last_seen changes between explicit roster calls");
  assert.ok(syncSnapshots.some(snapshot => snapshot.members[0].last_seen === refreshedRoster.members[0].last_seen));

  await session.sendUserMessage("/mustr leave", { expandPromptTemplates: true });
  assert.equal(session.getActiveToolNames().some(name => name.startsWith("mustr_")), false, "leave removes mustr tools");
  await session.prompt("A following ordinary run after leaving mustr.");
  assert.equal(views.length, 5);
  assert.equal(promptObservations.length, 3);
  assert.equal(promptObservations[2].sections.mustr_role, undefined, "unadmitted run has no active role section");
  assert.equal(promptObservations[2].forceSystemPrompt, undefined);
  assert.match(views[4].system, /BASE POLICY/);
  assert.match(views[4].system, /HOST SECTION/);
  assert.doesNotMatch(views[4].system, /Role: researcher|FIXTURE ROLE/);
  assert.equal(views[4].tools.some(tool => tool.name.startsWith("mustr_")), false);
  assert.equal(views[4].messages.some(message => JSON.stringify(message).includes("Current mustr state:")), false);
  assert.ok(calls.some(call => call.op === "leave"), "test exits through the production leave path");
  assert.deepEqual(hubErrors, []);
  assert.deepEqual(sessionErrors, []);
});
