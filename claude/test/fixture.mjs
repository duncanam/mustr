import assert from "node:assert/strict";
import { execFile as execFileCallback, spawn } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { once } from "node:events";
import net from "node:net";
import { fileURLToPath, pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import { HubClient } from "../../pi/transport.mjs";

const execFile = promisify(execFileCallback);
const root = fileURLToPath(new URL("../../", import.meta.url));
export const binary = `${root}/target/debug/mustr`;
export const adapterEntry = fileURLToPath(new URL("../index.mjs", import.meta.url));
const preload = fileURLToPath(new URL("./slow-offer-preload.mjs", import.meta.url));
let build;

export async function ensureBinary() {
  build ??= (async () => {
    await execFile("cargo", ["build", "--manifest-path", `${root}/Cargo.toml`, "--locked"], { cwd: root, timeout: 180_000, maxBuffer: 4 * 1024 * 1024 });
    assert.ok(existsSync(binary), "cargo build produced the mustr executable");
  })();
  await build;
}

export const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
export async function eventually(check, timeoutMs = 8000, note = "condition") {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    try { const result = await check(); if (result) return result; }
    catch (error) { lastError = error; }
    await pause(30);
  }
  throw lastError ?? new Error(`Timed out waiting for ${note}`);
}

function herdrServer(socketPath) {
  const panes = new Map([
    ["w1:p0", { pane_id: "w1:p0", workspace_id: "w1", agent: null }],
    ["w1:p1", { pane_id: "w1:p1", workspace_id: "w1", agent: "claude" }],
    ["w1:p2", { pane_id: "w1:p2", workspace_id: "w1", agent: "claude" }],
  ]);
  const connections = new Set();
  const server = net.createServer(socket => {
    connections.add(socket); socket.on("close", () => connections.delete(socket)); socket.on("error", () => {});
    let buffer = "";
    socket.on("data", chunk => {
      buffer += chunk;
      while (buffer.includes("\n")) {
        const end = buffer.indexOf("\n");
        const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
        let request;
        try { request = JSON.parse(line); }
        catch { socket.end(JSON.stringify({ id: null, error: "bad request" }) + "\n"); return; }
        let result;
        if (request.method === "pane.current") result = { pane: panes.get(request.params?.caller_pane_id) };
        else if (request.method === "workspace.list") result = { workspaces: [{ workspace_id: "w1", label: "Claude fixture" }] };
        else { socket.end(JSON.stringify({ id: request.id, error: `unexpected method ${request.method}` }) + "\n"); return; }
        socket.end(JSON.stringify({ id: request.id, result }) + "\n");
      }
    });
  });
  return { server, panes, connections };
}

export class McpProcess {
  constructor(child) {
    this.child = child;
    this.buffer = "";
    this.pending = new Map();
    this.messages = [];
    this.stderr = "";
    this.nextId = 1;
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", chunk => {
      this.buffer += chunk;
      while (this.buffer.includes("\n")) {
        const end = this.buffer.indexOf("\n");
        const line = this.buffer.slice(0, end); this.buffer = this.buffer.slice(end + 1);
        let message;
        try { message = JSON.parse(line); }
        catch (error) { this.messages.push({ parseError: error.message, raw: line }); continue; }
        this.messages.push(message);
        const key = message.id === null ? "null" : `${typeof message.id}:${message.id}`;
        const pending = this.pending.get(key);
        if (pending) { clearTimeout(pending.timer); this.pending.delete(key); pending.resolve(message); }
      }
    });
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", chunk => { this.stderr += chunk; });
    child.on("exit", (code, signal) => {
      const error = new Error(`MCP process exited before response (code=${code}, signal=${signal}): ${this.stderr}`);
      for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(error); }
      this.pending.clear();
    });
  }
  send(method, params = {}, id = this.nextId++, timeoutMs = 45_000) {
    const promise = new Promise((resolve, reject) => {
      const key = `${typeof id}:${id}`;
      const timer = setTimeout(() => { this.pending.delete(key); reject(new Error(`Timed out waiting for ${method}`)); }, timeoutMs);
      this.pending.set(key, { resolve, reject, timer });
      const wire = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
      this.child.stdin.write(wire, error => { if (error) { clearTimeout(timer); this.pending.delete(key); reject(error); } });
    });
    return { id, promise };
  }
  notify(method, params = {}) { this.child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n"); }
  raw(line) { this.child.stdin.write(`${line}\n`); }
  async request(method, params = {}, timeoutMs = 45_000) {
    const { promise } = this.send(method, params, this.nextId++, timeoutMs);
    const response = await promise;
    if (response.error) throw new Error(response.error.message);
    return response.result;
  }
  async tool(name, args = {}, timeoutMs = 45_000) {
    const result = await this.request("tools/call", { name, arguments: args }, timeoutMs);
    const text = result.content?.[0]?.text ?? "";
    return { ...result, value: (() => { try { return JSON.parse(text); } catch { return text; } })() };
  }
  async close() {
    if (this.child.exitCode !== null || this.child.signalCode !== null) return;
    this.child.stdin.end();
    await Promise.race([once(this.child, "exit"), pause(2500).then(() => { this.child.kill("SIGKILL"); })]);
  }
}

