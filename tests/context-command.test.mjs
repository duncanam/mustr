import test from "node:test";
import assert from "node:assert/strict";
import { chmod, mkdir, writeFile, unlink } from "node:fs/promises";
import { join } from "node:path";
import { spawn } from "node:child_process";
import net from "node:net";
import { binary, exec, fixture } from "./support.mjs";

function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

async function fakeHerdr(socketPath, onRequest) {
  const connections = new Set();
  const errors = [];
  const server = net.createServer(connection => {
    connections.add(connection);
    connection.on("close", () => connections.delete(connection));
    connection.on("error", () => {});
    let buffer = "";
    connection.on("data", chunk => {
      buffer += chunk.toString();
      while (buffer.includes("\n")) {
        const end = buffer.indexOf("\n"), line = buffer.slice(0, end);
        buffer = buffer.slice(end + 1);
        try { onRequest(connection, JSON.parse(line)); }
        catch (error) { errors.push(error); connection.destroy(); }
      }
    });
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, () => { server.off("error", reject); resolve(); });
  });
  return {
    errors,
    async close() {
      for (const connection of connections) connection.destroy();
      if (server.listening) await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
      assert.deepEqual(errors, [], "fake-herdr protocol errors");
    },
  };
}

function paneResult(request) {
  return { id: request.id, result: { pane: { pane_id: "w1:p0", workspace_id: "w1", agent: "pi" } } };
}

function runContext(f, env) {
  const child = spawn(binary, ["context"], { env, stdio: ["ignore", "pipe", "pipe"] });
  f.track(child);
  let stdout = "", stderr = "";
  child.stdout.setEncoding("utf8"); child.stdout.on("data", chunk => stdout += chunk);
  child.stderr.setEncoding("utf8"); child.stderr.on("data", chunk => stderr += chunk);
  const closed = new Promise(resolve => child.once("close", (code, signal) => resolve({ code, signal })));
  return { child, closed, output: () => ({ stdout, stderr }) };
}

async function assertNon75(env, expectedStderr) {
  let failure;
  try { await exec(binary, ["context"], { env, timeout: 8000 }); }
  catch (error) { failure = error; }
  assert.ok(failure, "mustr context must fail for this nontransient condition");
  assert.notEqual(failure.code, 75, "only typed HerdrUnavailable uses the reserved status");
  assert.equal(typeof failure.code, "number");
  assert.equal(failure.stdout, "", "failed context discovery does not write JSON to stdout");
  assert.match(String(failure.stderr), expectedStderr);
}

test("mustr context keeps successful discovery JSON unchanged", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const result = await exec(binary, ["context"], { env: f.env });
  assert.deepEqual(JSON.parse(result.stdout), f.discovery);
  assert.equal(result.stderr, "");
});

test("profile context waits for positive Pi recognition without changing discovery JSON", { timeout: 8000 }, async t => {
  const f = await fixture(t);
  const socketPath = join(f.directory, "starting-pi.sock");
  let calls = 0;
  const fake = await fakeHerdr(socketPath, (connection, request) => {
    const reply = paneResult(request);
    if (++calls <= 2) reply.result.pane.agent = null;
    connection.end(`${JSON.stringify(reply)}\n`);
  });
  try {
    const { stdout, stderr } = await exec(binary, ["context", "--wait-for-pi"], { env: { ...f.env, HERDR_SOCKET_PATH: socketPath }, timeout: 6000 });
    const context = JSON.parse(stdout);
    assert.equal(context.pane_id, "w1:p0");
    assert.equal(context.space_id, "w1");
    assert.deepEqual(Object.keys(context).sort(), Object.keys(f.discovery).sort());
    assert.ok(calls >= 3);
    assert.equal(stderr, "");
  } finally { await fake.close(); }
});

test("profile context refuses changed pane, space, or a different recognized agent", { timeout: 8000 }, async t => {
  const f = await fixture(t);
  for (const change of [{ pane_id: "w1:replacement" }, { workspace_id: "w2" }, { agent: "codex" }]) {
    const socketPath = join(f.directory, `changed-${Object.keys(change)[0]}.sock`);
    let calls = 0;
    const fake = await fakeHerdr(socketPath, (connection, request) => {
      const reply = paneResult(request);
      reply.result.pane.agent = null;
      if (++calls > 1) Object.assign(reply.result.pane, change);
      connection.end(`${JSON.stringify(reply)}\n`);
    });
    try {
      await assert.rejects(
        exec(binary, ["context", "--wait-for-pi"], { env: { ...f.env, HERDR_SOCKET_PATH: socketPath }, timeout: 6000 }),
        error => error.code === 1 && error.stdout === "" && /pane identity changed|another agent/.test(error.stderr),
      );
    } finally { await fake.close(); }
  }
});

