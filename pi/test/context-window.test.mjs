import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import {
  buildSessionProjection,
  createAgentSession,
  DefaultResourceLoader,
  ModelRuntime,
  SessionManager,
  SettingsManager,
} from "@earendil-works/pi-coding-agent";
import { fauxAssistantMessage, fauxProvider, InMemoryCredentialStore } from "@earendil-works/pi-ai";
import {
  FRESH_ASSIGNMENT_MARKER_KEY,
  FRESH_ASSIGNMENT_MARKER_SCHEMA,
  FRESH_COMPACTION_TAG_KEY,
  FRESH_COMPACTION_TAG_SCHEMA,
  MUSTR_CONTEXT_CUSTOM_TYPE,
  prepareFreshCompaction,
  projectFreshContext,
  resolveFreshContext,
  runFreshCompaction,
} from "../context-window.ts";

const USAGE = {
  input: 1,
  output: 1,
  cacheRead: 0,
  cacheWrite: 0,
  totalTokens: 2,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

const user = (text, timestamp = Date.now()) => ({ role: "user", content: text, timestamp });
const assistant = (text, timestamp = Date.now()) => ({
  role: "assistant",
  content: [{ type: "text", text }],
  api: "openai-completions",
  provider: "openai",
  model: "fixture-model",
  usage: USAGE,
  stopReason: "stop",
  timestamp,
});
const toolCall = (id, timestamp = Date.now()) => ({
  role: "assistant",
  content: [{ type: "toolCall", id, name: "read", arguments: { path: `${id}.txt` } }],
  api: "openai-completions",
  provider: "openai",
  model: "fixture-model",
  usage: USAGE,
  stopReason: "toolUse",
  timestamp,
});
const toolResult = (id, timestamp = Date.now()) => ({
  role: "toolResult",
  toolCallId: id,
  toolName: "read",
  content: [{ type: "text", text: `${id}-tool-result` }],
  isError: false,
  timestamp,
});

function appendFresh(manager, { taskId, messageId, revision = 1, ownerId = "native-owner", spaceId = "space-a" }) {
  const marker = {
    schema: FRESH_ASSIGNMENT_MARKER_SCHEMA,
    authority: "authenticated-hub-assignment",
    kind: "assignment",
    mode: "fresh",
    taskId,
    revision,
    messageId,
    nativeOwnerId: ownerId,
    nativeSessionId: manager.getSessionId(),
    originSpaceId: spaceId,
  };
  const entryId = manager.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, `Assignment ${taskId}`, true, {
    id: messageId,
    kind: "assignment",
    task_id: taskId,
    revision,
    context: "fresh",
    [FRESH_ASSIGNMENT_MARKER_KEY]: marker,
  });
  return { entryId, marker };
}

function currentProjection(manager) {
  return buildSessionProjection(manager.getBranch(), manager.getBranch().at(-1)?.id ?? null);
}

function resolve(manager, options = {}) {
  return resolveFreshContext({
    allEntries: manager.getEntries(),
    branchEntries: manager.getBranch(),
    nativeSessionId: manager.getSessionId(),
    ...options,
  });
}

function publicModel() {
  return {
    id: "fixture-model",
    name: "Offline context fixture",
    provider: "openai",
    api: "openai-completions",
    baseUrl: "https://api.openai.com/v1",
    reasoning: false,
    input: ["text"],
    contextWindow: 128_000,
    maxTokens: 256,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  };
}

async function withAutomaticCompactionEvent(reason, callback) {
  const cwd = await mkdtemp(join(tmpdir(), "mustr-auto-context-window-"));
  let session;
  try {
    const manager = SessionManager.inMemory(cwd, { id: randomUUID() });
    manager.appendMessage(user("Private pre-Fresh context."));
    const fresh = appendFresh(manager, { taskId: `task-${reason}`, messageId: `message-${reason}` });
    manager.appendMessage(user(`Fresh ${reason} work ${"detail ".repeat(reason === "threshold" ? 20 : 30)}`));
    manager.appendMessage(assistant("Fresh work progress."));
    manager.appendMessage(user("Current Fresh task turn."));

    const settingsManager = SettingsManager.inMemory({
      compaction: { enabled: true, reserveTokens: reason === "threshold" ? 3900 : 1, keepRecentTokens: 4 },
      defaultTools: [],
    });
    const captured = [];
    const resourceLoader = new DefaultResourceLoader({
      cwd,
      agentDir: cwd,
      settingsManager,
      noSkills: true,
      noPromptTemplates: true,
      noThemes: true,
      noContextFiles: true,
      extensionFactories: [pi => {
        pi.on("session_before_compact", event => {
          captured.push(event);
          return { cancel: true };
        });
      }],
    });
    await resourceLoader.reload();
    const runtime = await ModelRuntime.create({
      credentials: new InMemoryCredentialStore(),
      modelsPath: null,
      allowModelNetwork: false,
      refreshOnCreate: false,
    });
    const faux = fauxProvider({
      api: "openai-completions",
      provider: `mustr-${reason}-fixture`,
      models: [{ id: "small-window", contextWindow: 4096, maxTokens: 8, input: ["text"] }],
    });
    runtime.registerNativeProvider(faux.provider);
    await runtime.setRuntimeApiKey(faux.provider.id, "fixture-only-key");
    faux.setResponses([reason === "overflow"
      ? fauxAssistantMessage("overflow response", { stopReason: "error", errorMessage: "maximum context length exceeded" })
      : fauxAssistantMessage("threshold response")]);
    const created = await createAgentSession({
      cwd,
      agentDir: cwd,
      model: faux.getModel(),
      modelRuntime: runtime,
      settingsManager,
      sessionManager: manager,
      resourceLoader,
      noTools: "all",
      thinkingLevel: "off",
    });
    session = created.session;
    const sessionEvents = [];
    session.subscribe(event => sessionEvents.push(event));
    try {
      await session.prompt("Trigger the public automatic compaction path.");
    } catch {
      // Overflow is expected to reject after the hook cancels recovery compaction.
    }
    assert.equal(faux.state.callCount, 1, "the offline public provider completed one real AgentSession.prompt");
    const event = captured.find(candidate => candidate.reason === reason);
    assert.ok(event, `public AgentSession.prompt triggers a real ${reason} session_before_compact event: ${JSON.stringify({ calls: faux.state.callCount, contextWindow: faux.getModel().contextWindow, settings: settingsManager.getCompactionSettings(faux.getModel()), lifecycle: sessionEvents.filter(item => item.type.startsWith("compaction")) })}`);
    return await callback({ manager, event, boundary: { ...fresh.marker, entryId: fresh.entryId }, captured });
  } finally {
    session?.dispose();
    await rm(cwd, { recursive: true, force: true });
  }
}

