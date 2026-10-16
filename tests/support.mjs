import net from "node:net";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { once } from "node:events";

export const exec = promisify(execFile);
export const binary = fileURLToPath(new URL("../target/debug/mustr", import.meta.url));
export const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
export async function eventually(action, ms = 8000) {
  const deadline = Date.now() + ms;
  let error;
  while (Date.now() < deadline) { try { const value = await action(); if (value) return value; } catch (e) { error = e; } await delay(50); }
  throw error ?? new Error("Condition did not become true");
}

export async function fixture(t) {
  const directory = await mkdtemp("/tmp/mustr-test-");
  const socket = `${directory}/herdr.sock`;
  const panes = new Map([
    ["w1:p0", { pane_id: "w1:p0", workspace_id: "w1", agent: null }],
    ["w1:p1", { pane_id: "w1:p1", workspace_id: "w1", agent: "pi" }],
    ["w1:p2", { pane_id: "w1:p2", workspace_id: "w1", agent: "pi" }],
    ["w2:p1", { pane_id: "w2:p1", workspace_id: "w2", agent: "pi" }],
  ]);
  const connections = new Set();
  const server = net.createServer(connection => {
    connections.add(connection); connection.on("close", () => connections.delete(connection));
    connection.on("error", () => {});
    let buffer = "";
    connection.on("data", chunk => {
      buffer += chunk;
      if (!buffer.includes("\n")) return;
      const request = JSON.parse(buffer.split("\n")[0]);
      let result;
      if (request.method === "pane.current") result = { pane: panes.get(request.params.caller_pane_id) };
      else if (request.method === "workspace.list") result = { workspaces: [{ workspace_id: "w1", label: "First" }, { workspace_id: "w2", label: "Second" }] };
      else { connection.end(JSON.stringify({ id: request.id, error: "unexpected method" }) + "\n"); return; }
      connection.end(JSON.stringify({ id: request.id, result }) + "\n");
    });
  });
  server.listen(socket); await once(server, "listening");
  const env = { ...process.env, HERDR_ENV: "1", HERDR_SOCKET_PATH: socket, HERDR_PANE_ID: "w1:p0", MUSTR_CONFIG_DIR: `${directory}/config`, MUSTR_STATE_DIR: `${directory}/state`, NO_COLOR: "1" };
  const children = new Set();
  let runtimeSocket;
  t.after(async () => {
    for (const child of children) await stop(child);
    for (const connection of connections) connection.destroy();
    await new Promise(resolve => server.close(resolve));
    if (runtimeSocket) {
      await rm(runtimeSocket, { force: true });
      await rm(runtimeSocket.replace(/\.sock$/, ".lock"), { force: true });
    }
    await rm(directory, { recursive: true, force: true });
  });
  await exec(binary, ["init"], { env });
  await writeFile(`${directory}/config/config.toml`, "version = 1\n[scheduling]\ncontinuation_seconds = 1\ncheckin_seconds = 5\nmax_nudges = 2\n");
  const discovery = JSON.parse((await exec(binary, ["context"], { env })).stdout);
  runtimeSocket = discovery.socket;
  return {
    directory, env, discovery, panes,
    async start(headless = true) {
      const child = spawn(binary, ["-v", "run", ...(headless ? ["--headless"] : [])], { env, stdio: ["pipe", "pipe", "pipe"] });
      child.output = "";
      child.stdout.on("data", b => child.output += b); child.stderr.on("data", b => child.output += b);
      children.add(child);
      await eventually(async () => {
        if (child.exitCode !== null) throw new Error(child.output);
        return new Promise(resolve => { const c = net.createConnection(discovery.socket); c.once("connect", () => { c.destroy(); resolve(true); }); c.once("error", () => resolve(false)); });
      });
      return child;
    },
    track(child) { children.add(child); },
  };
}

export async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit"); child.kill("SIGTERM");
  await Promise.race([exited, delay(3000).then(() => { if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL"); })]);
}
