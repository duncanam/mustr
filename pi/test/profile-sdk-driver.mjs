import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import { Agent } from "@earendil-works/pi-agent-core";
import { InMemoryCredentialStore } from "@earendil-works/pi-ai";
import {
  AgentSession, createExtensionRuntime, ModelRuntime, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";

const args = process.argv.slice(2);
const valueAfter = flag => args[args.indexOf(flag) + 1];
const launchPath = valueAfter("--mustr-launch");
// Keep the entire model suffix; independently assert argv against the manifest.
const requestedModel = valueAfter("--model");
const match = /^([^/]+)\/(.+)$/.exec(requestedModel);
assert.ok(match, "model argv must be an exact provider/id pair");
const [, provider, id] = match;
const thinking = valueAfter("--thinking");
assert.ok(launchPath && provider && id && thinking);
const manifest = JSON.parse(await readFile(launchPath, "utf8"));
if (manifest.version === 2) {
  assert.equal(process.env.HERDR_SOCKET_PATH, undefined);
  assert.equal(process.env.HERDR_PANE_ID, undefined);
  assert.equal(process.env.HERDR_ENV, undefined);
  for (const field of ["config_dir", "state_dir", "binary", "ready_path"]) assert.equal(manifest[field], undefined);
}
assert.equal(`${provider}/${id}`, manifest.model);
assert.equal(thinking, manifest.thinking);

const model = {
  id, name: id, provider, api: "anthropic-messages", baseUrl: "", reasoning: thinking !== "off",
  input: ["text"], contextWindow: 100000, maxTokens: 128,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
};
const credentials = new InMemoryCredentialStore();
const modelRuntime = await ModelRuntime.create({ credentials, modelsPath: null, allowModelNetwork: false, refreshOnCreate: false });
modelRuntime.registerNativeProvider({
  id: provider, name: "Synthetic offline profile fixture",
  auth: { apiKey: { name: "Synthetic key", resolve: async () => undefined } },
  getModels: () => [model],
  stream: () => { throw new Error("Synthetic profile startup must not call a provider"); },
  streamSimple: () => { throw new Error("Synthetic profile startup must not call a provider"); },
});
const directory = process.cwd();
const sessionManager = SessionManager.inMemory(directory);
const agent = new Agent({
  initialState: { model, thinkingLevel: thinking },
  sessionId: sessionManager.getSessionId(),
  streamFn: () => { throw new Error("Synthetic profile startup must not request a model"); },
});
const extensionRuntime = createExtensionRuntime();
const extensionPath = `${process.env.PI_CODING_AGENT_DIR ?? `${process.env.HOME}/.pi/agent`}/extensions/mustr/index.ts`;
const sourceInfo = { path: extensionPath, source: "inline", scope: "temporary", origin: "top-level" };
const extension = {
  path: extensionPath, resolvedPath: extensionPath, sourceInfo,
  handlers: new Map(), commands: new Map(), tools: new Map(), flags: new Map(), shortcuts: new Map(), messageRenderers: new Map(),
};
const extensionApi = {
  on(name, handler) {
    const handlers = extension.handlers.get(name) ?? [];
    handlers.push(handler); extension.handlers.set(name, handlers);
    return () => { const index = handlers.indexOf(handler); if (index >= 0) handlers.splice(index, 1); };
  },
  registerTool(definition) { extension.tools.set(definition.name, { definition, sourceInfo }); },
  registerCommand(name, command) { extension.commands.set(name, { ...command, name, sourceInfo }); },
  registerFlag(name, options) { extension.flags.set(name, options); },
  getFlag(name) { return name === "mustr-launch" ? launchPath : undefined; },
};
const api = new Proxy(extensionApi, { get(target, key) {
  if (key in target) return target[key];
  const method = extensionRuntime[key];
  if (typeof method !== "function") throw new Error(`Unexpected Pi SDK API: ${String(key)}`);
  return (...values) => method.apply(extensionRuntime, values);
} });
const { default: mustr } = await import(pathToFileURL(extensionPath).href);
await mustr(api);
const resourceLoader = {
  getExtensions: () => ({ extensions: [extension], errors: [], runtime: extensionRuntime }),
  getSkills: () => ({ skills: [], diagnostics: [] }),
  getPrompts: () => ({ prompts: [], diagnostics: [] }),
  getThemes: () => ({ themes: [], diagnostics: [] }),
  getAgentsFiles: () => ({ agentsFiles: [] }),
  getSystemPrompt: () => "Synthetic profile startup; no user resources, network, model calls, or external jobs.",
  getSystemPromptSource: () => undefined, getAppendSystemPrompt: () => [], getAppendSystemPromptSources: () => [],
  extendResources() { throw new Error("Synthetic profile startup forbids resource discovery"); },
  async reload() { throw new Error("Synthetic profile startup forbids reload"); },
};
const errors = [], notices = [], sdkEvents = [];
const session = new AgentSession({
  agent, sessionManager, resourceLoader, modelRuntime,
  settingsManager: SettingsManager.inMemory({ compaction: { enabled: false }, retry: { enabled: false } }),
  cwd: directory, initialActiveToolNames: [],
});
session.refreshContext();
await session.bindExtensions({
  mode: "rpc", onError: error => errors.push(error),
  uiContext: { getEditorText: () => "", setStatus() {}, notify(message, level) { notices.push({ message, level }); } },
});
session.subscribe(event => sdkEvents.push(event.type));
try {
  const readyPath = launchPath.replace(/descriptor\.json$/, "ready.json");
  const deadline = Date.now() + 12_000;
  let ready = false;
  while (Date.now() < deadline) {
    if (errors.length) throw errors[0];
    if (manifest.version === 2) {
      if (api.getActiveTools().includes("mustr_roster")) { ready = true; break; }
      await delay(25);
    } else {
      try { await access(readyPath); ready = true; break; }
      catch { await delay(25); }
    }
  }
  if (!ready) throw new Error(`Pi profile startup did not write readiness: ${JSON.stringify({ errors: errors.map(String), notices, events: sdkEvents, flags: [...extension.flags.keys()], handlers: [...extension.handlers.keys()] })}`);
  assert.deepEqual(errors, []);
  // The ready receipt is written inside the admitted join pump; allow that
  // pump's ordinary post-join sync to return before this SDK fixture shuts down.
  await delay(250);
  if (manifest.version === 2) {
    await session.extensionRunner.emit({ type: "session_start", reason: "reload" });
    await delay(100);
    assert.ok(!api.getActiveTools().includes("mustr_roster"), "reload does not automatically rejoin a jailed launch");
  }
} finally {
  await session.extensionRunner.emit({ type: "session_shutdown", reason: "shutdown" });
  await session.dispose();
}
