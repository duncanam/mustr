import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { fixture, binary, eventually } from "../../tests/support.mjs";
import { HubClient, HubRemoteError } from "../transport.mjs";
import extension from "../index.ts";
import { harness } from "./adapter-harness.mjs";

// Unit transport seam only: production registration/lifecycle handlers execute,
// but this fake does not prove Rust or Pi runtime semantics.
export async function resetFixture(t, options = {}) {
  const cleanups = [];
  let h, original;
  // Stop adapter timers/clients BEFORE deleting fixture config. The shared
  // fixture's normal hook order otherwise races a late readonly context child.
  t.after(async () => {
    try { if (h) await h.root.current.emit("session_shutdown", { reason: "quit" }); }
    finally {
      if (original) Object.assign(HubClient.prototype, original);
      for (const cleanup of cleanups) await cleanup();
    }
  });
  const f = await fixture({ after: cleanup => cleanups.push(cleanup) });
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  h = harness(f.directory);
  const hub = { records: [], connections: [], statuses: [], capabilities: 0, claims: 0, receiptAttempts: [], completed: false,
    phase: undefined, claimed: false, claimHook: undefined, syncHook: undefined, failReceipt: 0,
    control: { plan_id: randomUUID(), token: randomUUID(), space_id: "w1", old_agent_id: "pi-session", name: "approved-name", role: "implementor", pane_id: "w1:p1", state: "pending" },
    prepare(state = "pending") { this.phase = "prepared"; this.control.state = state; },
    abandon() { this.phase = "abandoned"; },
  };
  original = { connect: HubClient.prototype.connect, request: HubClient.prototype.request, close: HubClient.prototype.close };
  HubClient.prototype.connect = async function(path, hello) {
    this.hello = { ...hello }; this.connected = true; hub.connections.push(this); hub.records.push({ op: "join", ...hello });
    if (hello.reset) {
      hub.receiptAttempts.push({ ...hello });
      const c = hub.control;
      if (hub.phase === "abandoned" || !hub.claimed || hello.reset.token !== c.token || hello.reset.plan_id !== c.plan_id
        || hello.agent_id === c.old_agent_id || hello.name !== c.name || hello.role !== c.role || hello.pane_id !== c.pane_id || hello.space_id !== c.space_id
        || (hub.freshId && hub.freshId !== hello.agent_id)) throw new HubRemoteError("Invalid or revoked fresh receipt");
      hub.freshId = hello.agent_id; hub.completed = true; hub.phase = "completed";
      if (hub.failReceipt-- > 0) throw new Error("Hub disconnected after storing receipt");
      return { protocol: 1, agent_id: hello.agent_id, delivery_policy: 1, inbox_notifications: 1, reset_protocol: 1, reset_receipt: true, context_selection: 1, context_modes: ["reuse", "fresh"] };
    }
    if (hub.claimed && hello.agent_id === hub.control.old_agent_id) throw new HubRemoteError("Old session identity is retired");
    return { protocol: 1, agent_id: hello.agent_id, delivery_policy: 1, inbox_notifications: 1, context_selection: 1, context_modes: ["reuse", "fresh"], ...(options.oldHub ? {} : { reset_protocol: 1 }) };
  };
  HubClient.prototype.request = async function(command) {
    if (!this.connected) throw new Error("Hub disconnected");
    hub.records.push({ ...command, agent: this.hello?.agent_id });
    if (hub.requestHook) await hub.requestHook(this, command);
    if (command.op === "reset_capability") { hub.capabilities++; return { reset_protocol: 1 }; }
    if (command.op === "sync") {
      if (hub.syncHook) await hub.syncHook(this, command);
      if (this.hello.agent_id === hub.control.old_agent_id && ["prepared", "incomplete"].includes(hub.phase)) {
        return { members: [], tasks: [], inbox: null, automation: null, reset_summary: { state: hub.phase }, reset_control: { ...hub.control } };
      }
      if (hub.phase === "abandoned" && this.hello.agent_id === hub.control.old_agent_id) throw new HubRemoteError("Connection is retired");
      const inbox = hub.inbox && command.receive ? hub.inbox : null;
      if (inbox) hub.inbox = undefined;
      return { members: [{ id: this.hello.agent_id, activity: command.activity }], tasks: [], inbox, insertion_policy: "live", automation: {} };
    }
    if (command.op === "reset_claim") {
      hub.claims++;
      if (hub.phase === "abandoned" || hub.claimed || command.token !== hub.control.token) throw new HubRemoteError("Claim denied; do not retry");
      hub.claimed = true; hub.control.state = "claimed";
      if (hub.claimHook) { const reply = await hub.claimHook(this, command); if (reply !== undefined) return reply; }
      return { claimed: true };
    }
    if (command.op === "reset_status") { hub.statuses.push({ ...command }); hub.control.state = command.status; hub.phase = "incomplete"; return { recorded: true }; }
    return {};
  };
  HubClient.prototype.close = function() { const connected = this.connected; this.connected = false; if (connected) this.onClose?.(); };
  await extension(h.api); await h.emit("session_start", { reason: "startup" });
  await h.commands.get("role").handler("implementor approved-name", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("ready", {}, undefined));
  async function discover() {
    const result = await h.tools.get("mustr_roster").execute("discover", {}, undefined);
    assert.equal(JSON.stringify(result).includes(hub.control.token), false);
    assert.equal(JSON.stringify(result).includes("reset_control"), false);
    return result;
  }
  function privateSurfaces() {
    return JSON.stringify(h.root.states.map(s => ({ notifications: s.notifications, statuses: s.statuses, entries: s.entries, messages: s.messages }))) + JSON.stringify(h.root.dispatches);
  }
  return { f, h, hub, discover, privateSurfaces };
}