async function withPublicCompactionEvent(t, callback, { oldCompactionBeforeFresh = false } = {}) {
  const cwd = await mkdtemp(join(tmpdir(), "mustr-context-window-"));
  let session;
  try {
    const sessionId = randomUUID();
    const manager = SessionManager.inMemory(cwd, { id: sessionId });
    manager.appendMessage(user("PRIVATE_BEFORE_FRESH_SENTINEL"));
    const oldAssistantId = manager.appendMessage(assistant("Old conversation that must never enter a Fresh summary."));
    if (oldCompactionBeforeFresh) {
      manager.appendCompaction("PRIVATE_OLD_COMPACTION_SENTINEL", oldAssistantId, 99, { readFiles: ["old.txt"], modifiedFiles: ["old.txt"] }, true, USAGE);
    }
    const fresh = appendFresh(manager, { taskId: "task-fresh-a", messageId: "message-fresh-a" });
    manager.appendMessage(user("Fresh task A starts here."));
    manager.appendMessage(toolCall("fresh-call-1"));
    manager.appendMessage(toolResult("fresh-call-1"));
    manager.appendMessage(assistant("Fresh task A made progress."));
    manager.appendMessage(user(`More task-A context ${"task-data ".repeat(600)}`));
    manager.appendMessage(assistant(`Task-A continuation ${"answer ".repeat(600)}`));
    manager.appendMessage(user("Keep a recent tail for the native cut."));
    manager.appendMessage(assistant("Recent task-A response."));

    const settingsManager = SettingsManager.inMemory({
      compaction: { enabled: true, reserveTokens: 1, keepRecentTokens: 4 },
      defaultTools: [],
    });
    const captured = [];
    const resourceLoader = new DefaultResourceLoader({
      cwd,
      agentDir: cwd,
      settingsManager,
      noSkills: true,
      noPromptTemplates: true,
      noThemes: true,
      noContextFiles: true,
      systemPrompt: "Stable host system instructions remain outside task-window filtering.",
      extensionFactories: [pi => {
        pi.on("session_before_compact", event => {
          captured.push(event);
          return { cancel: true };
        });
      }],
    });
    await resourceLoader.reload();
    const modelRuntime = await ModelRuntime.create({
      credentials: new InMemoryCredentialStore(),
      modelsPath: null,
      allowModelNetwork: false,
      refreshOnCreate: false,
    });
    const created = await createAgentSession({
      cwd,
      agentDir: cwd,
      model: publicModel(),
      modelRuntime,
      settingsManager,
      sessionManager: manager,
      resourceLoader,
      noTools: "all",
      thinkingLevel: "off",
    });
    session = created.session;
    await assert.rejects(session.compact(), /cancel/i);
    assert.equal(captured.length, 1, "the public session_before_compact event was captured");
    const event = captured[0];
    assert.ok(event.preparation, "the SDK supplied its native preparation object");
    return await callback({ cwd, manager, session, event, boundary: { ...fresh.marker, entryId: fresh.entryId }, model: publicModel(), captured });
  } finally {
    session?.dispose();
    await rm(cwd, { recursive: true, force: true });
  }
}

