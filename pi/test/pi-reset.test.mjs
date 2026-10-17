import test from "node:test";
import assert from "node:assert/strict";
import { Agent } from "@earendil-works/pi-agent-core";
import { InMemoryCredentialStore } from "@earendil-works/pi-ai";
import {
  AgentSession, createAgentSessionRuntime, createExtensionRuntime,
  ModelRuntime, SessionManager, SettingsManager,
} from "@earendil-works/pi-coding-agent";

// Version-pinned Pi 1.2.0 feasibility proof, NOT a production reset implementation.
// Supply public ResourceLoader/Extension records directly: no discovery, credentials,
// session files, network, or models. Registration helpers are deliberately not tested;
// real Pi command dispatch, command contexts, and runtime replacement ARE tested.
const cwd = "/mustr-reset-fixture";
const basePolicy = "BASE POLICY: retain protected-path and source-control guards.";
const sentinel = "OLD CONVERSATION MUST NOT ENTER THE REPLACEMENT";
const model = {
  id: "never-called", name: "never-called", provider: "mustr-reset-no-provider",
  api: "unknown", baseUrl: "", reasoning: false, input: ["text"],
  contextWindow: 100000, maxTokens: 100,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
};

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

async function fixture(t) {
  const modelRuntime = await ModelRuntime.create({
    credentials: new InMemoryCredentialStore(), modelsPath: null,
    allowModelNetwork: false, refreshOnCreate: false,
  });
  const states = [], outcomes = [], errors = [];
  // Test stand-in for the hub's durable approved reset plan. Production must redeem
  // against the persisted hub plan, not trust a role/name supplied in command text.
  const plans = new Map(), restores = new Map();
  let cancelSwitch = false, streamCalls = 0, runtime;
  let gate, busyGate;

  async function createRuntime({ sessionManager, sessionStartEvent }) {
    const pi = createExtensionRuntime();
    const state = { pi, role: undefined, name: undefined, starts: [], commands: 0 };
    states.push(state);
    const path = "<inline:mustr-reset-proof>";
    const sourceInfo = { path, source: "inline", scope: "temporary", origin: "top-level" };
    const commands = new Map();
    function command(name, handler) { commands.set(name, { name, sourceInfo, handler }); }

    command("mustr-reset-proof", async (token, ctx) => {
      state.commands++;
      const plan = plans.get(token);
      if (!plan || plan.oldId !== ctx.sessionManager.getSessionId() || plan.used) {
        outcomes.push("rejected-token"); return;
      }
      state.commandCtx = ctx;
      // Let a test change Pi's real pending queue after remote dispatch but before
      // command execution commits to the destructive operation.
      if (gate) { gate.entered.resolve(); await gate.release.promise; }
      if (!ctx.isIdle() || ctx.hasPendingMessages()) {
        outcomes.push("rejected-idle"); return;
      }
      plan.used = true;
      // Capture only plain approved data. Old pi/ctx are invalid after replacement.
      const approval = { role: plan.role, name: plan.name };
      const result = await ctx.newSession({
        withSession: async replacementCtx => {
          const newId = replacementCtx.sessionManager.getSessionId();
          assert.notEqual(newId, plan.oldId);
          const restoreToken = `restore-${token}`;
          restores.set(restoreToken, { newId, ...approval });
          // This command is dispatched against the NEW runtime, never the old pi.
          await replacementCtx.sendUserMessage(`/mustr-restore-proof ${restoreToken}`, {
            expandPromptTemplates: true,
          });
          assert.equal(restores.has(restoreToken), false, "fresh command redeemed approval");
          outcomes.push("completed");
        },
      });
      if (result.cancelled) outcomes.push("cancelled");
      // No old ctx/pi usage here, including notifications or status updates.
    });
    command("mustr-restore-proof", async (token, ctx) => {
      const approval = restores.get(token);
      if (!approval || approval.newId !== ctx.sessionManager.getSessionId()) {
        outcomes.push("rejected-restore"); return;
      }
      restores.delete(token);
      state.role = approval.role; state.name = approval.name;
      pi.appendEntry("mustr-role-proof", { role: state.role, name: state.name });
    });
    const extension = {
      path, resolvedPath: path, sourceInfo, commands,
      tools: new Map(), flags: new Map(), shortcuts: new Map(), messageRenderers: new Map(),
      handlers: new Map([
        ["session_start", [(event, ctx) => {
          state.starts.push(event.reason);
          assert.equal("newSession" in ctx, false, "ordinary event ctx is not command-capable");
        }]],
        ["session_before_switch", [() => cancelSwitch ? { cancel: true } : undefined]],
        ["before_agent_start", [async () => {
          if (busyGate) { busyGate.entered.resolve(); await busyGate.release.promise; }
        }]],
      ]),
    };
    const resourceLoader = {
      getExtensions: () => ({ extensions: [extension], errors: [], runtime: pi }),
      getSkills: () => ({ skills: [], diagnostics: [] }),
      getPrompts: () => ({ prompts: [], diagnostics: [] }),
      getThemes: () => ({ themes: [], diagnostics: [] }),
      getAgentsFiles: () => ({ agentsFiles: [] }),
      getSystemPrompt: () => basePolicy, getSystemPromptSource: () => undefined,
      getAppendSystemPrompt: () => [], getAppendSystemPromptSources: () => [],
      extendResources() { throw new Error("Unexpected resource discovery"); },
      async reload() { throw new Error("Unexpected resource discovery"); },
    };
    const settingsManager = SettingsManager.inMemory({
      compaction: { enabled: false }, retry: { enabled: false }, cacheWarming: "off",
    });
    const agent = new Agent({
      initialState: { model },
      // A public pre-request hook is a second fail-closed boundary. Pi can still
      // prepare an already-aborted starting run; never let that reach a provider.
      prepareRequest() { throw new Error("Provider requests disabled in reset proof"); },
      streamFn() { streamCalls++; throw new Error("This proof must never call a model"); },
    });
    const session = new AgentSession({
      agent, sessionManager, settingsManager, resourceLoader, modelRuntime, cwd,
      sessionStartEvent, initialActiveToolNames: ["policy-tool"],
      baseToolsOverride: {
        "policy-tool": {
          name: "policy-tool", label: "policy-tool", description: "Policy guard remains installed",
          parameters: { type: "object", properties: {} },
          async execute() { throw new Error("No tool execution in reset proof"); },
        },
      },
    });
    session.refreshContext();
    state.session = session;
    return {
      session, extensionsResult: resourceLoader.getExtensions(), diagnostics: [],
      services: { cwd, agentDir: "/mustr-reset-fixture-agent", settingsManager, modelRuntime, resourceLoader, diagnostics: [] },
    };
  }

  const manager = SessionManager.inMemory(cwd);
  manager.appendMessage({ role: "user", content: sentinel, timestamp: Date.now() });
  manager.appendCustomMessageEntry("old-mustr", sentinel, true, { old: true });
  runtime = await createAgentSessionRuntime(createRuntime, {
    cwd, agentDir: "/mustr-reset-fixture-agent", sessionManager: manager,
  });
  async function bind(session) {
    await session.bindExtensions({
      mode: "tui", onError: error => errors.push(error),
      commandContextActions: {
        waitForIdle: () => session.waitForIdle(),
        newSession: options => runtime.newSession(options),
        fork: (id, options) => runtime.fork(id, options),
        switchSession: (path, options) => runtime.switchSession(path, options),
        async navigateTree() { throw new Error("Unused navigation"); },
        async reload() { throw new Error("Unused reload"); },
      },
    });
  }
  runtime.setRebindSession(bind);
  await bind(runtime.session);
  states[0].role = "researcher"; states[0].name = "approved-name";
  t.after(async () => {
    const stopped = runtime.session.abort();
    busyGate?.release.resolve(); gate?.release.resolve();
    await stopped;
    await runtime.dispose();
    assert.equal(streamCalls, 0);
  });

  return {
    runtime, states, outcomes, errors, manager,
    approve(token = "approved") {
      plans.set(token, { oldId: runtime.session.sessionManager.getSessionId(), role: "researcher", name: "approved-name", used: false });
      return token;
    },
    cancel(value) { cancelSwitch = value; },
    pauseDispatch() { gate = { entered: deferred(), release: deferred() }; return gate; },
    pauseRunStart() { busyGate = { entered: deferred(), release: deferred() }; return busyGate; },
    // Invoke the real bound pi.sendUserMessage from a transport-like external turn,
    // NOT a lifecycle callback. This API intentionally returns void; completion must
    // be observed independently, just as a real reset needs a new-session hub ACK.
    async dispatch(text, expected) {
      const before = outcomes.length;
      states.at(-1).pi.sendUserMessage(text, { expandPromptTemplates: true });
      const deadline = Date.now() + 2000;
      while (outcomes.length === before && errors.length === 0 && Date.now() < deadline) {
        await new Promise(resolve => setTimeout(resolve, 5));
      }
      assert.deepEqual(errors, []);
      assert.equal(outcomes.at(-1), expected);
    },
  };
}

