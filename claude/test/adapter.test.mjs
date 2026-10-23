import test from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import net from "node:net";
import { isAbsolute } from "node:path";
import { fixture, eventually, pause } from "./fixture.mjs";

async function initialize(mcp) {
  const initialized = await mcp.request("initialize", {
    protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "mustr-claude-test", version: "1" },
  });
  assert.equal(initialized.protocolVersion, "2024-11-05");
  assert.equal(initialized.serverInfo.name, "mustr");
  mcp.notify("notifications/initialized");
  await mcp.request("ping");
}
async function setup(t, options = {}) {
  const f = await fixture(t, options);
  const { mcp } = f.startAdapter(options.adapterEnv);
  await initialize(mcp);
  return { f, mcp };
}
function resultValue(response) {
  assert.equal(response.isError, false, response.content?.[0]?.text);
  return JSON.parse(response.content[0].text);
}
async function toolPromise(promise) {
  const response = await promise;
  assert.equal(response.error, undefined, response.error?.message);
  return resultValue(response.result);
}
async function joined(mcp, role = "implementor") {
  const response = await mcp.tool("mustr_join", { role });
  const value = resultValue(response);
  assert.equal(value.joined, true);
  assert.equal(value.role, role);
  assert.match(value.agent_id, /^[0-9a-f-]{36}$/);
  assert.ok(value.prompt.length > 0, "resolved role prompt is returned to Claude");
  return value;
}
async function waitUntilIdle(f, peer, agentId, timeout = 5000) {
  return eventually(async () => {
    const members = await f.members(peer);
    return members.find(member => member.id === agentId && member.activity === "idle");
  }, timeout, "adapter to advertise idle only inside mustr_wait");
}
async function delayedMustrBinary(f, delayMs = 1200) {
  const wrapper = `${f.directory}/delayed-mustr-${delayMs}.mjs`;
  await writeFile(wrapper, `#!${process.execPath}\nimport { spawn } from 'node:child_process';\nconst args=process.argv.slice(2);\nif(args.includes('context')) await new Promise(resolve=>setTimeout(resolve,${delayMs}));\nconst child=spawn(${JSON.stringify(f.env.MUSTR_BIN)}, args, {stdio:'inherit', env:process.env});\nprocess.on('SIGTERM',()=>child.kill('SIGTERM'));\nchild.on('exit',(code,signal)=>process.exit(code??(signal?1:0)));\n`, { mode: 0o700 });
  return wrapper;
}

// These tests exercise the source adapter against the real Rust hub and a fake
// herdr socket. They use no Claude account, model, plugin hooks, or user config.
test("join is explicit, resolves the role, and keeps ordinary activity working", { timeout: 60000 }, async t => {
  const { f, mcp } = await setup(t);
  const notJoined = await mcp.tool("mustr_roster");
  assert.equal(notJoined.isError, true);
  assert.match(notJoined.content[0].text, /Not joined/);

  const badRole = await mcp.tool("mustr_join", { role: "missing-role" });
  assert.equal(badRole.isError, true);
  assert.match(badRole.content[0].text, /missing-role/);
  const peer = await f.observer();
  const before = await f.members(peer);
  assert.equal(before.some(member => member.role === "implementor"), false, "failed role resolution never admitted a participant");

  const joinedValue = await joined(mcp);
  const members = await f.members(peer);
  const self = members.find(member => member.id === joinedValue.agent_id);
  assert.equal(self.activity, "working", "join never begins idle");
  await pause(1200);
  assert.equal((await f.members(peer)).find(member => member.id === joinedValue.agent_id).activity, "working", "no-MCP activity remains working");
  const duplicate = await mcp.tool("mustr_join", { role: "implementor" });
  assert.equal(duplicate.isError, true);
  assert.match(duplicate.content[0].text, /Already joined/);
});