test("Reuse baseline passes through; Fresh A survives Reuse B and Guide B; Fresh C replaces A", () => {
  const manager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  manager.appendMessage({ role: "system", content: "Stable host prompt" });
  manager.appendMessage(user("OLD_WINDOW_SENTINEL"));
  manager.appendMessage(assistant("Old answer"));
  const a = appendFresh(manager, { taskId: "task-a", messageId: "message-a", spaceId: "space-old" });
  manager.appendMessage(user("A task body"));
  manager.appendMessage(assistant("A progress"));
  manager.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, "Assignment task-b", true, {
    id: "message-b", kind: "assignment", task_id: "task-b", revision: 1, context: "reuse",
  });
  manager.appendMessage(user("B reused A's context"));
  manager.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, "Guide task-b", true, { id: "guide-b" });

  const nativeSessionId = manager.getSessionId();
  const archiveBeforeProjection = structuredClone(manager.getEntries());
  const projection = currentProjection(manager);
  const incoming = projection.messages.filter(message => message.role !== "system");
  const unpersistedTail = user("Current unpersisted user tail");
  const missingNewFresh = resolve(manager, {
    requiredFresh: { ...a.marker, taskId: "not-inserted", messageId: "message-not-inserted", revision: 2 },
  });
  assert.equal(missingNewFresh.kind, "blocked", "an expected Fresh marker cannot fall back to an older boundary");
  if (missingNewFresh.kind === "blocked") assert.equal(missingNewFresh.problem.code, "required_marker_missing");
  const resolutionA = resolve(manager, { nativeOwnerId: "native-owner" });
  assert.equal(resolutionA.kind, "fresh");
  assert.equal(resolutionA.boundary.entryId, a.entryId);
  const projectedA = projectFreshContext({
    resolution: resolutionA,
    projection,
    branchEntries: manager.getBranch(),
    incomingMessages: [...incoming, unpersistedTail],
  });
  assert.equal(projectedA.ok, true);
  assert.equal(projectedA.kind, "fresh");
  assert.equal(projectedA.messages[0].content, "Assignment task-a");
  assert.equal(projectedA.messages.some(message => JSON.stringify(message).includes("OLD_WINDOW_SENTINEL")), false);
  assert.equal(projectedA.messages.some(message => JSON.stringify(message).includes("Guide task-b")), true);
  assert.equal(projectedA.messages.at(-1), unpersistedTail, "unpersisted incoming tail is preserved verbatim");
  assert.equal(projection.messages.some(message => message.role === "system"), true);
  assert.equal(projectedA.messages.some(message => message.role === "system"), false, "only non-system input is transformed; Pi retains its current host prompt/tools");
  const mismatchedInput = [...incoming, unpersistedTail];
  mismatchedInput[0] = { ...mismatchedInput[0], content: "MISMATCHED_PREFIX" };
  const mismatch = projectFreshContext({
    resolution: resolutionA,
    projection,
    branchEntries: manager.getBranch(),
    incomingMessages: mismatchedInput,
  });
  assert.equal(mismatch.ok, false);
  if (!mismatch.ok) {
    assert.equal(mismatch.problem.code, "canonical_projection_mismatch");
    assert.equal(mismatch.messages.length, 1, "failure returns a blocker-only input, not the old full transcript");
    assert.equal(JSON.stringify(mismatch.messages).includes("OLD_WINDOW_SENTINEL"), false);
    assert.equal(JSON.stringify(mismatch.messages).includes("MISMATCHED_PREFIX"), false);
  }
  assert.deepEqual(manager.getEntries(), archiveBeforeProjection, "context selection never edits native archive entries");
  assert.equal(manager.getSessionId(), nativeSessionId, "Fresh is not a native session reset");

  const baselineManager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  baselineManager.appendMessage(user("Reuse without any Fresh anchor"));
  const baselineProjection = currentProjection(baselineManager);
  const baselineIncoming = baselineProjection.messages.filter(message => message.role !== "system");
  const baselineTail = user("Baseline unpersisted tail");
  const baseline = projectFreshContext({
    resolution: resolve(baselineManager),
    projection: baselineProjection,
    branchEntries: baselineManager.getBranch(),
    incomingMessages: [...baselineIncoming, baselineTail],
  });
  assert.equal(baseline.ok, true);
  assert.equal(baseline.kind, "baseline");
  assert.equal(resolve(baselineManager, { provenSpaceId: "space-b" }).kind, "baseline", "a proven space with no Fresh history is a true baseline");
  assert.deepEqual(baseline.messages, [...baselineIncoming, baselineTail]);
  assert.equal(baseline.messages.at(-1), baselineTail);

  const c = appendFresh(manager, { taskId: "task-c", messageId: "message-c", revision: 2, spaceId: "space-new" });
  manager.appendMessage(user("C starts a replacement window"));
  const projectionC = currentProjection(manager);
  const resolutionC = resolve(manager, {
    nativeOwnerId: "native-owner",
    provenSpaceId: "space-new",
    requiredFresh: c.marker,
  });
  assert.equal(resolutionC.kind, "fresh");
  assert.equal(resolutionC.boundary.entryId, c.entryId);
  const projectedC = projectFreshContext({
    resolution: resolutionC,
    projection: projectionC,
    branchEntries: manager.getBranch(),
    incomingMessages: projectionC.messages.filter(message => message.role !== "system"),
  });
  assert.equal(projectedC.ok, true);
  assert.equal(projectedC.messages[0].content, "Assignment task-c");
  assert.equal(projectedC.messages.some(message => JSON.stringify(message).includes("A task body")), false);
  assert.equal(projectedC.messages.some(message => JSON.stringify(message).includes("B reused A's context")), false);
});

test("Fresh projection requires exact canonical custom timestamps, content, and structured identity", () => {
  const manager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  manager.appendMessage(user("PRIVATE_BEFORE_FRESH_WINDOW"));
  appendFresh(manager, { taskId: "timestamp-task", messageId: "timestamp-message" });
  manager.appendMessage(user("Fresh task body"));
  const projection = currentProjection(manager);
  const incoming = projection.messages.filter(message => message.role !== "system");
  const customIndex = incoming.findIndex(message => message.role === "custom" && message.customType === MUSTR_CONTEXT_CUSTOM_TYPE);
  assert.notEqual(customIndex, -1);
  const exact = projectFreshContext({
    resolution: resolve(manager), projection, branchEntries: manager.getBranch(), incomingMessages: incoming,
  });
  assert.equal(exact.ok, true);
  if (exact.ok) {
    assert.equal(exact.messages[0].timestamp, incoming[customIndex].timestamp);
    assert.equal(JSON.stringify(exact.messages).includes("PRIVATE_BEFORE_FRESH_WINDOW"), false);
  }

  for (const alter of [
    message => ({ ...message, timestamp: message.timestamp - 60_000 }),
    message => ({ ...message, content: "forged assignment body" }),
    message => ({ ...message, details: { ...message.details, task_id: "other-task" } }),
  ]) {
    const mismatched = [...incoming]; mismatched[customIndex] = alter(incoming[customIndex]);
    const blocked = projectFreshContext({
      resolution: resolve(manager), projection, branchEntries: manager.getBranch(), incomingMessages: mismatched,
    });
    assert.equal(blocked.ok, false, "timestamp-only differences and custom identity/content mutations fail closed");
    if (!blocked.ok) assert.equal(blocked.problem.code, "canonical_projection_mismatch");
  }
});