test("Pi reset bridge: real fresh runtime drops prior context and restores only approved identity", async t => {
  const f = await fixture(t);
  const original = f.runtime.session;
  const oldId = original.sessionManager.getSessionId();
  assert.match(JSON.stringify(original.messages), /OLD CONVERSATION/);
  assert.match(original.systemPrompt, /BASE POLICY: retain protected-path/);
  assert.deepEqual(original.getActiveToolNames(), ["policy-tool"]);
  const token = f.approve();
  await f.dispatch(`/mustr-reset-proof ${token}`, "completed");
  const fresh = f.runtime.session;
  assert.notEqual(fresh, original);
  assert.notEqual(fresh.sessionManager.getSessionId(), oldId);
  assert.equal(f.states.length, 2);
  assert.deepEqual(f.states.map(s => s.starts), [["startup"], ["new"]]);
  assert.equal(fresh.sessionManager.getBranch().some(e => e.type === "message" || e.type === "custom_message"), false);
  assert.equal(JSON.stringify(fresh.messages).includes(sentinel), false);
  assert.equal(JSON.stringify(fresh.sessionManager.getBranch()).includes(sentinel), false);
  assert.match(fresh.systemPrompt, /BASE POLICY: retain protected-path/);
  assert.deepEqual(fresh.getActiveToolNames(), ["policy-tool"]);
  assert.equal(f.states[1].role, "researcher");
  assert.equal(f.states[1].name, "approved-name");
  assert.deepEqual(fresh.sessionManager.getBranch().filter(e => e.type === "custom").map(e => e.data), [
    { role: "researcher", name: "approved-name" },
  ]);
  assert.throws(() => f.states[0].commandCtx.sessionManager, /stale/);
  assert.throws(() => f.states[0].pi.assertActive(), /stale/);
  // Fresh restore cannot accept arbitrary identity text or replay the old reset.
  await f.dispatch("/mustr-restore-proof attacker researcher evil-name", "rejected-restore");
  await f.dispatch(`/mustr-reset-proof ${token}`, "rejected-token");
  assert.equal(f.states[1].name, "approved-name");
  assert.equal(f.states.length, 2);
});

