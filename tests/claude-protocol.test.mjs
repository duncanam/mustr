import test from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// MCP lifecycle and tools contracts:
// https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle
// https://modelcontextprotocol.io/specification/2025-11-25/server/tools
// Exercise the adapter process directly; never launch Claude or join a mustr hub.
const adapterEntry = fileURLToPath(new URL("../claude/index.mjs", import.meta.url));
const adapterProtocolVersion = "2024-11-05";
const requestTimeoutMs = 2_000;
const testTimeoutMs = 15_000;
const frameLimitBytes = 256 * 1024;

const pause = ms => new Promise(resolve => setTimeout(resolve, ms));

class McpProcess {
  constructor(child) {
    this.child = child;
    this.buffer = "";
    this.messages = [];
    this.waiters = [];
    this.stderr = "";
    this.exitInfo = null;
    this.exitPromise = new Promise(resolve => {
      child.once("exit", (code, signal) => {
        this.exitInfo = { code, signal };
        const error = new Error(`MCP adapter exited (${signal ?? code}) before replying; stderr: ${this.stderr}`);
        for (const waiter of this.waiters.splice(0)) {
          clearTimeout(waiter.timer);
          waiter.reject(error);
        }
        resolve(this.exitInfo);
      });
    });

    child.stdout.setEncoding("utf8");
    child.stdout.on("data", chunk => {
      this.buffer += chunk;
      while (true) {
        const newline = this.buffer.indexOf("\n");
        if (newline < 0) break;
        const line = this.buffer.slice(0, newline);
        this.buffer = this.buffer.slice(newline + 1);
        let message;
        try { message = JSON.parse(line); }
        catch (error) { message = { parseError: error.message, raw: line }; }
        const waiter = this.waiters.shift();
        if (waiter) {
          clearTimeout(waiter.timer);
          waiter.resolve(message);
        } else {
          this.messages.push(message);
        }
      }
    });
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", chunk => { this.stderr += chunk; });
    child.on("error", error => {
      for (const waiter of this.waiters.splice(0)) {
        clearTimeout(waiter.timer);
        waiter.reject(error);
      }
    });
  }

  async writeRaw(data) {
    if (this.exitInfo) throw new Error(`MCP adapter already exited (${this.exitInfo.signal ?? this.exitInfo.code})`);
    await new Promise((resolve, reject) => {
      try {
        this.child.stdin.write(data, error => error ? reject(error) : resolve());
      } catch (error) { reject(error); }
    });
  }

  async writeMessage(message) {
    await this.writeRaw(`${JSON.stringify(message)}\n`);
  }

  async request(message) {
    await this.writeMessage(message);
    const response = await this.nextMessage();
    assert.equal(response.id, message.id, `response id for ${message.method}`);
    return response;
  }

  async notify(method, params) {
    const message = { jsonrpc: "2.0", method };
    if (params !== undefined) message.params = params;
    await this.writeMessage(message);
  }

  nextMessage(timeoutMs = requestTimeoutMs) {
    if (this.messages.length > 0) return Promise.resolve(this.messages.shift());
    if (this.exitInfo) {
      return Promise.reject(new Error(`MCP adapter exited (${this.exitInfo.signal ?? this.exitInfo.code}) without a response; stderr: ${this.stderr}`));
    }
    return new Promise((resolve, reject) => {
      const waiter = { resolve, reject, timer: undefined };
      waiter.timer = setTimeout(() => {
        const index = this.waiters.indexOf(waiter);
        if (index >= 0) this.waiters.splice(index, 1);
        reject(new Error(`Timed out after ${timeoutMs} ms waiting for MCP response; stderr: ${this.stderr}`));
      }, timeoutMs);
      this.waiters.push(waiter);
    });
  }

  async waitForExit(timeoutMs) {
    if (this.exitInfo) return this.exitInfo;
    return Promise.race([this.exitPromise, pause(timeoutMs).then(() => null)]);
  }

  async stop() {
    if (!this.exitInfo) {
      this.child.stdin.end();
      if (!await this.waitForExit(500)) {
        this.child.kill("SIGTERM");
        if (!await this.waitForExit(1_000)) {
          this.child.kill("SIGKILL");
          if (!await this.waitForExit(1_000)) throw new Error("MCP adapter process survived SIGKILL");
        }
      }
    }
  }
}