test("Fresh projection validates canonical tool pairs together with the unchanged native tail", () => {
  const crossBoundaryManager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  crossBoundaryManager.appendMessage(user("PRIVATE_BEFORE_FRESH_WINDOW"));
  appendFresh(crossBoundaryManager, { taskId: "cross-boundary-task", messageId: "cross-boundary-message" });
  crossBoundaryManager.appendMessage(user("Fresh suffix begins."));
  crossBoundaryManager.appendMessage(toolCall("fresh-call-tail-result"));
  const crossProjection = currentProjection(crossBoundaryManager);
  const crossCanonical = crossProjection.messages.filter(message => message.role !== "system");
  const tailResult = toolResult("fresh-call-tail-result");
  const crossBoundary = projectFreshContext({
    resolution: resolve(crossBoundaryManager),
    projection: crossProjection,
    branchEntries: crossBoundaryManager.getBranch(),
    incomingMessages: [...crossCanonical, tailResult],
  });
  assert.equal(crossBoundary.ok, true, "a canonical Fresh-suffix call may be completed by its matching unchanged tail result");
  if (crossBoundary.ok) {
    assert.ok(crossBoundary.messages.some(message => message.role === "assistant" && message.content.some(block => block.type === "toolCall" && block.id === "fresh-call-tail-result")));
    assert.equal(crossBoundary.messages.at(-1), tailResult, "the native tail result is preserved verbatim");
    assert.equal(JSON.stringify(crossBoundary.messages).includes("PRIVATE_BEFORE_FRESH_WINDOW"), false);
  }

  const tailPairManager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  appendFresh(tailPairManager, { taskId: "tail-pair-task", messageId: "tail-pair-message" });
  tailPairManager.appendMessage(user("Fresh suffix without an open tool call."));
  const tailPairProjection = currentProjection(tailPairManager);
  const tailPairCanonical = tailPairProjection.messages.filter(message => message.role !== "system");
  const tailCall = toolCall("tail-local-pair");
  const tailPairResult = toolResult("tail-local-pair");
  const tailPair = projectFreshContext({
    resolution: resolve(tailPairManager),
    projection: tailPairProjection,
    branchEntries: tailPairManager.getBranch(),
    incomingMessages: [...tailPairCanonical, tailCall, tailPairResult],
  });
  assert.equal(tailPair.ok, true, "a complete tool pair wholly in the unchanged tail is retained");
  if (tailPair.ok) assert.deepEqual(tailPair.messages.slice(-2), [tailCall, tailPairResult]);

  for (const [label, tail] of [
    ["orphan result", [toolResult("orphan-tail-result")]],
    ["pre-Fresh call result", [toolResult("pre-fresh-call")]],
  ]) {
    const manager = SessionManager.inMemory("/fixture", { id: randomUUID() });
    manager.appendMessage(user("PRIVATE_BEFORE_FRESH_WINDOW"));
    if (label === "pre-Fresh call result") manager.appendMessage(toolCall("pre-fresh-call"));
    appendFresh(manager, { taskId: `blocked-${label}`, messageId: `blocked-${label}-message` });
    manager.appendMessage(user("Fresh suffix has no matching call."));
    const projection = currentProjection(manager);
    const canonical = projection.messages.filter(message => message.role !== "system");
    const blocked = projectFreshContext({
      resolution: resolve(manager),
      projection,
      branchEntries: manager.getBranch(),
      incomingMessages: [...canonical, ...tail],
    });
    assert.equal(blocked.ok, false, `${label} fails closed`);
    if (!blocked.ok) {
      assert.equal(blocked.problem.code, "tool_pair_boundary");
      assert.equal(blocked.messages.length, 1, "failure returns only a blocker, not the old transcript");
      assert.equal(JSON.stringify(blocked.messages).includes("PRIVATE_BEFORE_FRESH_WINDOW"), false);
    }
  }
});

test("a Guide revision retains the original Fresh assignment anchor", () => {
  const manager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  const fresh = appendFresh(manager, { taskId: "task-a", messageId: "message-a", revision: 1 });
  manager.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, "Guide task-a revision 2", true, {
    id: "guide-a-r2", kind: "guide", task_id: "task-a", revision: 2,
  });
  const result = resolve(manager, { requiredFresh: fresh.marker });
  assert.equal(result.kind, "fresh");
  assert.equal(result.boundary.messageId, "message-a");
  assert.equal(result.boundary.revision, 1, "Guide revision is not a new Fresh boundary or a rewrite of its assignment revision");
});

test("an unscoped BranchSummary after Fresh blocks context instead of importing its summary", () => {
  const manager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  manager.appendMessage(user("PRIVATE_BEFORE_FRESH"));
  const fresh = appendFresh(manager, { taskId: "task-a", messageId: "message-a" });
  manager.appendMessage(user("Fresh window body"));
  manager.branchWithSummary(manager.getBranch().at(-1).id, "Unscoped old branch summary");
  const projection = currentProjection(manager);
  const incoming = projection.messages.filter(message => message.role !== "system");
  const result = projectFreshContext({
    resolution: resolve(manager),
    projection,
    branchEntries: manager.getBranch(),
    incomingMessages: incoming,
  });
  assert.equal(result.ok, false);
  if (!result.ok) {
    assert.equal(result.problem.code, "unscoped_branch_summary");
    assert.equal(result.messages.length, 1);
    assert.equal(JSON.stringify(result.messages).includes("PRIVATE_BEFORE_FRESH"), false);
    assert.equal(JSON.stringify(result.messages).includes("Unscoped old branch summary"), false);
  }
  assert.ok(fresh.entryId);
});