test("default participant names hash full Claude identity and leave explicit names unchanged", { timeout: 60000 }, async t => {
  const f = await fixture(t);
  const longRole = "custom-role-with-a-very-long-name-for-testing";
  await writeFile(`${f.env.MUSTR_CONFIG_DIR}/roles/${longRole}.md`, "Long custom role fixture.\n");
  const firstId = "12345678-1111-4111-8111-111111111111";
  const secondId = "12345678-2222-4222-8222-222222222222";
  const { mcp: first } = f.startAdapter({ HERDR_PANE_ID: "w1:p1", MUSTR_TEST_AGENT_ID: firstId });
  const { mcp: second } = f.startAdapter({ HERDR_PANE_ID: "w1:p2", MUSTR_TEST_AGENT_ID: secondId });
  await initialize(first); await initialize(second);

  const firstJoin = await joined(first, "researcher");
  const secondJoin = await joined(second, "researcher");
  assert.notEqual(firstJoin.name, secondJoin.name, "different full identities sharing eight leading characters get distinct defaults");
  for (const value of [firstJoin, secondJoin]) {
    assert.match(value.name, /^[a-z][a-z0-9-]{0,47}$/);
    assert.ok(value.name.length <= 48);
  }
  const roster = resultValue(await first.tool("mustr_roster"));
  const peers = roster.members.filter(member => member.role === "researcher");
  assert.deepEqual(peers.map(member => [member.id, member.name, member.pane_id]).sort(), [
    [firstId, firstJoin.name, "w1:p1"], [secondId, secondJoin.name, "w1:p2"],
  ]);

  await resultValue(await second.tool("mustr_leave"));
  const collision = await second.tool("mustr_join", { role: "researcher", name: firstJoin.name });
  assert.equal(collision.isError, true);
  assert.match(collision.content[0].text, /name already in use in this space/);
  const secondAgain = await joined(second, "researcher");
  assert.equal(secondAgain.name, secondJoin.name, "a rejected explicit-name collision is not auto-retried or renamed");

  await resultValue(await first.tool("mustr_leave"));
  const repeated = await joined(first, "researcher");
  assert.equal(repeated.name, firstJoin.name, "the same full process identity deterministically gets the same default");
  await resultValue(await first.tool("mustr_leave"));
  const explicit = resultValue(await first.tool("mustr_join", { role: "researcher", name: "explicitly-named" }));
  assert.equal(explicit.name, "explicitly-named", "a supplied name is preserved exactly");
  await resultValue(await first.tool("mustr_leave"));

  const long = resultValue(await first.tool("mustr_join", { role: longRole }));
  assert.equal(long.role, longRole);
  assert.match(long.name, /^[a-z][a-z0-9-]{0,47}$/);
  assert.ok(long.name.length <= 48);
  assert.ok(long.name.startsWith(`${longRole.slice(0, 12).replace(/-+$/, "")}-`));
});

test("authoritative connection retirement closes Claude's client without Leave or automatic rejoin", { timeout: 60000 }, async t => {
  const f = await fixture(t);
  const requestLog = `${f.directory}/retired-requests.log`;
  const { mcp } = f.startAdapter({ MUSTR_TEST_RETIRE_OP: "send", MUSTR_TEST_REQUEST_LOG: requestLog });
  await initialize(mcp);
  const peer = await f.observer();
  const first = await joined(mcp);
  const before = await readFile(requestLog, "utf8").catch(() => "");

  const retired = await mcp.tool("mustr_send", { to: "peer", text: "This request is rejected before execution", wake: false });
  assert.equal(retired.isError, true);
  assert.match(retired.content[0].text, /retired or superseded this connection/);
  assert.match(retired.content[0].text, /explicitly use mustr_join/);
  const after = await readFile(requestLog, "utf8");
  assert.equal(after.slice(before.length).trim().split("\n").filter(Boolean).filter(op => op === "send").length, 1);
  assert.equal(after.slice(before.length).includes("leave\n"), false, "retirement cleanup sends no unverified Leave");

  const noLongerJoined = await mcp.tool("mustr_roster");
  assert.equal(noLongerJoined.isError, true);
  assert.match(noLongerJoined.content[0].text, /Not joined/);
  await eventually(async () => (await f.members(peer)).find(member => member.id === first.agent_id)?.activity === "offline");
  await pause(1200);
  assert.equal(await readFile(requestLog, "utf8"), after, "no heartbeat or automatic rejoin follows retirement");

  const explicit = await joined(mcp);
  assert.equal(explicit.agent_id, first.agent_id, "the existing session identity is reused only after explicit Join and admission");
});