async function startAdapter(t) {
  const root = await mkdtemp(join(tmpdir(), "mustr-claude-protocol-"));
  const home = join(root, "home");
  const config = join(root, "config");
  const cwd = join(root, "work");
  await Promise.all([mkdir(home), mkdir(config), mkdir(cwd)]);

  // Deliberately do not inherit HERDR, mustr, Claude auth, user HOME, or Node options.
  const env = {
    HOME: home,
    CLAUDE_CONFIG_DIR: config,
    TMPDIR: root,
    PATH: "/usr/bin:/bin",
  };
  const child = spawn(process.execPath, [adapterEntry], {
    cwd,
    env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  const mcp = new McpProcess(child);
  t.after(async () => {
    try { await mcp.stop(); }
    finally { await rm(root, { recursive: true, force: true }); }
  });
  return mcp;
}

function initializeMessage(id = 1, params = {}) {
  return {
    jsonrpc: "2.0",
    id,
    method: "initialize",
    params: {
      protocolVersion: adapterProtocolVersion,
      capabilities: {},
      clientInfo: { name: "protocol-test", version: "1.0.0" },
      ...params,
    },
  };
}

async function initialize(mcp, id = 1) {
  const response = await mcp.request(initializeMessage(id));
  assert.equal(response.error, undefined);
  assert.equal(response.result.protocolVersion, adapterProtocolVersion);
  await mcp.notify("notifications/initialized");
  return response.result;
}

function assertRpcError(response, code) {
  assert.ok(response.error, "expected a JSON-RPC error, not a tool result");
  assert.equal(response.error.code, code);
  assert.equal(response.result, undefined);
}

test("MCP lifecycle gates normal requests until initialized, exempts ping, and lists no-argument tools", { timeout: testTimeoutMs }, async t => {
  const mcp = await startAdapter(t);

  // MCP ping is a health check and must be answered even before initialization.
  const preInitPing = await mcp.request({ jsonrpc: "2.0", id: 1, method: "ping" });
  assert.deepEqual(preInitPing.result, {});

  const earlyCall = await mcp.request({
    jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "mustr_roster" },
  });
  assertRpcError(earlyCall, -32002);

  const init = await mcp.request(initializeMessage(3));
  assert.equal(init.result.protocolVersion, adapterProtocolVersion);
  assert.equal(init.result.serverInfo.name, "mustr");
  assert.ok(init.result.capabilities.tools);

  // Ping remains available after the initialize response but before the
  // client's notifications/initialized handshake completion.
  const handshakePing = await mcp.request({ jsonrpc: "2.0", id: 4, method: "ping" });
  assert.deepEqual(handshakePing.result, {});

  const beforeInitialized = await mcp.request({ jsonrpc: "2.0", id: 5, method: "tools/list" });
  assertRpcError(beforeInitialized, -32002);

  // notifications/initialized has no response. If it emits one, it will be read
  // before the tools/list response and fail the request-ID assertion below.
  await mcp.notify("notifications/initialized");
  const listed = await mcp.request({ jsonrpc: "2.0", id: 6, method: "tools/list" });
  assert.equal(listed.error, undefined);
  assert.ok(Array.isArray(listed.result.tools));
  const byName = new Map(listed.result.tools.map(tool => [tool.name, tool]));
  for (const name of ["mustr_join", "mustr_wait", "mustr_ack", "mustr_roster", "mustr_assign", "mustr_cancel", "mustr_leave"]) {
    assert.ok(byName.has(name), `tools/list includes ${name}`);
    assert.equal(byName.get(name).inputSchema.type, "object");
  }
  for (const name of ["mustr_roster", "mustr_leave"]) {
    assert.deepEqual(byName.get(name).inputSchema.properties, {});
    assert.equal(byName.get(name).inputSchema.additionalProperties, false);
  }
  assert.match(byName.get("mustr_assign").description, /per-owner FIFO/i);
  assert.match(byName.get("mustr_assign").description, /not executed until the hub dispatches/i);
  assert.match(byName.get("mustr_cancel").description, /does NOT stop running tools or external jobs/i);
});

test("initialize requires well-formed parameters and cannot be repeated", { timeout: testTimeoutMs }, async t => {
  const mcp = await startAdapter(t);
  const validClientInfo = { name: "protocol-test", version: "1.0.0" };
  const invalidInitializeRequests = [
    { jsonrpc: "2.0", id: 1, method: "initialize" },
    { jsonrpc: "2.0", id: 2, method: "initialize", params: null },
    { jsonrpc: "2.0", id: 3, method: "initialize", params: { protocolVersion: adapterProtocolVersion, capabilities: {} } },
    { jsonrpc: "2.0", id: 4, method: "initialize", params: { protocolVersion: " ", capabilities: {}, clientInfo: validClientInfo } },
    { jsonrpc: "2.0", id: 5, method: "initialize", params: { protocolVersion: adapterProtocolVersion, capabilities: [], clientInfo: validClientInfo } },
    { jsonrpc: "2.0", id: 6, method: "initialize", params: { protocolVersion: adapterProtocolVersion, capabilities: {}, clientInfo: { name: "protocol-test" } } },
  ];
  for (const request of invalidInitializeRequests) {
    const response = await mcp.request(request);
    assertRpcError(response, -32602);
  }

  const accepted = await mcp.request(initializeMessage(7));
  assert.equal(accepted.result.protocolVersion, adapterProtocolVersion);
  const duplicate = await mcp.request(initializeMessage(8));
  assertRpcError(duplicate, -32600);

  await mcp.notify("notifications/initialized");
  const ping = await mcp.request({ jsonrpc: "2.0", id: 9, method: "ping" });
  assert.deepEqual(ping.result, {});
});

test("tools/call accepts omitted arguments and _meta, separating protocol errors from operation errors", { timeout: testTimeoutMs }, async t => {
  const mcp = await startAdapter(t);
  await initialize(mcp);

  const leave = await mcp.request({
    jsonrpc: "2.0",
    id: 2,
    method: "tools/call",
    params: { name: "mustr_leave", _meta: { progressToken: 7 } },
  });
  assert.equal(leave.error, undefined);
  assert.equal(leave.result.isError, false);
  assert.deepEqual(JSON.parse(leave.result.content[0].text), { left: true, joined: false });

  const operationError = await mcp.request({
    jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "mustr_roster" },
  });
  assert.equal(operationError.error, undefined);
  assert.equal(operationError.result.isError, true);
  assert.match(operationError.result.content[0].text, /not joined|hub disconnected/i);

  const unknownTool = await mcp.request({
    jsonrpc: "2.0", id: 4, method: "tools/call", params: { name: "mustr_not_a_tool" },
  });
  assertRpcError(unknownTool, -32602);
  assert.match(unknownTool.error.message, /unknown tool/i);

  const unknownMethod = await mcp.request({ jsonrpc: "2.0", id: 5, method: "mustr/not-a-method" });
  assertRpcError(unknownMethod, -32601);
});