test("prose, Info, queued snapshots, attempt-only, and malformed stale markers do not activate Fresh", () => {
  const manager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  manager.appendMessage(user("The prose says fresh assignment context, but is not an authoritative marker."));
  manager.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, "Info from Mustr", true, { id: "info-only" });
  manager.appendCustomEntry(MUSTR_CONTEXT_CUSTOM_TYPE, {
    kind: "assignment_attempt",
    task_id: "queued-task",
    context: "fresh",
    message_id: "attempt-only",
  });
  const malformed = {
    schema: FRESH_ASSIGNMENT_MARKER_SCHEMA,
    authority: "attempt-only",
    kind: "assignment",
    mode: "fresh",
    taskId: "stale-task",
    revision: 1,
    messageId: "stale-message",
    nativeOwnerId: "native-owner",
    nativeSessionId: manager.getSessionId(),
    originSpaceId: "space-a",
  };
  manager.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, "Stale/cancelled assignment attempt", true, {
    id: "stale-message",
    kind: "assignment",
    task_id: "stale-task",
    revision: 1,
    context: "fresh",
    [FRESH_ASSIGNMENT_MARKER_KEY]: malformed,
  });
  const result = resolve(manager);
  assert.equal(result.kind, "blocked");
  if (result.kind === "blocked") assert.equal(result.problem.code, "malformed_marker");

  const clean = SessionManager.inMemory("/fixture", { id: randomUUID() });
  clean.appendMessage(user("Prose and Info are not markers."));
  clean.appendCustomMessageEntry(MUSTR_CONTEXT_CUSTOM_TYPE, "Info", true, { id: "info" });
  clean.appendCustomEntry(MUSTR_CONTEXT_CUSTOM_TYPE, { task_id: "queued-only", context: "fresh" });
  assert.equal(resolve(clean).kind, "baseline");
  const expectedButNotInserted = {
    schema: FRESH_ASSIGNMENT_MARKER_SCHEMA, authority: "authenticated-hub-assignment", kind: "assignment", mode: "fresh", taskId: "future-task", revision: 1,
    messageId: "future-message", nativeOwnerId: "native-owner", nativeSessionId: clean.getSessionId(), originSpaceId: "space-a",
  };
  const missing = resolve(clean, { requiredFresh: expectedButNotInserted });
  assert.equal(missing.kind, "blocked");
  if (missing.kind === "blocked") assert.equal(missing.problem.code, "required_marker_missing");

  const staleRevision = SessionManager.inMemory("/fixture", { id: randomUUID() });
  const insertedOldRevision = appendFresh(staleRevision, { taskId: "revised-task", messageId: "revised-message", revision: 1 });
  const currentRevision = resolve(staleRevision, { requiredFresh: { ...insertedOldRevision.marker, revision: 2 } });
  assert.equal(currentRevision.kind, "blocked", "a revision-invalid delivery cannot activate the old marker as a fallback");
  if (currentRevision.kind === "blocked") assert.equal(currentRevision.problem.code, "required_marker_mismatch");
});

test("missing required Fresh and off-branch latest markers fail closed; a later valid Fresh repairs the selected branch", async t => {
  const cwd = await mkdtemp(join(tmpdir(), "mustr-context-archive-"));
  try {
    const sessionDir = join(cwd, "sessions");
    const original = SessionManager.create(cwd, sessionDir, { id: randomUUID() });
    const rootId = original.appendMessage(user("Root branch"));
    const a = appendFresh(original, { taskId: "task-a", messageId: "message-a" });
    original.appendMessage(user("A history"));
    const b = appendFresh(original, { taskId: "task-b", messageId: "message-b", revision: 2 });
    original.appendMessage(user("B history"));
    const file = original.getSessionFile();
    assert.ok(file, "conversation entries have a persisted native archive");
    const reopened = SessionManager.open(file);
    assert.equal(reopened.getSessionId(), original.getSessionId());
    const fromArchiveWithoutLiveOwner = resolve(reopened);
    assert.equal(fromArchiveWithoutLiveOwner.kind, "fresh", "role/admission cleanup does not erase archive-bound identity");
    assert.equal(fromArchiveWithoutLiveOwner.boundary.messageId, "message-b", "append order selects the latest marker");

    reopened.branch(rootId);
    reopened.appendMessage(user("Alternative branch without Fresh"));
    const offBranch = resolve(reopened);
    assert.equal(offBranch.kind, "blocked");
    if (offBranch.kind === "blocked") assert.equal(offBranch.problem.code, "latest_marker_off_branch");
    const requiredOffBranch = resolve(reopened, { requiredFresh: b.marker });
    assert.equal(requiredOffBranch.kind, "blocked");
    if (requiredOffBranch.kind === "blocked") assert.equal(requiredOffBranch.problem.code, "required_marker_off_branch");

    const c = appendFresh(reopened, { taskId: "task-c", messageId: "message-c", revision: 3, spaceId: "space-c" });
    const repaired = resolve(reopened, { provenSpaceId: "space-c", requiredFresh: c.marker });
    assert.equal(repaired.kind, "fresh");
    assert.equal(repaired.boundary.entryId, c.entryId);

    const cTailId = reopened.appendMessage(user("Fresh C retained tail"));
    const cBoundary = { ...c.marker, entryId: c.entryId };
    reopened.appendCompaction("Persisted Fresh C checkpoint", cTailId, 17, {
      readFiles: [],
      modifiedFiles: [],
      [FRESH_COMPACTION_TAG_KEY]: { schema: FRESH_COMPACTION_TAG_SCHEMA, boundary: cBoundary },
    }, true, USAGE);
    reopened.appendMessage(user("After persisted Fresh C compaction"));
    const archiveAfterCompaction = SessionManager.open(reopened.getSessionFile());
    const archiveProjection = currentProjection(archiveAfterCompaction);
    const archiveContext = projectFreshContext({
      resolution: resolve(archiveAfterCompaction, { provenSpaceId: "space-c" }),
      projection: archiveProjection,
      branchEntries: archiveAfterCompaction.getBranch(),
      incomingMessages: archiveProjection.messages.filter(message => message.role !== "system"),
    });
    assert.equal(archiveContext.ok, true);
    assert.equal(archiveContext.kind, "fresh");
    assert.ok(archiveContext.messages.some(message => JSON.stringify(message).includes("Persisted Fresh C checkpoint")));
    assert.equal(archiveContext.messages.some(message => JSON.stringify(message).includes("Root branch")), false);

    const parentSessionId = reopened.getSessionId();
    const forkPath = reopened.createBranchedSession(c.entryId);
    const fork = SessionManager.open(forkPath);
    assert.notEqual(fork.getSessionId(), parentSessionId);
    assert.equal(resolve(fork).kind, "baseline", "a native fork does not rebind copied Fresh metadata");
    const requiredFromParent = resolve(fork, { requiredFresh: c.marker });
    assert.equal(requiredFromParent.kind, "blocked");
    if (requiredFromParent.kind === "blocked") assert.equal(requiredFromParent.problem.code, "required_marker_mismatch");
    assert.notEqual(a.entryId, b.entryId);
  } finally {
    await rm(cwd, { recursive: true, force: true });
  }
});

