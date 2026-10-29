import { isDeepStrictEqual } from "node:util";
import {
  buildSessionProjection,
  compact as nativeCompact,
  type ExtensionContext,
  type SessionBeforeCompactEvent,
  type SessionEntry,
  type SessionProjection,
} from "@earendil-works/pi-coding-agent";

type AgentMessage = SessionProjection["messages"][number];
type CompactionPreparation = SessionBeforeCompactEvent["preparation"];

type ContextProblemCode =
  | "invalid_identity"
  | "invalid_archive"
  | "malformed_marker"
  | "owner_conflict"
  | "space_conflict"
  | "required_marker_missing"
  | "required_marker_mismatch"
  | "required_marker_off_branch"
  | "latest_marker_off_branch"
  | "invalid_branch"
  | "canonical_projection_mismatch"
  | "unscoped_compaction"
  | "unscoped_branch_summary"
  | "boundary_not_projected"
  | "tool_pair_boundary"
  | "invalid_compaction_preparation"
  | "native_cut_before_boundary"
  | "no_scoped_compaction_content"
  | "scoped_compaction_failed";

export const MUSTR_CONTEXT_CUSTOM_TYPE = "mustr";
export const FRESH_ASSIGNMENT_MARKER_KEY = "assignmentContext";
export const FRESH_ASSIGNMENT_MARKER_SCHEMA = "mustr.assignment-context.v1";
export const FRESH_COMPACTION_TAG_KEY = "mustrContextWindow";
export const FRESH_COMPACTION_TAG_SCHEMA = "mustr.context-window.v1";

/**
 * Structured metadata stored only after an authenticated hub Assignment is actually inserted.
 * `authority` is a local provenance discriminator, not a cryptographic token.
 */
export interface FreshAssignmentContextMarker {
  schema: typeof FRESH_ASSIGNMENT_MARKER_SCHEMA;
  authority: "authenticated-hub-assignment";
  kind: "assignment";
  mode: "fresh";
  taskId: string;
  revision: number;
  messageId: string;
  nativeOwnerId: string;
  nativeSessionId: string;
  originSpaceId: string;
}

/** The archive entry id binds a Fresh assignment to one immutable native session entry. */
export interface FreshContextBoundary extends FreshAssignmentContextMarker {
  entryId: string;
}

/** A current authoritative Fresh dispatch requiring an inserted marker before model input. */
export interface RequiredFreshAssignment extends FreshAssignmentContextMarker {}

export interface ContextWindowProblem {
  code: ContextProblemCode;
  message: string;
}

export type FreshContextResolution =
  | { kind: "baseline" }
  | { kind: "fresh"; boundary: FreshContextBoundary }
  | { kind: "blocked"; problem: ContextWindowProblem };

export interface ResolveFreshContextInput {
  /** All non-header entries from the current native session, in append order. */
  allEntries: readonly SessionEntry[];
  /** The current selected branch in parent order. */
  branchEntries: readonly SessionEntry[];
  nativeSessionId: string;
  /** Optional live identity pin; archive provenance remains usable when this is absent. */
  nativeOwnerId?: string;
  /** Only pass a proven current/captured-run space binding; never infer it from role or prose. */
  provenSpaceId?: string;
  /** Distinguishes a required Fresh anchor from a true no-Fresh Reuse baseline. */
  requiredFresh?: RequiredFreshAssignment;
}

export interface ProjectFreshContextInput {
  resolution: FreshContextResolution;
  projection: SessionProjection;
  branchEntries: readonly SessionEntry[];
  incomingMessages: readonly AgentMessage[];
  blockerTimestamp?: number;
}

export type ProjectFreshContextResult =
  | { ok: true; kind: "baseline" | "fresh"; messages: AgentMessage[]; boundary?: FreshContextBoundary }
  | { ok: false; problem: ContextWindowProblem; messages: [AgentMessage] };

