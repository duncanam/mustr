import net from "node:net";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import { basename, dirname, join } from "node:path";
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
    ["w2:p0", { pane_id: "w2:p0", workspace_id: "w2", agent: null }],
    ["w2:p1", { pane_id: "w2:p1", workspace_id: "w2", agent: "pi" }],
    ["w2:p2", { pane_id: "w2:p2", workspace_id: "w2", agent: "pi" }],
  ]);
  const spaces = new Map([
    ["w1", { workspace_id: "w1", label: "First" }],
    ["w2", { workspace_id: "w2", label: "Second" }],
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
      else if (request.method === "workspace.list") result = { workspaces: [...spaces.values()] };
      else { connection.end(JSON.stringify({ id: request.id, error: "unexpected method" }) + "\n"); return; }
      connection.end(JSON.stringify({ id: request.id, result }) + "\n");
    });
  });
  server.listen(socket); await once(server, "listening");
  const env = { ...process.env, HERDR_ENV: "1", HERDR_SOCKET_PATH: socket, HERDR_PANE_ID: "w1:p0", MUSTR_CONFIG_DIR: `${directory}/config`, MUSTR_STATE_DIR: `${directory}/state`, NO_COLOR: "1" };
  const children = new Set();
  const runtimeSockets = new Set();
  const beforeCleanup = [];
  t.after(async () => {
    const errors = [];
    const attempt = async operation => {
      try { await operation(); } catch (error) { errors.push(error); }
    };
    for (const cleanup of beforeCleanup.reverse()) await attempt(cleanup);
    for (const child of children) await attempt(() => stop(child));
    for (const connection of connections) connection.destroy();
    await attempt(() => new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve())));
    for (const runtimeSocket of runtimeSockets) {
      await attempt(() => rm(runtimeSocket, { force: true }));
      await attempt(() => rm(runtimeSocket.replace(/\.sock$/, ".lock"), { force: true }));
    }
    const legacySocket = join(dirname(discovery.socket), `${discovery.server_key}.sock`);
    await attempt(() => rm(legacySocket, { force: true }));
    await attempt(() => rm(legacySocket.replace(/\.sock$/, ".lock"), { force: true }));
    await attempt(() => rm(directory, { recursive: true, force: true }));
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) throw new AggregateError(errors, "Fixture cleanup failed");
  });
  await exec(binary, ["init"], { env });
  await writeFile(`${directory}/config/config.toml`, "version = 1\n[scheduling]\ncontinuation_seconds = 1\ncheckin_seconds = 5\nmax_nudges = 2\n");
  const discover = async (paneId = "w1:p0") => {
    const paneEnv = { ...env, HERDR_PANE_ID: paneId };
    const discovery = JSON.parse((await exec(binary, ["context"], { env: paneEnv })).stdout);
    runtimeSockets.add(discovery.socket);
    return discovery;
  };
  const discovery = await discover();
  return {
    directory, env, discovery, panes, spaces, discover,
    beforeCleanup(callback) { beforeCleanup.push(callback); },
    database(identity = discovery) {
      const instanceKey = basename(identity.socket, ".sock");
      return `${env.MUSTR_STATE_DIR}/${identity.server_key}/${instanceKey}/hub.sqlite`;
    },
    async start({ paneId = "w1:p0", headless = true } = {}) {
      const identity = await discover(paneId);
      const paneEnv = { ...env, HERDR_PANE_ID: paneId };
      const child = spawn(binary, ["-v", "run", ...(headless ? ["--headless"] : [])], { env: paneEnv, stdio: ["pipe", "pipe", "pipe"] });
      child.output = "";
      child.discovery = identity;
      child.stdout.on("data", b => child.output += b); child.stderr.on("data", b => child.output += b);
      children.add(child);
      await eventually(async () => {
        if (child.exitCode !== null) throw new Error(child.output);
        return new Promise(resolve => { const c = net.createConnection(identity.socket); c.once("connect", () => { c.destroy(); resolve(true); }); c.once("error", () => resolve(false)); });
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