test("Pi reset bridge: cancellation retains old session/context and never acknowledges completion", async t => {
  const f = await fixture(t);
  const original = f.runtime.session;
  const before = original.sessionManager.getBranch();
  f.cancel(true);
  await f.dispatch(`/mustr-reset-proof ${f.approve()}`, "cancelled");
  assert.equal(f.runtime.session, original);
  assert.deepEqual(original.sessionManager.getBranch(), before);
  assert.match(JSON.stringify(original.messages), /OLD CONVERSATION/);
  assert.equal(f.states[0].role, "researcher");
  assert.deepEqual(f.outcomes, ["cancelled"]);
  assert.equal(f.states.length, 1);
});

test("Pi reset bridge: command rechecks approved token and actual pending queue at execution", async t => {
  const f = await fixture(t);
  const original = f.runtime.session;
  await f.dispatch("/mustr-reset-proof invented-token", "rejected-token");
  const token = f.approve();
  const gate = f.pauseDispatch();
  const dispatch = f.dispatch(`/mustr-reset-proof ${token}`, "rejected-idle");
  await gate.entered.promise;
  await original.followUp("Human queued input must not be discarded");
  assert.equal(original.pendingMessageCount, 1);
  gate.release.resolve();
  await dispatch;
  assert.equal(f.runtime.session, original);
  assert.equal(original.pendingMessageCount, 1);
  assert.match(JSON.stringify(original.messages), /OLD CONVERSATION/);
  assert.equal(f.states.length, 1);
});

test("Pi reset bridge: command rejects a genuinely busy Pi before any model request", async t => {
  const f = await fixture(t);
  const original = f.runtime.session;
  const gate = f.pauseRunStart();
  // Pi counts the asynchronous before_agent_start boundary as busy. Hold that
  // boundary, dispatch externally, then abort before the first provider request.
  const starting = original.sendCustomMessage({
    customType: "busy-proof", content: "Do not request a model", display: false,
  }, { triggerTurn: true });
  await gate.entered.promise;
  try {
    assert.equal(original.isIdle, false);
    await f.dispatch(`/mustr-reset-proof ${f.approve()}`, "rejected-idle");
    assert.equal(f.runtime.session, original);
    assert.equal(f.states.length, 1);
  } finally {
    const stopped = original.abort();
    gate.release.resolve();
    await Promise.all([starting, stopped]);
  }
  assert.equal(original.isIdle, true);
  assert.match(JSON.stringify(original.messages), /OLD CONVERSATION/);
});

test("Pi reset bridge: raw /new and omitted expansion do not execute a reset command", async t => {
  const f = await fixture(t);
  const original = f.runtime.session;
  const id = original.sessionManager.getSessionId();
  // Unknown local provider has no credentials. This safely exercises prompt
  // preflight and fails BEFORE any model/transport can be invoked.
  await assert.rejects(original.sendUserMessage("/new", { expandPromptTemplates: true }), /API key|authentication|credentials/i);
  await assert.rejects(original.sendUserMessage(`/mustr-reset-proof ${f.approve()}`), /API key|authentication|credentials/i);
  assert.equal(f.states[0].commands, 0);
  assert.equal(f.runtime.session, original);
  assert.equal(original.sessionManager.getSessionId(), id);
  assert.match(JSON.stringify(original.messages), /OLD CONVERSATION/);
  assert.deepEqual(f.outcomes, []);
});