export interface FreshCompactionTag {
  schema: typeof FRESH_COMPACTION_TAG_SCHEMA;
  boundary: FreshContextBoundary;
}

export type PrepareFreshCompactionResult =
  | { ok: true; preparation: CompactionPreparation; tag: FreshCompactionTag }
  | { ok: false; problem: ContextWindowProblem };

export type FreshCompactionOutcome =
  | { kind: "cancel"; result: { cancel: true }; problem: ContextWindowProblem }
  | { kind: "compaction"; result: { compaction: Awaited<ReturnType<typeof nativeCompact>> & { details: Record<string, unknown> } }; tag: FreshCompactionTag };

export interface RunFreshCompactionInput {
  event: SessionBeforeCompactEvent;
  boundary: FreshContextBoundary;
  nativeSessionId: string;
  model: ExtensionContext["model"];
  modelRegistry: Pick<ExtensionContext["modelRegistry"], "streamSimple">;
  thinkingLevel?: ExtensionContext["thinkingLevel"];
}

interface ParsedMarker {
  entry: SessionEntry;
  marker: FreshAssignmentContextMarker;
}

interface ProjectedMessageRecord {
  message: AgentMessage;
  sourceEntry: SessionEntry;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function nonblank(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

function problem(code: ContextProblemCode): ContextWindowProblem {
  const messages: Record<ContextProblemCode, string> = {
    invalid_identity: "The native session or task-owner identity could not be verified.",
    invalid_archive: "The current native session archive could not be inspected safely.",
    malformed_marker: "A stored Mustr assignment-context marker is malformed or unsupported.",
    owner_conflict: "Fresh assignment markers disagree about their native owner.",
    space_conflict: "Fresh assignment markers do not have one authoritative origin-space binding.",
    required_marker_missing: "The current Fresh assignment has no inserted authoritative marker.",
    required_marker_mismatch: "The stored Fresh marker does not match the current authoritative assignment.",
    required_marker_off_branch: "The required Fresh marker is not on the selected native-session branch.",
    latest_marker_off_branch: "The latest Fresh marker is not on the selected native-session branch.",
    invalid_branch: "The selected native-session branch cannot be verified without rewriting history.",
    canonical_projection_mismatch: "Incoming messages do not exactly match the canonical native-session projection.",
    unscoped_compaction: "A compaction after the Fresh boundary lacks matching window provenance.",
    unscoped_branch_summary: "A branch summary after the Fresh boundary is not safely scoped.",
    boundary_not_projected: "The Fresh boundary is absent from model context and no matching scoped compaction represents it.",
    tool_pair_boundary: "The Fresh boundary or compaction input would separate a tool call from its result.",
    invalid_compaction_preparation: "Native compaction preparation does not match the public canonical projection.",
    native_cut_before_boundary: "The native compaction cut would cross the Fresh boundary.",
    no_scoped_compaction_content: "There is no safe current-window content to summarize.",
    scoped_compaction_failed: "Scoped summarization failed; ordinary full-history compaction was cancelled.",
  };
  return { code, message: messages[code] };
}

function blockedMessage(value: ContextWindowProblem, timestamp = 0): AgentMessage {
  return {
    role: "user",
    content: [{
      type: "text",
      text: `Mustr could not safely select this assignment context (${value.code}). ${value.message} Stop and request guidance; no prior conversation was provided to this request.`,
    }],
    timestamp: Number.isFinite(timestamp) ? timestamp : 0,
  };
}

function blockedProjection(value: ContextWindowProblem, timestamp?: number): ProjectFreshContextResult {
  return { ok: false, problem: value, messages: [blockedMessage(value, timestamp)] };
}

function parseMarker(entry: SessionEntry): { kind: "none" } | { kind: "marker"; marker: FreshAssignmentContextMarker } | { kind: "invalid" } {
  if (entry.type !== "custom_message" || entry.customType !== MUSTR_CONTEXT_CUSTOM_TYPE || !isRecord(entry.details)) return { kind: "none" };
  if (!Object.hasOwn(entry.details, FRESH_ASSIGNMENT_MARKER_KEY)) return { kind: "none" };
  const raw = entry.details[FRESH_ASSIGNMENT_MARKER_KEY];
  if (!isRecord(raw)) return { kind: "invalid" };
  const marker: FreshAssignmentContextMarker = {
    schema: raw.schema as typeof FRESH_ASSIGNMENT_MARKER_SCHEMA,
    authority: raw.authority as "authenticated-hub-assignment",
    kind: raw.kind as "assignment",
    mode: raw.mode as "fresh",
    taskId: raw.taskId as string,
    revision: raw.revision as number,
    messageId: raw.messageId as string,
    nativeOwnerId: raw.nativeOwnerId as string,
    nativeSessionId: raw.nativeSessionId as string,
    originSpaceId: raw.originSpaceId as string,
  };
  if (
    marker.schema !== FRESH_ASSIGNMENT_MARKER_SCHEMA || marker.authority !== "authenticated-hub-assignment" ||
    marker.kind !== "assignment" || marker.mode !== "fresh" ||
    !nonblank(marker.taskId) || !Number.isSafeInteger(marker.revision) || marker.revision < 1 ||
    !nonblank(marker.messageId) || !nonblank(marker.nativeOwnerId) || !nonblank(marker.nativeSessionId) || !nonblank(marker.originSpaceId) ||
    entry.details.id !== marker.messageId || entry.details.kind !== "assignment" ||
    entry.details.task_id !== marker.taskId || entry.details.revision !== marker.revision || entry.details.context !== "fresh"
  ) return { kind: "invalid" };
  return { kind: "marker", marker };
}

function matchesExpected(marker: FreshAssignmentContextMarker, expected: RequiredFreshAssignment): boolean {
  return isDeepStrictEqual(marker, expected);
}

function indexBranch(branchEntries: readonly SessionEntry[]): Map<string, number> | undefined {
  const indices = new Map<string, number>();
  let previousId: string | null = null;
  for (let index = 0; index < branchEntries.length; index++) {
    const entry = branchEntries[index];
    if (!nonblank(entry.id) || indices.has(entry.id)) return undefined;
    if (index === 0 ? entry.parentId !== null : entry.parentId !== previousId) return undefined;
    indices.set(entry.id, index);
    previousId = entry.id;
  }
  return indices;
}

function entryMarkerEquals(entry: SessionEntry, boundary: FreshContextBoundary): boolean {
  const parsed = parseMarker(entry);
  if (parsed.kind !== "marker" || entry.id !== boundary.entryId) return false;
  const { entryId: _entryId, ...metadata } = boundary;
  return isDeepStrictEqual(parsed.marker, metadata);
}

export function resolveFreshContext(input: ResolveFreshContextInput): FreshContextResolution {
  try {
    if (!nonblank(input.nativeSessionId) || (input.nativeOwnerId !== undefined && !nonblank(input.nativeOwnerId)) ||
        (input.provenSpaceId !== undefined && !nonblank(input.provenSpaceId))) {
      return { kind: "blocked", problem: problem("invalid_identity") };
    }
    const branchIndices = indexBranch(input.branchEntries);
    if (!branchIndices) return { kind: "blocked", problem: problem("invalid_branch") };
    const candidates: ParsedMarker[] = [];
    const matchingMessageEntries = new Map<string, ParsedMarker[]>();
    for (const entry of input.allEntries) {
      const parsed = parseMarker(entry);
      if (parsed.kind === "invalid") return { kind: "blocked", problem: problem("malformed_marker") };
      if (parsed.kind !== "marker") continue;
      const record = { entry, marker: parsed.marker };
      const sameMessage = matchingMessageEntries.get(parsed.marker.messageId) ?? [];
      sameMessage.push(record);
      matchingMessageEntries.set(parsed.marker.messageId, sameMessage);
      if (parsed.marker.nativeSessionId === input.nativeSessionId) candidates.push(record);
    }

    if (input.requiredFresh) {
      if (input.requiredFresh.nativeSessionId !== input.nativeSessionId ||
          (input.nativeOwnerId !== undefined && input.requiredFresh.nativeOwnerId !== input.nativeOwnerId) ||
          (input.provenSpaceId !== undefined && input.requiredFresh.originSpaceId !== input.provenSpaceId)) {
        return { kind: "blocked", problem: problem("required_marker_mismatch") };
      }
      const matching = matchingMessageEntries.get(input.requiredFresh.messageId) ?? [];
      const exact = matching.find(candidate => matchesExpected(candidate.marker, input.requiredFresh!));
      if (!exact) {
        return { kind: "blocked", problem: matching.length ? problem("required_marker_mismatch") : problem("required_marker_missing") };
      }
      if (!branchIndices.has(exact.entry.id)) return { kind: "blocked", problem: problem("required_marker_off_branch") };
    }

    if (input.nativeOwnerId !== undefined && candidates.some(candidate => candidate.marker.nativeOwnerId !== input.nativeOwnerId)) {
      return { kind: "blocked", problem: problem("owner_conflict") };
    }
    if (input.nativeOwnerId === undefined && new Set(candidates.map(candidate => candidate.marker.nativeOwnerId)).size > 1) {
      return { kind: "blocked", problem: problem("owner_conflict") };
    }
    const applicable = input.provenSpaceId === undefined
      ? candidates
      : candidates.filter(candidate => candidate.marker.originSpaceId === input.provenSpaceId);
    if (input.provenSpaceId === undefined && new Set(candidates.map(candidate => candidate.marker.originSpaceId)).size > 1) {
      return { kind: "blocked", problem: problem("space_conflict") };
    }
    if (input.provenSpaceId !== undefined && candidates.length > 0 && applicable.length === 0) {
      return { kind: "blocked", problem: problem("space_conflict") };
    }
    if (applicable.length === 0) {
      return input.requiredFresh
        ? { kind: "blocked", problem: problem("required_marker_missing") }
        : { kind: "baseline" };
    }

    const latest = applicable[applicable.length - 1];
    if (!branchIndices.has(latest.entry.id)) return { kind: "blocked", problem: problem("latest_marker_off_branch") };
    const boundary: FreshContextBoundary = { ...latest.marker, entryId: latest.entry.id };
    return { kind: "fresh", boundary };
  } catch {
    return { kind: "blocked", problem: problem("invalid_archive") };
  }
}

function freshTag(boundary: FreshContextBoundary): FreshCompactionTag {
  return { schema: FRESH_COMPACTION_TAG_SCHEMA, boundary: { ...boundary } };
}

function matchesFreshTag(entry: SessionEntry, boundary: FreshContextBoundary): boolean {
  if (entry.type !== "compaction" || !isRecord(entry.details)) return false;
  const tag = entry.details[FRESH_COMPACTION_TAG_KEY];
  return isRecord(tag) && tag.schema === FRESH_COMPACTION_TAG_SCHEMA && isDeepStrictEqual(tag.boundary, boundary);
}

function projectedRecords(projection: SessionProjection): ProjectedMessageRecord[] | undefined {
  const records: ProjectedMessageRecord[] = [];
  for (const projected of projection.entries) {
    if (!projected || !projected.sourceEntry || !Array.isArray(projected.messages)) return undefined;
    for (const message of projected.messages) records.push({ message, sourceEntry: projected.sourceEntry });
  }
  if (!isDeepStrictEqual(records.map(record => record.message), projection.messages)) return undefined;
  return records;
}

function nonSystemMessages(records: readonly ProjectedMessageRecord[]): ProjectedMessageRecord[] {
  return records.filter(record => record.message.role !== "system");
}

function hasCompleteToolPairs(messages: readonly AgentMessage[]): boolean {
  const calls = new Set<string>();
  const results = new Set<string>();
  for (const message of messages) {
    if (message.role === "assistant") {
      for (const block of message.content) {
        if (block.type === "toolCall") {
          if (calls.has(block.id)) return false;
          calls.add(block.id);
        }
      }
    } else if (message.role === "toolResult") {
      if (!calls.has(message.toolCallId) || results.has(message.toolCallId)) return false;
      results.add(message.toolCallId);
    }
  }
  return calls.size === results.size;
}

export function projectFreshContext(input: ProjectFreshContextInput): ProjectFreshContextResult {
  try {
    if (input.resolution.kind === "blocked") return blockedProjection(input.resolution.problem, input.blockerTimestamp);
    const branchIndices = indexBranch(input.branchEntries);
    const records = projectedRecords(input.projection);
    if (!branchIndices || !records || records.some(record => !branchIndices.has(record.sourceEntry.id))) {
      return blockedProjection(problem("canonical_projection_mismatch"), input.blockerTimestamp);
    }
    const canonicalRecords = nonSystemMessages(records);
    const canonical = canonicalRecords.map(record => record.message);
    if (input.incomingMessages.length < canonical.length) {
      return blockedProjection(problem("canonical_projection_mismatch"), input.blockerTimestamp);
    }
    const incomingBySource = new Map<string, AgentMessage[]>();
    for (let index = 0; index < canonical.length; index++) {
      const record = canonicalRecords[index];
      const incoming = input.incomingMessages[index];
      if (!isDeepStrictEqual(incoming, canonical[index])) {
        return blockedProjection(problem("canonical_projection_mismatch"), input.blockerTimestamp);
      }
      const sourceId = record.sourceEntry.id;
      if (!sourceId) return blockedProjection(problem("canonical_projection_mismatch"), input.blockerTimestamp);
      const sourceMessages = incomingBySource.get(sourceId) ?? [];
      sourceMessages.push(incoming);
      incomingBySource.set(sourceId, sourceMessages);
    }
    if (input.resolution.kind === "baseline") {
      return { ok: true, kind: "baseline", messages: [...input.incomingMessages] };
    }

    const boundary = input.resolution.boundary;
    const boundaryIndex = branchIndices.get(boundary.entryId);
    if (boundaryIndex === undefined || !entryMarkerEquals(input.branchEntries[boundaryIndex], boundary)) {
      return blockedProjection(problem("latest_marker_off_branch"), input.blockerTimestamp);
    }
    for (const entry of input.branchEntries.slice(boundaryIndex + 1)) {
      if (entry.type === "branch_summary") {
        return blockedProjection(problem("unscoped_branch_summary"), input.blockerTimestamp);
      }
      if (entry.type === "compaction") {
        if (!matchesFreshTag(entry, boundary)) return blockedProjection(problem("unscoped_compaction"), input.blockerTimestamp);
        const cutIndex = branchIndices.get(entry.firstKeptEntryId);
        if (cutIndex === undefined || cutIndex <= boundaryIndex) {
          return blockedProjection(problem("native_cut_before_boundary"), input.blockerTimestamp);
        }
      }
    }
    const projectionEntryIndex = new Map(input.projection.entries.map((entry, index) => [entry.sourceEntry.id, index]));
    const contributingCompactions: Array<{ entry: SessionEntry; projectionIndex: number; branchIndex: number }> = [];
    for (const projected of input.projection.entries) {
      const source = projected.sourceEntry;
      const sourceBranchIndex = branchIndices.get(source.id);
      if (sourceBranchIndex === undefined || sourceBranchIndex <= boundaryIndex || projected.messages.length === 0) continue;
      if (source.type === "branch_summary") {
        return blockedProjection(problem("unscoped_branch_summary"), input.blockerTimestamp);
      }
      if (source.type === "compaction") {
        if (!matchesFreshTag(source, boundary)) {
          return blockedProjection(problem("unscoped_compaction"), input.blockerTimestamp);
        }
        const cutIndex = branchIndices.get(source.firstKeptEntryId);
        if (cutIndex === undefined || cutIndex <= boundaryIndex) {
          return blockedProjection(problem("native_cut_before_boundary"), input.blockerTimestamp);
        }
        contributingCompactions.push({ entry: source, projectionIndex: projectionEntryIndex.get(source.id)!, branchIndex: sourceBranchIndex });
      }
    }
    const latestCompaction = contributingCompactions.sort((a, b) => b.branchIndex - a.branchIndex)[0];
    let startIndex: number;
    if (latestCompaction) {
      startIndex = latestCompaction.projectionIndex;
    } else {
      const markerProjectionIndex = projectionEntryIndex.get(boundary.entryId);
      if (markerProjectionIndex === undefined || input.projection.entries[markerProjectionIndex].messages.length === 0) {
        return blockedProjection(problem("boundary_not_projected"), input.blockerTimestamp);
      }
      startIndex = markerProjectionIndex;
    }
    const suffixRecords = nonSystemMessages(
      input.projection.entries.slice(startIndex).flatMap(entry => entry.messages.map(message => ({ message, sourceEntry: entry.sourceEntry }))),
    );
    const messages: AgentMessage[] = [];
    const sourceOffsets = new Map<string, number>();
    for (const record of suffixRecords) {
      const sourceId = record.sourceEntry.id;
      if (!sourceId) return blockedProjection(problem("canonical_projection_mismatch"), input.blockerTimestamp);
      const sourceMessages = incomingBySource.get(sourceId);
      const offset = sourceOffsets.get(sourceId) ?? 0;
      const incoming = sourceMessages?.[offset];
      if (!incoming) return blockedProjection(problem("canonical_projection_mismatch"), input.blockerTimestamp);
      sourceOffsets.set(sourceId, offset + 1);
      messages.push(incoming);
    }
    const tail = input.incomingMessages.slice(canonical.length);
    const finalMessages = [...messages, ...tail];
    if (!hasCompleteToolPairs(finalMessages)) return blockedProjection(problem("tool_pair_boundary"), input.blockerTimestamp);
    return { ok: true, kind: "fresh", messages: finalMessages, boundary: { ...boundary } };
  } catch {
    return blockedProjection(problem("invalid_archive"), input.blockerTimestamp);
  }
}

function preparationHasExpectedShape(preparation: CompactionPreparation): boolean {
  return Array.isArray(preparation.messagesToSummarize) && Array.isArray(preparation.turnPrefixMessages) &&
    typeof preparation.firstKeptEntryId === "string" && preparation.firstKeptEntryId.length > 0 &&
    typeof preparation.isSplitTurn === "boolean" && Number.isFinite(preparation.tokensBefore) &&
    isRecord(preparation.settings) && isRecord(preparation.fileOps) &&
    preparation.fileOps.read instanceof Set && preparation.fileOps.written instanceof Set && preparation.fileOps.edited instanceof Set &&
    (preparation.previousSummary === undefined || typeof preparation.previousSummary === "string");
}

export function prepareFreshCompaction(event: SessionBeforeCompactEvent, boundary: FreshContextBoundary): PrepareFreshCompactionResult {
  try {
    const branchIndices = indexBranch(event.branchEntries);
    if (!branchIndices) return { ok: false, problem: problem("invalid_branch") };
    const boundaryIndex = branchIndices.get(boundary.entryId);
    if (boundaryIndex === undefined || !entryMarkerEquals(event.branchEntries[boundaryIndex], boundary)) {
      return { ok: false, problem: problem("latest_marker_off_branch") };
    }
    for (const entry of event.branchEntries.slice(boundaryIndex + 1)) {
      if (entry.type === "branch_summary") return { ok: false, problem: problem("unscoped_branch_summary") };
      if (entry.type === "compaction") {
        if (!matchesFreshTag(entry, boundary)) return { ok: false, problem: problem("unscoped_compaction") };
        const scopedCut = branchIndices.get(entry.firstKeptEntryId);
        if (scopedCut === undefined || scopedCut <= boundaryIndex) return { ok: false, problem: problem("native_cut_before_boundary") };
      }
    }
    const preparation = event.preparation;
    if (!preparationHasExpectedShape(preparation)) return { ok: false, problem: problem("invalid_compaction_preparation") };
    const cutBranchIndex = branchIndices.get(preparation.firstKeptEntryId);
    if (cutBranchIndex === undefined || cutBranchIndex < boundaryIndex) {
      return { ok: false, problem: problem("native_cut_before_boundary") };
    }
    const leafId = event.branchEntries.at(-1)?.id ?? null;
    const projection = buildSessionProjection([...event.branchEntries], leafId);
    const records = projectedRecords(projection);
    if (!records || records.some(record => !branchIndices.has(record.sourceEntry.id))) {
      return { ok: false, problem: problem("invalid_compaction_preparation") };
    }
    const projectionEntryIndex = new Map(projection.entries.map((entry, index) => [entry.sourceEntry.id, index]));
    const cutProjectionIndex = projectionEntryIndex.get(preparation.firstKeptEntryId);
    if (cutProjectionIndex === undefined) return { ok: false, problem: problem("invalid_compaction_preparation") };

    const previousCompactionIndex = projection.entries.findIndex(entry => entry.sourceEntry.type === "compaction" && entry.messages.length > 0);
    const previousCompaction = previousCompactionIndex >= 0 ? projection.entries[previousCompactionIndex].sourceEntry : undefined;
    if (previousCompaction?.type === "compaction") {
      if (preparation.previousSummary !== previousCompaction.summary) return { ok: false, problem: problem("invalid_compaction_preparation") };
      const previousBranchIndex = branchIndices.get(previousCompaction.id);
      if (previousBranchIndex === undefined) return { ok: false, problem: problem("invalid_compaction_preparation") };
      if (previousBranchIndex > boundaryIndex && !matchesFreshTag(previousCompaction, boundary)) {
        return { ok: false, problem: problem("unscoped_compaction") };
      }
      if (previousBranchIndex > boundaryIndex) {
        const previousCut = branchIndices.get(previousCompaction.firstKeptEntryId);
        if (previousCut === undefined || previousCut <= boundaryIndex) return { ok: false, problem: problem("native_cut_before_boundary") };
      }
    } else if (preparation.previousSummary !== undefined) {
      return { ok: false, problem: problem("invalid_compaction_preparation") };
    }

    for (const projected of projection.entries) {
      const source = projected.sourceEntry;
      const sourceBranchIndex = branchIndices.get(source.id);
      if (sourceBranchIndex === undefined || sourceBranchIndex <= boundaryIndex || projected.messages.length === 0) continue;
      if (source.type === "branch_summary") return { ok: false, problem: problem("unscoped_branch_summary") };
      if (source.type === "compaction") {
        if (!matchesFreshTag(source, boundary)) return { ok: false, problem: problem("unscoped_compaction") };
        const scopedCut = branchIndices.get(source.firstKeptEntryId);
        if (scopedCut === undefined || scopedCut <= boundaryIndex) return { ok: false, problem: problem("native_cut_before_boundary") };
      }
    }

    const projectionStart = previousCompactionIndex >= 0 ? previousCompactionIndex + 1 : 0;
    if (cutProjectionIndex < projectionStart) return { ok: false, problem: problem("invalid_compaction_preparation") };
    const expected: ProjectedMessageRecord[] = [];
    for (const projected of projection.entries.slice(projectionStart, cutProjectionIndex)) {
      if (projected.sourceEntry.type === "compaction") continue;
      for (const message of projected.messages) {
        if (message.role !== "system") expected.push({ message, sourceEntry: projected.sourceEntry });
      }
    }
    const history = preparation.messagesToSummarize;
    const prefix = preparation.turnPrefixMessages;
    if (preparation.isSplitTurn !== (prefix.length > 0) || history.length + prefix.length !== expected.length) {
      return { ok: false, problem: problem("invalid_compaction_preparation") };
    }
    const nativeMessages = [...history, ...prefix];
    if (expected.some((record, index) => !isDeepStrictEqual(record.message, nativeMessages[index]))) {
      return { ok: false, problem: problem("invalid_compaction_preparation") };
    }

    const historyRecords = expected.slice(0, history.length);
    const prefixRecords = expected.slice(history.length);
    const inWindow = (record: ProjectedMessageRecord) => (branchIndices.get(record.sourceEntry.id) ?? -1) >= boundaryIndex;
    const scopedHistory = historyRecords.filter(inWindow).map(record => structuredClone(record.message));
    const scopedPrefix = prefixRecords.filter(inWindow).map(record => structuredClone(record.message));
    const scopedMessages = [...scopedHistory, ...scopedPrefix];
    if (scopedMessages.length === 0) return { ok: false, problem: problem("no_scoped_compaction_content") };
    if (!hasCompleteToolPairs(scopedMessages)) return { ok: false, problem: problem("tool_pair_boundary") };

    const keepPreviousSummary = previousCompaction?.type === "compaction" &&
      (branchIndices.get(previousCompaction.id) ?? -1) > boundaryIndex && matchesFreshTag(previousCompaction, boundary);
    const copied: CompactionPreparation = {
      ...preparation,
      firstKeptEntryId: preparation.firstKeptEntryId,
      messagesToSummarize: scopedHistory,
      turnPrefixMessages: scopedPrefix,
      isSplitTurn: preparation.isSplitTurn,
      tokensBefore: preparation.tokensBefore,
      previousSummary: keepPreviousSummary ? preparation.previousSummary : undefined,
      fileOps: { read: new Set<string>(), written: new Set<string>(), edited: new Set<string>() },
      settings: { ...preparation.settings },
    };
    return { ok: true, preparation: copied, tag: freshTag(boundary) };
  } catch {
    return { ok: false, problem: problem("invalid_compaction_preparation") };
  }
}

export async function runFreshCompaction(input: RunFreshCompactionInput): Promise<FreshCompactionOutcome> {
  const prepared = prepareFreshCompaction(input.event, input.boundary);
  if (!prepared.ok) return { kind: "cancel", result: { cancel: true }, problem: prepared.problem };
  try {
    if (!input.model || input.nativeSessionId !== input.boundary.nativeSessionId || input.event.signal.aborted) {
      return { kind: "cancel", result: { cancel: true }, problem: problem("scoped_compaction_failed") };
    }
    const streamFn: Parameters<typeof nativeCompact>[7] = (model, context, options) =>
      input.modelRegistry.streamSimple(model, context, options);
    const compacted = await nativeCompact(
      prepared.preparation,
      input.model,
      undefined,
      undefined,
      input.event.customInstructions,
      input.event.signal,
      input.thinkingLevel,
      streamFn,
      undefined,
      undefined,
      undefined,
      input.nativeSessionId,
    );
    if (typeof compacted.summary !== "string" || compacted.summary.trim().length === 0 || !isRecord(compacted.details)) {
      return { kind: "cancel", result: { cancel: true }, problem: problem("scoped_compaction_failed") };
    }
    const details = { ...compacted.details, [FRESH_COMPACTION_TAG_KEY]: prepared.tag };
    return { kind: "compaction", result: { compaction: { ...compacted, details } }, tag: prepared.tag };
  } catch {
    return { kind: "cancel", result: { cancel: true }, problem: problem("scoped_compaction_failed") };
  }
}
