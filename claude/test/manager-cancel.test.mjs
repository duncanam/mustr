import test from "node:test";
import assert from "node:assert/strict";
import { fixture, eventually } from "./fixture.mjs";

async function initialize(mcp) {
  const initialized = await mcp.request("initialize", {
    protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "mustr-manager-cancel-test", version: "1" },
  });
  assert.equal(initialized.protocolVersion, "2024-11-05");
  mcp.notify("notifications/initialized");
  await mcp.request("ping");
}
function resultValue(response) {
  assert.equal(response.isError, false, response.content?.[0]?.text);
  return response.value;
}
async function joined(mcp) {
  const result = resultValue(await mcp.tool("mustr_join", { role: "implementor" }));
  assert.equal(result.joined, true);
  return result;
}
async function waitUntilIdle(f, peer, agentId) {
  return eventually(async () => {
    const member = (await f.members(peer)).find(value => value.id === agentId);
    return member?.activity === "idle" ? member : undefined;
  }, 5_000, "Claude adapter to advertise idle during mustr_wait");
}

test("Claude manager-cancellation schema, exact argument checks, call-alone fencing, and current identity", { timeout: 30_000 }, async t => {
  const f = await fixture(t);
  const { mcp } = f.startAdapter();
  await initialize(mcp);

  const listed = await mcp.request("tools/list");
  const definition = listed.tools.find(tool => tool.name === "mustr_cancel_owner_tasks");
  assert.ok(definition);
  assert.match(definition.description, /manager-only/i);
  assert.match(definition.description, /explicit user-directed redirection/i);
  assert.match(definition.description, /does NOT stop tools or external jobs/i);
  assert.deepEqual(definition.inputSchema.required, ["owner_id", "scope", "expected_guard", "text"]);
  assert.deepEqual(Object.keys(definition.inputSchema.properties).sort(), ["expected_guard", "owner_id", "scope", "text"]);
  assert.equal(definition.inputSchema.additionalProperties, false);
  assert.deepEqual(definition.inputSchema.properties.scope.enum, ["queued_only", "all_unfinished"]);
  assert.equal(definition.inputSchema.properties.expected_guard.pattern, "^[0-9a-f]{24}$");

  const beforeJoin = await mcp.tool("mustr_cancel_owner_tasks", {
    owner_id: "owner", scope: "all_unfinished", expected_guard: "0".repeat(24), text: "User redirected this work.",
  });
  assert.equal(beforeJoin.isError, true);
  assert.match(beforeJoin.content[0].text, /Not joined/);

  const participant = await joined(mcp);
  const peer = await f.observer();
  const roster = resultValue(await mcp.tool("mustr_roster"));
  const self = roster.members.find(member => member.id === participant.agent_id);
  const guard = self.task_queue.cancellation_guards.all_unfinished;
  assert.match(guard, /^[0-9a-f]{24}$/);

  const missing = await mcp.tool("mustr_cancel_owner_tasks", {
    owner_id: participant.agent_id, scope: "all_unfinished", expected_guard: guard,
  });
  assert.equal(missing.isError, true);
  assert.match(missing.content[0].text, /Missing required argument: text/);
  const extra = await mcp.tool("mustr_cancel_owner_tasks", {
    owner_id: participant.agent_id, scope: "all_unfinished", expected_guard: guard,
    text: "User redirected this work.", unexpected: true,
  });
  assert.equal(extra.isError, true);
  assert.match(extra.content[0].text, /Unknown argument: unexpected/);
  const badScope = await mcp.tool("mustr_cancel_owner_tasks", {
    owner_id: participant.agent_id, scope: "all", expected_guard: guard,
    text: "User redirected this work.",
  });
  assert.equal(badScope.isError, true);
  assert.match(badScope.content[0].text, /scope must be queued_only or all_unfinished/);
  const badGuard = await mcp.tool("mustr_cancel_owner_tasks", {
    owner_id: participant.agent_id, scope: "all_unfinished", expected_guard: "stale",
    text: "User redirected this work.",
  });
  assert.equal(badGuard.isError, true);
  assert.match(badGuard.content[0].text, /24-character cancellation guard/);

  const wait = mcp.send("tools/call", { name: "mustr_wait", arguments: { timeout_seconds: 10 } });
  await waitUntilIdle(f, peer, participant.agent_id);
  const busyCall = await mcp.tool("mustr_cancel_owner_tasks", {
    owner_id: participant.agent_id, scope: "all_unfinished", expected_guard: guard,
    text: "User redirected this work.",
  });
  assert.equal(busyCall.isError, true);
  assert.match(busyCall.content[0].text, /Another mustr tool call is in progress/);
  await peer.request({
    op: "send", to: participant.agent_id, wake: true,
    text: "Release the call-alone wait for the fixture.",
  });
  const waitResponse = await wait.promise;
  assert.equal(waitResponse.error, undefined, waitResponse.error?.message);
  const waited = JSON.parse(waitResponse.result.content[0].text);
  await mcp.tool("mustr_ack", { message_id: waited.message.id }).then(resultValue);

  const rejectedByHub = await mcp.tool("mustr_cancel_owner_tasks", {
    owner_id: participant.agent_id, scope: "all_unfinished", expected_guard: guard,
    text: "User redirected this work.",
  });
  assert.equal(rejectedByHub.isError, true);
  assert.match(rejectedByHub.content[0].text, /current admitted manager/);
  assert.deepEqual(
    (await f.members(peer)).find(member => member.id === participant.agent_id).task_queue,
    self.task_queue,
    "adapter preserves current identity, scope, and guard while the hub enforces manager authority",
  );
});