export async function fixture(t, { delayOfferMs = 0 } = {}) {
  await ensureBinary();
  const directory = await mkdtemp("/tmp/mustr-claude-");
  const herdrPath = `${directory}/herdr.sock`;
  let herdr;
  let discovery;
  let hubOutput = "";
  let closed = false;
  const processes = new Set();
  const clients = new Set();
  const stopChild = async child => {
    if (!child || child.exitCode !== null || child.signalCode !== null) return;
    const exited = once(child, "exit"); child.kill("SIGTERM");
    await Promise.race([exited, pause(3000).then(() => { if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL"); })]);
  };
  async function close() {
    if (closed) return;
    closed = true;
    for (const client of clients) {
      try { if (client instanceof HubClient) client.close(); else await client.close(); } catch {}
    }
    for (const child of processes) { try { await stopChild(child); } catch {} }
    for (const socket of herdr?.connections ?? []) socket.destroy();
    if (herdr?.server.listening) await new Promise(resolve => herdr.server.close(resolve));
    if (discovery?.socket) {
      await rm(discovery.socket, { force: true });
      await rm(discovery.socket.replace(/\.sock$/, ".lock"), { force: true });
    }
    await rm(directory, { recursive: true, force: true });
  }
  t.after(close);
  herdr = herdrServer(herdrPath);
  herdr.server.listen(herdrPath);
  await once(herdr.server, "listening");
  const env = {
    ...process.env,
    HERDR_ENV: "1", HERDR_SOCKET_PATH: herdrPath, HERDR_PANE_ID: "w1:p0",
    MUSTR_CONFIG_DIR: `${directory}/config`, MUSTR_STATE_DIR: `${directory}/state`,
    MUSTR_BIN: binary, NO_COLOR: "1",
  };
  await execFile(binary, ["init"], { env, timeout: 8000 });
  discovery = JSON.parse((await execFile(binary, ["context"], { env, timeout: 8000 })).stdout);
  const hub = spawn(binary, ["run", "--headless"], { env, stdio: ["ignore", "pipe", "pipe"] });
  processes.add(hub);
  hub.stdout.on("data", chunk => { hubOutput += chunk; });
  hub.stderr.on("data", chunk => { hubOutput += chunk; });
  const waitHub = await eventually(async () => {
    if (hub.exitCode !== null) throw new Error(`mustr hub exited: ${hubOutput}`);
    return await new Promise(resolve => {
      const socket = net.createConnection(discovery.socket);
      socket.once("connect", () => { socket.destroy(); resolve(true); });
      socket.once("error", () => resolve(false));
    });
  }, 8000, "mustr hub socket");
  assert.ok(waitHub);

  function startAdapter(extraEnv = {}) {
    const nodeOptions = [process.env.NODE_OPTIONS, "--import", pathToFileURL(preload).href].filter(Boolean).join(" ");
    const child = spawn(process.execPath, [adapterEntry], {
      env: { ...env, HERDR_PANE_ID: "w1:p1", NODE_OPTIONS: nodeOptions, MUSTR_TEST_DELAY_OFFER_MS: String(delayOfferMs), ...extraEnv },
      stdio: ["pipe", "pipe", "pipe"],
    });
    processes.add(child);
    const mcp = new McpProcess(child);
    clients.add(mcp);
    const exited = once(child, "exit").then(([code, signal]) => ({ code, signal }));
    return { mcp, exited };
  }
  async function observer({ id = `observer-${randomUUID().slice(0, 8)}`, pane = "w1:p2", role = "reviewer" } = {}) {
    const peer = new HubClient(); clients.add(peer); peer.agentId = id;
    await peer.connect(discovery.socket, { agent_id: id, name: id, role, pane_id: pane, space_id: "w1" });
    await peer.request({ op: "sync", activity: "working", receive: false });
    return peer;
  }
  async function members(peer) { return (await peer.request({ op: "sync", activity: "working", receive: false })).members; }
  return { directory, env, discovery, hub, get hubOutput() { return hubOutput; }, startAdapter, observer, members, close };
}