test("native-owner conflicts fail closed instead of binding another participant's Fresh marker", () => {
  const manager = SessionManager.inMemory("/fixture", { id: randomUUID() });
  appendFresh(manager, { taskId: "owner-a-task", messageId: "owner-a-message", ownerId: "owner-a" });
  appendFresh(manager, { taskId: "owner-b-task", messageId: "owner-b-message", ownerId: "owner-b" });
  const unpinned = resolve(manager);
  assert.equal(unpinned.kind, "blocked");
  if (unpinned.kind === "blocked") assert.equal(unpinned.problem.code, "owner_conflict");
  const pinned = resolve(manager, { nativeOwnerId: "owner-a" });
  assert.equal(pinned.kind, "blocked");
  if (pinned.kind === "blocked") assert.equal(pinned.problem.code, "owner_conflict");
});

test("space authority scopes marker selection before append order; mixed unpinned origins fail closed", () => {
  const sessionId = randomUUID();
  const ownerId = "owner-order";
  const first = {
    id: "z-first",
    parentId: null,
    type: "custom_message",
    customType: MUSTR_CONTEXT_CUSTOM_TYPE,
    content: "Fresh first",
    display: true,
    timestamp: "2035-01-01T00:00:00.000Z",
    details: {
      id: "message-first", kind: "assignment", task_id: "task-first", revision: 1, context: "fresh",
      [FRESH_ASSIGNMENT_MARKER_KEY]: {
        schema: FRESH_ASSIGNMENT_MARKER_SCHEMA, authority: "authenticated-hub-assignment", kind: "assignment", mode: "fresh", taskId: "task-first", revision: 1,
        messageId: "message-first", nativeOwnerId: ownerId, nativeSessionId: sessionId, originSpaceId: "space-a",
      },
    },
  };
  const second = {
    id: "a-second",
    parentId: first.id,
    type: "custom_message",
    customType: MUSTR_CONTEXT_CUSTOM_TYPE,
    content: "Fresh second",
    display: true,
    timestamp: "2020-01-01T00:00:00.000Z",
    details: {
      id: "message-second", kind: "assignment", task_id: "task-second", revision: 2, context: "fresh",
      [FRESH_ASSIGNMENT_MARKER_KEY]: {
        schema: FRESH_ASSIGNMENT_MARKER_SCHEMA, authority: "authenticated-hub-assignment", kind: "assignment", mode: "fresh", taskId: "task-second", revision: 2,
        messageId: "message-second", nativeOwnerId: ownerId, nativeSessionId: sessionId, originSpaceId: "space-b",
      },
    },
  };
  const third = {
    id: "m-third",
    parentId: second.id,
    type: "custom_message",
    customType: MUSTR_CONTEXT_CUSTOM_TYPE,
    content: "Fresh third, but foreign to space B",
    display: true,
    timestamp: "2010-01-01T00:00:00.000Z",
    details: {
      id: "message-third", kind: "assignment", task_id: "task-third", revision: 3, context: "fresh",
      [FRESH_ASSIGNMENT_MARKER_KEY]: {
        schema: FRESH_ASSIGNMENT_MARKER_SCHEMA, authority: "authenticated-hub-assignment", kind: "assignment", mode: "fresh", taskId: "task-third", revision: 3,
        messageId: "message-third", nativeOwnerId: ownerId, nativeSessionId: sessionId, originSpaceId: "space-a",
      },
    },
  };
  const fourth = {
    id: "0-fourth",
    parentId: third.id,
    type: "custom_message",
    customType: MUSTR_CONTEXT_CUSTOM_TYPE,
    content: "Fresh fourth in space B",
    display: true,
    timestamp: "2000-01-01T00:00:00.000Z",
    details: {
      id: "message-fourth", kind: "assignment", task_id: "task-fourth", revision: 4, context: "fresh",
      [FRESH_ASSIGNMENT_MARKER_KEY]: {
        schema: FRESH_ASSIGNMENT_MARKER_SCHEMA, authority: "authenticated-hub-assignment", kind: "assignment", mode: "fresh", taskId: "task-fourth", revision: 4,
        messageId: "message-fourth", nativeOwnerId: ownerId, nativeSessionId: sessionId, originSpaceId: "space-b",
      },
    },
  };
  const header = { type: "session", version: 3, id: sessionId, timestamp: "2020-01-01T00:00:00.000Z", cwd: "/fixture" };
  const manager = SessionManager.inMemory("/fixture", { id: sessionId }, [header, first, second, third, fourth]);
  const unpinned = resolve(manager);
  assert.equal(unpinned.kind, "blocked", "mixed origin spaces cannot be resolved by append recency alone");
  if (unpinned.kind === "blocked") assert.equal(unpinned.problem.code, "space_conflict");
  const selectedB = resolve(manager, { provenSpaceId: "space-b" });
  assert.equal(selectedB.kind, "fresh");
  assert.equal(selectedB.boundary.entryId, "0-fourth", "latest applicable B wins despite a later foreign A, older timestamp, and earlier lexicographic id");
  const selectedA = resolve(manager, { provenSpaceId: "space-a" });
  assert.equal(selectedA.kind, "fresh");
  assert.equal(selectedA.boundary.entryId, "m-third", "append order selects the latest applicable A marker, not the latest overall marker");
  const onlyA = SessionManager.inMemory("/fixture", { id: sessionId }, [header, first]);
  const foreignOnly = resolve(onlyA, { provenSpaceId: "space-b" });
  assert.equal(foreignOnly.kind, "blocked", "foreign-only history cannot become a no-Fresh baseline");
  if (foreignOnly.kind === "blocked") assert.equal(foreignOnly.problem.code, "space_conflict");
  assert.deepEqual(manager.getEntries().map(entry => entry.id), ["z-first", "a-second", "m-third", "0-fourth"]);
});