test("stdio parses malformed, array, oversized, and fragmented frames without joining mustr", { timeout: testTimeoutMs }, async t => {
  const mcp = await startAdapter(t);

  await mcp.writeRaw('{"jsonrpc":"2.0","id":1\n');
  const parseError = await mcp.nextMessage();
  assert.equal(parseError.id, null);
  assertRpcError(parseError, -32700);

  await mcp.writeRaw("[]\n");
  const invalidRequest = await mcp.nextMessage();
  assert.equal(invalidRequest.id, null);
  assertRpcError(invalidRequest, -32600);

  // The input limit is bytes per newline-delimited frame. Split the oversized
  // frame across writes so the implementation must safely discard through LF.
  const oversized = Buffer.alloc(frameLimitBytes + 1, 0x61);
  await mcp.writeRaw(oversized);
  await mcp.writeRaw(Buffer.from("\n"));
  const frameError = await mcp.nextMessage(5_000);
  assert.equal(frameError.id, null);
  assertRpcError(frameError, -32700);
  assert.match(frameError.error.message, /frame exceeds 256 KiB/i);

  const wire = Buffer.from(`${JSON.stringify(initializeMessage(2))}\n`);
  await mcp.writeRaw(wire.subarray(0, 7));
  await mcp.writeRaw(wire.subarray(7));
  const init = await mcp.nextMessage();
  assert.equal(init.id, 2);
  assert.equal(init.result.protocolVersion, adapterProtocolVersion);
  await mcp.notify("notifications/initialized");
  const ping = await mcp.request({ jsonrpc: "2.0", id: 3, method: "ping" });
  assert.deepEqual(ping.result, {});
});

test("MCP notifications do not receive responses", { timeout: testTimeoutMs }, async t => {
  const mcp = await startAdapter(t);
  await initialize(mcp);

  // Unknown request IDs are ignored for cancellation; notifications never get
  // a JSON-RPC response (https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation).
  await mcp.notify("notifications/cancelled", { requestId: "not-in-flight", reason: "protocol test" });
  await mcp.notify("notifications/test-no-response", { value: "ignored" });
  const ping = await mcp.request({ jsonrpc: "2.0", id: 2, method: "ping" });
  assert.deepEqual(ping.result, {});
});