test("authoritative connection retirement in Claude's heartbeat disconnects without retry or rejoin", { timeout: 60000 }, async t => {
  const f = await fixture(t);
  const requestLog = `${f.directory}/retired-heartbeat-requests.log`;
  const { mcp } = f.startAdapter({
    MUSTR_TEST_RETIRE_OP: "sync", MUSTR_TEST_RETIRE_AFTER: "2", MUSTR_TEST_REQUEST_LOG: requestLog,
  });
  await initialize(mcp);
  const peer = await f.observer();
  const first = await joined(mcp);
  await eventually(() => mcp.stderr.includes("retired or superseded this connection"));
  assert.equal((mcp.stderr.match(/retired or superseded this connection/g) ?? []).length, 1, "one actionable heartbeat notice is emitted");
  await eventually(async () => (await f.members(peer)).find(member => member.id === first.agent_id)?.activity === "offline");
  const after = await readFile(requestLog, "utf8");
  const noLongerJoined = await mcp.tool("mustr_roster");
  assert.equal(noLongerJoined.isError, true);
  assert.match(noLongerJoined.content[0].text, /Not joined/);
  await pause(1200);
  assert.equal(await readFile(requestLog, "utf8"), after, "the heartbeat retirement does not reconnect or send follow-up work");
});

test("Claude Join advertises its absolute checkout without shell or model discovery", { timeout: 60000 }, async t => {
  const f = await fixture(t);
  const socketPath = `${f.directory}/join-capture.sock`;
  let resolveJoin;
  const seen = new Promise(resolve => { resolveJoin = resolve; });
  const server = net.createServer(socket => {
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("data", chunk => {
      buffer += chunk;
      while (buffer.includes("\n")) {
        const end = buffer.indexOf("\n");
        const request = JSON.parse(buffer.slice(0, end));
        buffer = buffer.slice(end + 1);
        if (request.op === "join") resolveJoin(request);
        const result = request.op === "join"
          ? { protocol: 1, agent_id: request.agent_id, delivery_policy: 1, inbox_notifications: 1 }
          : request.op === "sync" ? { members: [], tasks: [], automation: {}, inbox: null } : {};
        socket.write(`${JSON.stringify({ id: request.id, ok: true, result })}\n`);
      }
    });
  });
  server.listen(socketPath);
  await new Promise(resolve => server.once("listening", resolve));
  t.after(() => new Promise(resolveClose => server.close(resolveClose)));
  const wrapper = `${f.directory}/mustr-join-capture.mjs`;
  await writeFile(wrapper, `#!${process.execPath}\nconst args=process.argv.slice(2);\nif(args.includes("roles")) process.stdout.write(JSON.stringify([{name:"implementor",description:"fixture role",prompt:"fixture prompt"}]));\nelse if(args.includes("context")) process.stdout.write(JSON.stringify({socket:process.env.FAKE_HUB_SOCKET,server_key:"fixture",pane_id:"w1:p1",space_id:"w1"}));\nelse process.exit(2);\n`, { mode: 0o700 });
  const { mcp } = f.startAdapter({ MUSTR_BIN: wrapper, FAKE_HUB_SOCKET: socketPath });
  await initialize(mcp);
  const join = resultValue(await mcp.tool("mustr_join", { role: "implementor" }));
  assert.equal(join.joined, true);
  const request = await seen;
  assert.ok(isAbsolute(request.cwd));
  assert.equal(request.cwd, process.cwd());
  await mcp.tool("mustr_leave").then(resultValue);
});

