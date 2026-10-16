import test from "node:test";
import assert from "node:assert/strict";
import { AgentSession, SessionManager } from "@earendil-works/pi-coding-agent";
import { insertionEvidence } from "../transport.mjs";

// Exercise the installed dependency's actual delivery methods, not a fake sendMessage
// that manufactures extension callbacks. No resource discovery, credentials or model.
for (const streaming of [false, true]) test(`Pi context-only insertion evidence (streaming=${streaming})`, async () => {
  const manager = SessionManager.inMemory("/fixture");
  const publicEvents = [], extensionEvents = [];
  const session = {
    isStreaming: streaming, sessionManager: manager, _pendingCustomMessages: [],
    _refreshFinalizedContext() {},
    _eventListeners: [event => publicEvents.push(event.type)],
    _extensionRunner: { async emitMessageEnd(event) { extensionEvents.push(event.type); } },
    _emit: AgentSession.prototype._emit,
    _appendCustomMessage: AgentSession.prototype._appendCustomMessage,
  };
  manager.appendCustomEntry("mustr-received", { id: "m1", state: "offered" });
  assert.equal(insertionEvidence(manager.getBranch(), "m1"), "attempted");
  await AgentSession.prototype.sendCustomMessage.call(session,
    { customType: "mustr", content: "Information", display: true, details: { id: "m1" } },
    { triggerTurn: false, deliverAs: streaming ? "steer" : "followUp" });
  if (streaming) {
    assert.equal(insertionEvidence(manager.getBranch(), "m1"), "attempted");
    AgentSession.prototype._flushPendingCustomMessages.call(session);
  }
  assert.equal(insertionEvidence(manager.getBranch(), "m1"), "inserted");
  assert.deepEqual(publicEvents, ["message_start", "message_end"]);
  assert.deepEqual(extensionEvents, [], "Pi 1.2 context-only insertion bypasses extension message_end");
  // Control: normal agent events DO reach extensions. This distinction caused the bug.
  await AgentSession.prototype._emitExtensionEvent.call(session,
    { type: "message_end", message: { role: "custom", customType: "mustr" } });
  assert.deepEqual(extensionEvents, ["message_end"]);
});

test("insertion evidence requires the matching custom message on the active branch", () => {
  const manager = SessionManager.inMemory("/fixture");
  assert.equal(insertionEvidence(manager.getBranch(), "m1"), "absent");
  const marker = manager.appendCustomEntry("mustr-received", { id: "m1", state: "offered" });
  manager.appendCustomMessageEntry("other-extension", "not ours", true, { id: "m1" });
  manager.appendCustomMessageEntry("mustr", "different ID", true, { id: "m2" });
  assert.equal(insertionEvidence(manager.getBranch(), "m1"), "attempted");
  manager.appendCustomMessageEntry("mustr", "ours", true, { id: "m1" });
  assert.equal(insertionEvidence(manager.getBranch(), "m1"), "inserted");
  manager.branch(marker);
  assert.equal(insertionEvidence(manager.getBranch(), "m1"), "attempted", "abandoned branches are not receipts");
});
