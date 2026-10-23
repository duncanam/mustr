import test from "node:test";
import assert from "node:assert/strict";
import { fixture, binary, eventually } from "../../tests/support.mjs";
import { HubClient } from "../transport.mjs";
import { harness } from "./adapter-harness.mjs";

test("manager cancellation tool has an exact guarded schema, validates inputs, dispatches once, and is call-alone", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary;
  process.env.HERDR_PANE_ID = "w1:p1";
  const { default: extension } = await import(`../index.ts?manager-cancel=${Date.now()}`);
  const h = harness(f.directory);
  await extension(h.api);
  await h.emit("session_start");
  f.beforeCleanup(() => h.emit("session_shutdown"));
  await h.commands.get("role").handler("implementor manager-tool-test", h.context);
  await eventually(() => h.tools.get("mustr_roster").execute("ready", {}, undefined));

  const tool = h.tools.get("mustr_cancel_owner_tasks");
  assert.ok(tool);
  assert.match(tool.description, /manager-only/i);
  assert.match(tool.description, /explicit user-directed redirection/i);
  assert.match(tool.description, /does NOT stop tools or external jobs/i);
  const schema = tool.parameters;
  assert.equal(schema.additionalProperties, false);
  assert.deepEqual(schema.required, ["owner_id", "scope", "expected_guard", "text"]);
  assert.deepEqual(Object.keys(schema.properties).sort(), ["expected_guard", "owner_id", "scope", "text"]);
  assert.deepEqual(schema.properties.scope.anyOf.map(option => option.const), ["queued_only", "all_unfinished"]);
  assert.equal(schema.properties.expected_guard.pattern, "^[0-9a-f]{24}$");
  assert.equal(schema.properties.expected_guard.minLength, 24);
  assert.equal(schema.properties.expected_guard.maxLength, 24);

  const guard = "0123456789abcdef01234567";
  await assert.rejects(tool.execute("missing", {
    owner_id: "owner", scope: "queued_only", expected_guard: guard,
  }, undefined), /exactly owner_id, scope, expected_guard, and text/);
  await assert.rejects(tool.execute("unknown-scope", {
    owner_id: "owner", scope: "all", expected_guard: guard, text: "Redirected by the user.",
  }, undefined), /scope and guard must match the roster/);
  await assert.rejects(tool.execute("bad-guard", {
    owner_id: "owner", scope: "all_unfinished", expected_guard: "not-a-guard", text: "Redirected by the user.",
  }, undefined), /scope and guard must match the roster/);
  await assert.rejects(tool.execute("extra", {
    owner_id: "owner", scope: "all_unfinished", expected_guard: guard,
    text: "Redirected by the user.", unexpected: true,
  }, undefined), /exactly owner_id, scope, expected_guard, and text/);

  const originalRequest = HubClient.prototype.request;
  const dispatched = [];
  HubClient.prototype.request = async function(command, ...args) {
    if (command.op === "cancel_owner_tasks") {
      dispatched.push(structuredClone(command));
      return { cancelled_count: 0, cancelled_task_ids: [], actor_id: "pi-session", notices_queued: 0, notice_status: "queued_not_delivered" };
    }
    return originalRequest.call(this, command, ...args);
  };
  t.after(() => { HubClient.prototype.request = originalRequest; });
  const payload = {
    owner_id: "worker-id",
    scope: "queued_only",
    expected_guard: guard,
    text: "The user explicitly redirected queued work.",
  };
  const result = await tool.execute("valid", payload, undefined);
  assert.deepEqual(dispatched, [{ op: "cancel_owner_tasks", ...payload }]);
  assert.equal(JSON.parse(result.content[0].text).notice_status, "queued_not_delivered");

  h.entries.push({ type: "message", message: { role: "assistant", content: [
    { type: "toolCall", name: "mustr_cancel_owner_tasks" },
    { type: "toolCall", name: "read" },
  ] } });
  assert.match((await h.emit("tool_call", { toolName: "mustr_cancel_owner_tasks" }))[0].reason, /alone/);
});