test("wait heartbeats while idle and cancellation restores working before later heartbeats", { timeout: 60000 }, async t => {
  const { f, mcp } = await setup(t);
  const peer = await f.observer();
  const joinedValue = await joined(mcp);
  const request = mcp.send("tools/call", { name: "mustr_wait", arguments: { timeout_seconds: 8 } });
  const idle = await waitUntilIdle(f, peer, joinedValue.agent_id);
  const lastSeen = idle.last_seen;
  await pause(2200);
  const refreshed = (await f.members(peer)).find(member => member.id === joinedValue.agent_id);
  assert.equal(refreshed.activity, "idle");
  assert.ok(refreshed.last_seen > lastSeen, "wait activity heartbeat continues during the blocking MCP call");

  mcp.notify("notifications/cancelled", { requestId: request.id, reason: "fixture cancellation" });
  const cancelled = await request.promise;
  assert.equal(cancelled.error?.code, -32800);
  const working = await eventually(async () => {
    const member = (await f.members(peer)).find(candidate => candidate.id === joinedValue.agent_id);
    return member?.activity === "working" ? member : undefined;
  }, 4000, "working activity restored after cancellation");
  await pause(1300);
  const later = (await f.members(peer)).find(member => member.id === joinedValue.agent_id);
  assert.equal(later.activity, "working", "no post-cancel heartbeat re-advertises idle");
  assert.ok(later.last_seen >= working.last_seen);
  const roster = resultValue(await mcp.tool("mustr_roster"));
  assert.equal(roster.members.find(member => member.id === joinedValue.agent_id).activity, "working");
});

test("cancelling after a hub offer disconnects uncertain and never replays it", { timeout: 60000 }, async t => {
  const { f, mcp } = await setup(t, { delayOfferMs: 1000 });
  const peer = await f.observer();
  const joinedValue = await joined(mcp);
  const wait = mcp.send("tools/call", { name: "mustr_wait", arguments: { timeout_seconds: 20 } });
  await waitUntilIdle(f, peer, joinedValue.agent_id);
  const assignment = await peer.request({ op: "assign", to: joinedValue.agent_id, title: "Cancellation receipt", text: "Do not execute; this fixture tests an unacknowledged offer." });
  // The preload delays the response only after the real hub has changed this
  // message to Offered, making this cancellation race deterministic.
  await pause(700);
  mcp.notify("notifications/cancelled", { requestId: wait.id, reason: "cancel after offer" });
  const response = await wait.promise;
  assert.equal(response.error?.code, -32800);
  const offline = await eventually(async () => {
    const members = await f.members(peer);
    const member = members.find(value => value.id === joinedValue.agent_id);
    return member?.activity === "offline" ? member : undefined;
  }, 5000, "connection retired after offered result was cancelled");
  assert.equal(offline.activity, "offline");
  let tasks = (await f.members(peer));
  assert.equal(tasks.find(member => member.id === joinedValue.agent_id).activity, "offline");

  const again = await joined(mcp);
  assert.equal(again.agent_id, joinedValue.agent_id, "an explicit join may reuse this process identity");
  const empty = resultValue(await mcp.tool("mustr_wait", { timeout_seconds: 0 }));
  assert.equal(empty.message, null, "uncertain offers are not replayed on explicit rejoin");
  assert.equal(assignment.task_id.length > 0, true);
});