test("profile context bounds the wait when Pi is never recognized", { timeout: 8000 }, async t => {
  const f = await fixture(t);
  const socketPath = join(f.directory, "unrecognized-pi.sock");
  const fake = await fakeHerdr(socketPath, (connection, request) => {
    const reply = paneResult(request);
    reply.result.pane.agent = null;
    connection.end(`${JSON.stringify(reply)}\n`);
  });
  try {
    await assert.rejects(
      exec(binary, ["context", "--wait-for-pi"], { env: { ...f.env, HERDR_SOCKET_PATH: socketPath }, timeout: 6000 }),
      error => error.code === 1 && error.stdout === "" && /Pi was not recognized.*4 seconds/.test(error.stderr),
    );
  } finally { await fake.close(); }
});

test("mustr context maps a held pane.current reply to concise reserved exit 75", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const socketPath = join(f.directory, "withheld-herdr.sock");
  const seen = deferred();
  const fake = await fakeHerdr(socketPath, () => seen.resolve());
  try {
    const running = runContext(f, { ...f.env, HERDR_SOCKET_PATH: socketPath });
    await seen.promise;
    const { code, signal } = await running.closed;
    const { stdout, stderr } = running.output();
    assert.equal(code, 75);
    assert.equal(signal, null);
    assert.equal(stdout, "");
    assert.match(stderr, /herdr temporarily unavailable/);
    assert.ok(stderr.trim().length < 200, `typed transient diagnostic should be concise: ${stderr}`);
    assert.doesNotMatch(stderr, /Backtrace:|Location:|SpanTrace|RUST_BACKTRACE/i);
  } finally { await fake.close(); }
});

test("mustr context keeps malformed JSON and topology failures nontransient", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const cases = [
    ["malformed-json", connection => connection.end("{malformed JSON}\n"), /herdr|JSON|expected|key/i],
    ["malformed-pane", (connection, request) => connection.end(`${JSON.stringify({ id: request.id, result: { pane: { pane_id: "w1:p0", agent: "pi" } } })}\n`), /invalid herdr pane|workspace_id/i],
  ];
  for (const [name, respond, expected] of cases) {
    const socketPath = join(f.directory, `${name}.sock`);
    const fake = await fakeHerdr(socketPath, (connection, request) => respond(connection, request));
    try { await assertNon75({ ...f.env, HERDR_SOCKET_PATH: socketPath }, expected); }
    finally { await fake.close(); }
  }
});

test("mustr context treats environment, socket, and configuration errors as actionable non-75 failures", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const { HERDR_ENV: _herdrEnv, ...withoutHerdrEnv } = f.env;
  await assertNon75(withoutHerdrEnv, /run mustr inside a herdr-managed pane/);
  const { HERDR_SOCKET_PATH: _socket, ...withoutSocket } = f.env;
  await assertNon75(withoutSocket, /HERDR_SOCKET_PATH is missing/);
  const { HERDR_PANE_ID: _pane, ...withoutPane } = f.env;
  await assertNon75(withoutPane, /HERDR_PANE_ID is missing/);
  await assertNon75({ ...f.env, HERDR_SOCKET_PATH: join(f.directory, "missing.sock") }, /herdr socket is unavailable/);
  await writeFile(join(f.directory, "config", "config.toml"), "version = [\n");
  await assertNon75(f.env, /parsing .*config\.toml|TOML|expected/i);
  const configFile = join(f.directory, "not-a-config-directory");
  await writeFile(configFile, "not a directory");
  await assertNon75({ ...f.env, MUSTR_CONFIG_DIR: configFile }, /creating .*not-a-config-directory|expected a real directory/i);
});

test("mustr context keeps filesystem permission failures nontransient", { timeout: 12000 }, async t => {
  if (process.getuid?.() === 0) return t.skip("permission-denied metadata checks require a non-root test process");
  const f = await fixture(t);
  const denied = join(f.directory, "denied");
  await mkdir(denied, { mode: 0o700 });
  await chmod(denied, 0);
  try { await assertNon75({ ...f.env, HERDR_SOCKET_PATH: join(denied, "herdr.sock") }, /herdr socket is unavailable|Permission denied/i); }
  finally { await chmod(denied, 0o700); }
});

test("mustr context rejects a replaced herdr socket even when the old request replies validly", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const socketPath = join(f.directory, "replace-herdr.sock");
  const seen = deferred();
  let oldConnection, oldRequest;
  const original = await fakeHerdr(socketPath, (connection, request) => { oldConnection = connection; oldRequest = request; seen.resolve(); });
  let replacement;
  try {
    const running = runContext(f, { ...f.env, HERDR_SOCKET_PATH: socketPath });
    await seen.promise;
    await unlink(socketPath);
    replacement = await fakeHerdr(socketPath, () => {});
    oldConnection.end(`${JSON.stringify(paneResult(oldRequest))}\n`);
    const { code, signal } = await running.closed;
    const { stdout, stderr } = running.output();
    assert.equal(code, 1);
    assert.equal(signal, null);
    assert.equal(stdout, "");
    assert.match(stderr, /herdr server changed|server disappeared/);
    assert.doesNotMatch(stderr, /herdr temporarily unavailable/);
  } finally {
    if (replacement) await replacement.close();
    await original.close();
  }
});
