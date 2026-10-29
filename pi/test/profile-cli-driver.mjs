// Opt-in native ai-jail fixture. Runs the real Pi CLI offline, never a prompt.
import assert from "node:assert/strict";
import net from "node:net";
import { appendFile, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createInterface } from "node:readline";
import { HubClient } from "../transport.mjs";

const args = process.argv.slice(2);
const descriptorPath = args[args.indexOf("--mustr-launch") + 1];
const descriptor = JSON.parse(await readFile(descriptorPath, "utf8"));
await assert.rejects(appendFile(descriptorPath, " "), error => ["EPERM", "EACCES"].includes(error.code));
const isolation = JSON.parse(await readFile(join(dirname(descriptor.cwd), "isolation.json"), "utf8"));
for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "MUSTR_BIN", "MUSTR_STATE_DIR", "MUSTR_CONFIG_DIR", "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "NUCLEUS_AUTH_TOKEN"]) assert.equal(process.env[key], undefined, key);
for (const path of isolation.forbiddenFiles) await assert.rejects(readFile(path), error => ["EPERM", "EACCES", "ENOENT"].includes(error.code));
async function denied(destination) {
  await assert.rejects(new Promise((resolve, reject) => {
    const socket = net.createConnection(destination);
    socket.setTimeout(2000, () => { socket.destroy(); reject(new Error("isolation probe timed out")); });
    socket.once("connect", () => { socket.destroy(); resolve(); });
    socket.once("error", reject);
  }), error => ["EPERM", "EACCES", "ENOENT"].includes(error.code));
}
for (const path of isolation.forbiddenSockets) await denied(path);
await denied({ host: "127.0.0.1", port: isolation.unforwardedPort });
const hello = { agent_id: "native-probe", name: "probe", role: descriptor.role.name, pane_id: descriptor.pane_id, space_id: descriptor.space_id, cwd: descriptor.cwd };
const unauthenticated = new HubClient();
await assert.rejects(unauthenticated.connect({ port: descriptor.gateway.port, token: "0".repeat(64) }, hello));
unauthenticated.close();
const wrongScope = new HubClient();
await assert.rejects(wrongScope.connect(descriptor.gateway, { ...hello, pane_id: "unowned-pane" }));
wrongScope.close();

const cli = fileURLToPath(new URL("../node_modules/@earendil-works/pi-coding-agent/dist/cli.js", import.meta.url));
const child = spawn(process.execPath, [cli, "--offline", "--mode", "rpc", "--no-session", ...args], {
  stdio: ["pipe", "pipe", "pipe"], env: { ...process.env, PI_OFFLINE: "1" },
});
const exited = once(child, "close");
let stderr = "", state, failure, shutdownTimer, killer;
child.stderr.on("data", chunk => { stderr += chunk; });
const lines = createInterface({ input: child.stdout });
const stopOwnedChild = () => {
  child.kill("SIGTERM");
  killer = setTimeout(() => { if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL"); }, 1500);
};
const deadline = setTimeout(() => { failure = new Error("Offline Pi CLI fixture timed out"); stopOwnedChild(); }, 15000);
const polling = setInterval(() => {
  if (!child.stdin.destroyed && !child.stdin.writableEnded) child.stdin.write(`${JSON.stringify({ id: "state", type: "get_state" })}\n`);
}, 100);
child.stdin.on("error", error => { failure ??= error; });
try {
  for await (const line of lines) {
    let response;
    try { response = JSON.parse(line); } catch { continue; }
    if (response.type !== "response" || response.command !== "get_state" || state) continue;
    assert.equal(response.success, true);
    state = response.data;
    assert.equal(`${state.model.provider}/${state.model.id}`, descriptor.model);
    assert.equal(state.thinkingLevel, descriptor.thinking);
    assert.equal(state.messageCount, 0);
    clearInterval(polling);
    // The host independently requires an admitted gateway subscription. Let the
    // ordinary adapter pump run, then EOF invokes Pi's normal RPC shutdown.
    shutdownTimer = setTimeout(() => child.stdin.end(), 2000);
  }
  const [code, signal] = await exited;
  if (failure) throw failure;
  assert.equal(code, 0, `Pi CLI failed (${signal}): ${stderr}`);
  assert.ok(state, `Pi CLI returned no state: ${stderr}`);
  await writeFile(join(descriptor.cwd, "native-cli-result.json"), JSON.stringify({
    nativePiCli: true, model: descriptor.model, thinking: state.thinkingLevel,
    modelMessages: state.messageCount, isolated: true, cliPid: child.pid, exited: true,
  }));
} finally {
  clearTimeout(deadline); clearTimeout(shutdownTimer); clearInterval(polling); lines.close();
  if (child.exitCode === null && child.signalCode === null) stopOwnedChild();
  await exited;
  clearTimeout(killer);
}