test("pause preserves an outstanding offer without receipt and permits roster/leave", { timeout: 60000 }, async t => {
  const { f, mcp } = await setup(t);
  const peer = await f.observer();
  const joinedValue = await joined(mcp);
  const wait = mcp.send("tools/call", { name: "mustr_wait", arguments: { timeout_seconds: 10 } });
  await waitUntilIdle(f, peer, joinedValue.agent_id);
  const assignment = await peer.request({ op: "assign", to: joinedValue.agent_id, title: "Pause before receipt", text: "This assignment must remain unreceived." });
  const offer = await toolPromise(wait.promise);
  assert.equal(offer.message.kind, "assignment");
  assert.equal(offer.acknowledged, false);

  const paused = resultValue(await mcp.tool("mustr_pause", { reason: "human decision required" }));
  assert.equal(paused.paused, true);
  const state = await f.members(peer);
  assert.equal(state.find(member => member.id === joinedValue.agent_id).activity, "paused");
  assert.equal(state.find(member => member.id === joinedValue.agent_id).id, joinedValue.agent_id);
  const ack = await mcp.tool("mustr_ack", { message_id: offer.message.id });
  assert.equal(ack.isError, true, "local pause blocks receipt mutation");
  const waitAgain = await mcp.tool("mustr_wait", { timeout_seconds: 0 });
  assert.equal(waitAgain.isError, true, "local pause blocks wait");
  const roster = resultValue(await mcp.tool("mustr_roster"));
  assert.equal(roster.tasks.find(task => task.id === assignment.task_id).state, "assigned", "pause did not acknowledge/activate the assignment");
  assert.equal(resultValue(await mcp.tool("mustr_leave")).left, true);
});

test("queued assignment reaches Claude only after an explicit wait and dispatch", { timeout: 60000 }, async t => {
  const { f, mcp } = await setup(t);
  const peer = await f.observer();
  const joinedValue = await joined(mcp);

  const assignment = await peer.request({
    op: "assign",
    to: joinedValue.agent_id,
    title: "Wait for dispatch",
    text: "Do not begin before the assignment message is returned by mustr_wait.",
  });
  assert.equal(assignment.state, "queued");
  assert.equal(assignment.queue_position, 1);
  const queued = resultValue(await mcp.tool("mustr_roster"));
  assert.equal(queued.tasks.find(task => task.id === assignment.task_id).state, "queued");
  assert.equal(queued.tasks.find(task => task.id === assignment.task_id).queue_position, 1);
  assert.equal(queued.members.find(member => member.id === joinedValue.agent_id).task_queue.queued_count, 1);

  const wait = mcp.send("tools/call", { name: "mustr_wait", arguments: { timeout_seconds: 10 } });
  await waitUntilIdle(f, peer, joinedValue.agent_id);
  const offer = await toolPromise(wait.promise);
  assert.equal(offer.message.kind, "assignment");
  assert.equal(offer.message.task_id, assignment.task_id);
  assert.equal(offer.acknowledged, false);
  assert.equal(resultValue(await mcp.tool("mustr_ack", { message_id: offer.message.id })).acknowledged, true);
  const active = resultValue(await mcp.tool("mustr_roster"));
  assert.equal(active.tasks.find(task => task.id === assignment.task_id).state, "active");
});