test("public automatic threshold and overflow hooks use the same provenance-safe preparation", async t => {
  for (const reason of ["threshold", "overflow"]) {
    await withAutomaticCompactionEvent(reason, async ({ manager, event, boundary }) => {
      assert.ok(event.preparation);
      const result = prepareFreshCompaction(event, boundary);
      assert.equal(result.ok, true, `${reason} preparation is safely scoped`);
      if (result.ok) {
        assert.equal(result.preparation.firstKeptEntryId, event.preparation.firstKeptEntryId);
        assert.equal(result.preparation.messagesToSummarize.some(message => JSON.stringify(message).includes("Private pre-Fresh context")), false);
        assert.equal(result.preparation.turnPrefixMessages.some(message => JSON.stringify(message).includes("Private pre-Fresh context")), false);
      }
      assert.equal(event.reason, reason);
      assert.equal(manager.getSessionId(), boundary.nativeSessionId);
    });
  }
});

test("a pre-Fresh compaction summary is removed from the copied preparation", async t => {
  await withPublicCompactionEvent(t, async ({ event, boundary }) => {
    assert.equal(event.preparation.previousSummary, "PRIVATE_OLD_COMPACTION_SENTINEL");
    const copied = prepareFreshCompaction(event, boundary);
    assert.equal(copied.ok, true);
    if (copied.ok) {
      assert.equal(copied.preparation.previousSummary, undefined);
      assert.equal(JSON.stringify(copied.preparation).includes("PRIVATE_OLD_COMPACTION_SENTINEL"), false);
    }
  }, { oldCompactionBeforeFresh: true });
});