test("mixed task exchange acknowledges only after wait, supports guide and terminal report", { timeout: 60000 }, async t => {
  const { f, mcp } = await setup(t);
  const peer = await f.observer();
  const joinedValue = await joined(mcp);

  const sending = resultValue(await mcp.tool("mustr_send", { to: peer.agentId, text: "Use the addressed participant ID, not role name.", wake: false }));
  assert.equal(sending.delivery, "queued");
  let peerInbox = await peer.request({ op: "sync", activity: "working", receive: true });
  assert.equal(peerInbox.inbox.text, "Use the addressed participant ID, not role name.");
  await peer.request({ op: "ack", message_id: peerInbox.inbox.id });

  const waiting = mcp.send("tools/call", { name: "mustr_wait", arguments: { timeout_seconds: 10 } });
  await waitUntilIdle(f, peer, joinedValue.agent_id);
  const assignment = await peer.request({ op: "assign", to: joinedValue.agent_id, title: "Verify fixture exchange", text: "Report blocked, then follow explicit guidance." });
  const assignmentOffer = await toolPromise(waiting.promise);
  assert.equal(assignmentOffer.message.task_id, assignment.task_id);
  assert.equal(assignmentOffer.message.kind, "assignment");
  const repeat = await mcp.tool("mustr_wait", { timeout_seconds: 0 });
  assert.equal(repeat.isError, true, "outstanding offer is not reinjected");
  assert.match(repeat.content[0].text, /Acknowledge/);
  const unknownAck = await mcp.tool("mustr_ack", { message_id: "not-the-offer" });
  assert.equal(unknownAck.isError, true);
  assert.match(unknownAck.content[0].text, /Unknown message ID/);
  assert.equal(resultValue(await mcp.tool("mustr_ack", { message_id: assignmentOffer.message.id })).acknowledged, true);

  const blocked = resultValue(await mcp.tool("mustr_report", { task_id: assignment.task_id, revision: 1, kind: "blocked", text: "Need one explicit detail." }));
  assert.equal(blocked.declared_state, "blocked");
  peerInbox = await peer.request({ op: "sync", activity: "working", receive: true });
  assert.equal(peerInbox.inbox.kind, "blocked");
  await peer.request({ op: "ack", message_id: peerInbox.inbox.id });
  await peer.request({ op: "guide", task_id: assignment.task_id, revision: 1, text: "The fixture label is the expected value." });

  const guidedWait = mcp.send("tools/call", { name: "mustr_wait", arguments: { timeout_seconds: 5 } });
  const guidance = await toolPromise(guidedWait.promise);
  assert.equal(guidance.message.kind, "guide");
  assert.equal(guidance.message.revision, 2);
  await mcp.tool("mustr_ack", { message_id: guidance.message.id }).then(resultValue);
  const done = resultValue(await mcp.tool("mustr_report", { task_id: assignment.task_id, revision: 2, kind: "done", text: "Verified the fixture behavior with a real hub." }));
  assert.equal(done.declared_state, "done");
  assert.match(done.notice, /cannot block Claude/);
  peerInbox = await peer.request({ op: "sync", activity: "working", receive: true });
  assert.equal(peerInbox.inbox.kind, "done");
  await peer.request({ op: "ack", message_id: peerInbox.inbox.id });
  await peer.request({ op: "accept", task_id: assignment.task_id, revision: 2, text: "Independent evidence reviewed." });
  const roster = resultValue(await mcp.tool("mustr_roster"));
  assert.equal(roster.tasks.find(task => task.id === assignment.task_id).state, "accepted");

  await peer.request({ op: "sync", activity: "idle", receive: false });
  const cancellable = resultValue(await mcp.tool("mustr_assign", { to: peer.agentId, title: "Cancel fixture task", text: "Cancel this bookkeeping task immediately." }));
  const cancelled = resultValue(await mcp.tool("mustr_cancel", { task_id: cancellable.task_id, revision: 1, text: "No longer needed." }));
  assert.equal(cancelled.state, "cancelled");
});

test("cancelling delayed role/context discovery aborts before hub admission", { timeout: 60000 }, async t => {
  const f = await fixture(t);
  const peer = await f.observer();
  const wrapper = await delayedMustrBinary(f, 1200);
  const { mcp } = f.startAdapter({ MUSTR_BIN: wrapper });
  await initialize(mcp);
  const join = mcp.send("tools/call", { name: "mustr_join", arguments: { role: "implementor" } });
  await pause(100);
  mcp.notify("notifications/cancelled", { requestId: join.id, reason: "cancel explicit join" });
  const response = await join.promise;
  assert.equal(response.error?.code, -32800);
  assert.equal((await f.members(peer)).some(member => member.role === "implementor" && member.activity !== "offline"), false);

  const explicit = resultValue(await mcp.tool("mustr_join", { role: "implementor" }));
  assert.equal(explicit.joined, true, "only an explicit subsequent join may admit the participant");
  await mcp.tool("mustr_leave").then(resultValue);
});

test("EOF during delayed role/context discovery aborts children and cannot create a join", { timeout: 60000 }, async t => {
  const { f } = await setup(t);
  const wrapper = `${f.directory}/delayed-mustr.mjs`;
  await writeFile(wrapper, `#!${process.execPath}\nimport { spawn } from 'node:child_process';\nconst args=process.argv.slice(2);\nif(args.includes('context')) await new Promise(resolve=>setTimeout(resolve,1200));\nconst child=spawn(${JSON.stringify(f.env.MUSTR_BIN)}, args, {stdio:'inherit', env:process.env});\nprocess.on('SIGTERM',()=>child.kill('SIGTERM'));\nchild.on('exit',(code,signal)=>process.exit(code??(signal?1:0)));\n`, { mode: 0o700 });
  const { mcp, exited } = f.startAdapter({ MUSTR_BIN: wrapper });
  const initialized = await mcp.request("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "EOF fixture", version: "1" } });
  assert.equal(initialized.serverInfo.name, "mustr");
  mcp.notify("notifications/initialized");
  const join = mcp.send("tools/call", { name: "mustr_join", arguments: { role: "implementor" } });
  const joinOutcome = join.promise.catch(() => undefined);
  await pause(100);
  mcp.child.stdin.end();
  await Promise.race([exited, pause(5000).then(() => { throw new Error("adapter did not exit after MCP stdin EOF"); })]);
  const peer = await f.observer();
  const members = await f.members(peer);
  assert.equal(members.some(member => member.role === "implementor"), false, "EOF fences delayed role/context and prevents hub join");
  await joinOutcome;
  const runtimeSocket = f.discovery.socket;
  const runtimeLock = runtimeSocket.replace(/\.sock$/, ".lock");
  assert.equal(existsSync(runtimeSocket), true, "fixture hub owns its runtime socket before cleanup");
  assert.equal(existsSync(runtimeLock), true, "fixture hub owns its lock before cleanup");
  await f.close();
  assert.equal(existsSync(runtimeSocket), false, "fixture cleanup removes only its hub socket");
  assert.equal(existsSync(runtimeLock), false, "fixture cleanup removes only its hub lock");
});

test("maximal escaped role prompt fits bounded MCP output; failed wait output disconnects uncertain", { timeout: 90000 }, async t => {
  const { f, mcp } = await setup(t);
  const roles = `${f.directory}/config/roles`;
  const body = `Quoted \\\" text\\\\\n${'q"\\\n'.repeat(12000)}`;
  await writeFile(`${roles}/chunk-a.md`, body);
  await writeFile(`${roles}/chunk-b.md`, body);
  await writeFile(`${roles}/large.md`, '+++\ncompose = ["chunk-a", "chunk-b"]\n+++\nLarge composed role.');
  const large = resultValue(await mcp.tool("mustr_join", { role: "large" }));
  assert.ok(Buffer.byteLength(large.prompt) > 90_000);
  assert.ok(large.prompt.includes('q"\\\n'));
  assert.equal(large.joined, true, "valid max-sized prompt context reaches the caller within the bounded frame");
  await mcp.tool("mustr_leave").then(resultValue);

  // Simulate failure to write the wait response to MCP stdout. The actual hub
  // has already offered an assignment; the adapter must close rather than ACK.
  const failed = f.startAdapter({ MUSTR_TEST_FAIL_WAIT_OUTPUT: "1" });
  await initialize(failed.mcp);
  const secondJoin = resultValue(await failed.mcp.tool("mustr_join", { role: "implementor" }));
  const peer = await f.observer();
  const wait = failed.mcp.send("tools/call", { name: "mustr_wait", arguments: { timeout_seconds: 10 } });
  await waitUntilIdle(f, peer, secondJoin.agent_id);
  const assignment = await peer.request({ op: "assign", to: secondJoin.agent_id, title: "Unemitted result", text: "Must not be acknowledged when stdout fails." });
  const waitFailure = wait.promise.catch(error => error);
  const error = await waitFailure;
  assert.match(error.message, /exited before response/);
  await eventually(async () => {
    const member = (await f.members(peer)).find(value => value.id === secondJoin.agent_id);
    return member?.activity === "offline";
  }, 5000, "wait output failure disconnects the participant");
  const tasks = (await f.members(peer));
  assert.ok(tasks.some(member => member.id === secondJoin.agent_id && member.activity === "offline"));
  assert.ok(assignment.task_id);
});