test("public compaction event preparation is copied, source-checked, scoped, and uses public native compact with an offline registry", async t => {
  await withPublicCompactionEvent(t, async ({ manager, session, event, boundary, model, captured }) => {
    const originalPreparation = structuredClone(event.preparation);
    const originalBranch = structuredClone(event.branchEntries);
    const prepared = prepareFreshCompaction(event, boundary);
    assert.equal(prepared.ok, true);
    assert.notEqual(prepared.preparation, event.preparation);
    assert.deepEqual(event.preparation, originalPreparation, "native preparation remains immutable");
    assert.deepEqual(event.branchEntries, originalBranch, "native branch entries remain immutable");
    assert.notEqual(prepared.preparation.messagesToSummarize, event.preparation.messagesToSummarize);
    assert.notEqual(prepared.preparation.turnPrefixMessages, event.preparation.turnPrefixMessages);
    assert.notEqual(prepared.preparation.settings, event.preparation.settings);
    assert.equal(prepared.preparation.firstKeptEntryId, event.preparation.firstKeptEntryId);
    assert.equal(prepared.preparation.isSplitTurn, event.preparation.isSplitTurn);
    assert.equal(prepared.preparation.tokensBefore, event.preparation.tokensBefore);
    assert.ok(event.preparation.messagesToSummarize.length > 0);
    assert.ok(event.preparation.turnPrefixMessages.length > 0, "fixture exercises native split-turn prefix scoping");
    assert.deepEqual(prepared.preparation.settings, event.preparation.settings);
    assert.deepEqual(prepared.preparation.fileOps, { read: new Set(), written: new Set(), edited: new Set() });
    assert.equal(prepared.preparation.messagesToSummarize.some(message => JSON.stringify(message).includes("PRIVATE_BEFORE_FRESH_SENTINEL")), false);
    assert.equal(prepared.preparation.turnPrefixMessages.some(message => JSON.stringify(message).includes("PRIVATE_BEFORE_FRESH_SENTINEL")), false);
    const scopedText = JSON.stringify([...prepared.preparation.messagesToSummarize, ...prepared.preparation.turnPrefixMessages]);
    assert.match(scopedText, /Fresh task A|task-data|fresh-call-1/);
    assert.match(scopedText, /fresh-call-1-tool-result/, "tool call/result pair remains available together when summarized");

    const badCut = {
      ...event,
      preparation: { ...event.preparation, firstKeptEntryId: event.branchEntries[0].id },
    };
    const cutResult = prepareFreshCompaction(badCut, boundary);
    assert.equal(cutResult.ok, false);
    if (!cutResult.ok) assert.equal(cutResult.problem.code, "native_cut_before_boundary");

    const eventProjection = buildSessionProjection(event.branchEntries, event.branchEntries.at(-1)?.id ?? null);
    const markerProjectionIndex = eventProjection.entries.findIndex(entry => entry.sourceEntry.id === boundary.entryId);
    const beforeBoundary = eventProjection.entries.slice(0, markerProjectionIndex)
      .filter(entry => entry.sourceEntry.type !== "compaction")
      .flatMap(entry => entry.messages.filter(message => message.role !== "system"));
    const noTail = {
      ...event,
      preparation: {
        ...event.preparation,
        firstKeptEntryId: boundary.entryId,
        messagesToSummarize: beforeBoundary,
        turnPrefixMessages: [],
        isSplitTurn: false,
        previousSummary: undefined,
      },
    };
    const noTailResult = prepareFreshCompaction(noTail, boundary);
    assert.equal(noTailResult.ok, false);
    if (!noTailResult.ok) assert.equal(noTailResult.problem.code, "no_scoped_compaction_content");

    let streamCalls = 0;
    const streamSimple = async (_model, context) => {
      streamCalls++;
      assert.equal(JSON.stringify(context).includes("PRIVATE_BEFORE_FRESH_SENTINEL"), false, "old history never reaches the provider stream");
      return {
        result: async () => ({
          role: "assistant",
          content: [{ type: "text", text: "Scoped Fresh task checkpoint." }],
          api: model.api,
          provider: model.provider,
          model: model.id,
          usage: USAGE,
          stopReason: "stop",
          timestamp: Date.now(),
        }),
      };
    };
    for (const reason of ["manual", "threshold", "overflow"]) {
      const eventForReason = { ...event, reason, signal: new AbortController().signal };
      const outcome = await runFreshCompaction({
        event: eventForReason,
        boundary,
        nativeSessionId: manager.getSessionId(),
        model,
        modelRegistry: { streamSimple },
      });
      assert.equal(outcome.kind, "compaction", `public compact path succeeds for ${reason} event semantics`);
      assert.equal(outcome.result.compaction.firstKeptEntryId, event.preparation.firstKeptEntryId);
      assert.deepEqual(outcome.result.compaction.details[FRESH_COMPACTION_TAG_KEY], {
        schema: FRESH_COMPACTION_TAG_SCHEMA,
        boundary,
      });
      assert.deepEqual(event.preparation, originalPreparation, "native event remains untouched after compact");
      assert.deepEqual(outcome.result.compaction.details.readFiles, []);
      assert.deepEqual(outcome.result.compaction.details.modifiedFiles, []);
    }
    assert.ok(streamCalls >= 3, "current registry streamSimple is used without a provider call");
    const aborted = new AbortController();
    aborted.abort();
    const abortedOutcome = await runFreshCompaction({
      event: { ...event, signal: aborted.signal },
      boundary,
      nativeSessionId: manager.getSessionId(),
      model,
      modelRegistry: { streamSimple },
    });
    assert.equal(abortedOutcome.kind, "cancel");
    assert.deepEqual(abortedOutcome.result, { cancel: true });
    const noSpanOutcome = await runFreshCompaction({
      event: { ...noTail, signal: new AbortController().signal },
      boundary,
      nativeSessionId: manager.getSessionId(),
      model,
      modelRegistry: { streamSimple },
    });
    assert.equal(noSpanOutcome.kind, "cancel");
    assert.deepEqual(noSpanOutcome.result, { cancel: true });
    const failedSummary = await runFreshCompaction({
      event: { ...event, signal: new AbortController().signal },
      boundary,
      nativeSessionId: manager.getSessionId(),
      model,
      modelRegistry: { streamSimple: () => { throw new Error("offline fixture stream failure"); } },
    });
    assert.equal(failedSummary.kind, "cancel");
    assert.deepEqual(failedSummary.result, { cancel: true }, "summarizer failures never fall through to native full-history compaction");

    const successful = await runFreshCompaction({
      event: { ...event, signal: new AbortController().signal },
      boundary,
      nativeSessionId: manager.getSessionId(),
      model,
      modelRegistry: { streamSimple },
    });
    assert.equal(successful.kind, "compaction");
    const compaction = successful.result.compaction;
    manager.appendCompaction(compaction.summary, compaction.firstKeptEntryId, compaction.tokensBefore, compaction.details, true, compaction.usage);
    const projection = currentProjection(manager);
    const incoming = projection.messages.filter(message => message.role !== "system");
    const resolution = resolve(manager);
    const context = projectFreshContext({ resolution, projection, branchEntries: manager.getBranch(), incomingMessages: incoming });
    assert.equal(context.ok, true);
    assert.equal(context.kind, "fresh");
    assert.equal(context.messages.some(message => JSON.stringify(message).includes("PRIVATE_BEFORE_FRESH_SENTINEL")), false);
    assert.ok(context.messages.some(message => JSON.stringify(message).includes("Scoped Fresh task checkpoint.")));

    manager.appendMessage(user(`Post-compaction task continuation ${"new detail ".repeat(120)}`));
    manager.appendMessage(assistant(`Post-compaction response ${"continued work ".repeat(120)}`));
    await assert.rejects(session.compact(), /cancel/i);
    assert.equal(captured.length, 2, "repeated native compaction uses the same public event shape");
    const repeated = prepareFreshCompaction(captured[1], boundary);
    assert.equal(repeated.ok, true);
    if (repeated.ok) {
      assert.equal(repeated.preparation.previousSummary, compaction.summary);
      assert.deepEqual(repeated.preparation.fileOps, { read: new Set(), written: new Set(), edited: new Set() });
    }

    const unscopedKeptId = manager.appendMessage(user("Legacy unscoped compaction follows the Fresh marker."));
    manager.appendCompaction("UNSCOPED_POST_FRESH_SUMMARY", unscopedKeptId, 31, { readFiles: ["legacy.txt"], modifiedFiles: [] }, true, USAGE);
    manager.appendMessage(user("Resume after the unscoped compaction."));
    manager.appendMessage(assistant("More current work."));
    await assert.rejects(session.compact(), /cancel/i);
    assert.equal(captured.length, 3);
    const unsafe = prepareFreshCompaction(captured[2], boundary);
    assert.equal(unsafe.ok, false);
    if (!unsafe.ok) assert.equal(unsafe.problem.code, "unscoped_compaction");
    const unsafeOutcome = await runFreshCompaction({
      event: { ...captured[2], signal: new AbortController().signal },
      boundary,
      nativeSessionId: manager.getSessionId(),
      model,
      modelRegistry: { streamSimple },
    });
    assert.equal(unsafeOutcome.kind, "cancel");
    assert.deepEqual(unsafeOutcome.result, { cancel: true });
  });
});
