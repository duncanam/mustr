import test from "node:test";
import assert from "node:assert/strict";
import { access, mkdtemp, readFile, rm, writeFile, readdir } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { binary, exec } from "./support.mjs";

test("install selects an explicit harness and touches only the requested temporary Pi directory", async t => {
  const directory = await mkdtemp("/tmp/mustr-install-test-");
  t.after(() => rm(directory, { recursive: true, force: true }));
  const config = `${directory}/config`, agent = `${directory}/agent`, envAgent = `${directory}/env-agent`;
  const env = { ...process.env, MUSTR_CONFIG_DIR: config, MUSTR_STATE_DIR: `${directory}/state`, PI_CODING_AGENT_DIR: envAgent };

  const help = (await exec(binary, ["install", "--help"], { env })).stdout;
  assert.match(help, /<HARNESS>/);
  assert.match(help, /pi\s+Install the Pi adapter/);
  assert.match((await exec(binary, ["install", "pi", "--help"], { env })).stdout, /--agent-dir/);
  await assert.rejects(exec(binary, ["install"], { env }));
  await assert.rejects(exec(binary, ["install", "unknown"], { env }));
  await assert.rejects(access(config), { code: "ENOENT" }); // help/errors do not initialize or install

  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  const index = `${agent}/extensions/mustr/index.ts`;
  const contents = await readFile(index, "utf8");
  assert.match(contents, /mustr-managed adapter v1/);
  assert.match(contents, /mustr install pi/);
  assert.ok(contents.includes(config));
  assert.ok(!contents.includes("__MUSTR_BINARY__"));
  const transportName = contents.match(/from "\.\/(transport\.[a-f0-9]{24}\.mjs)"/)?.[1];
  assert.ok(transportName, "installed entry point uses an immutable native module URL");
  await access(`${agent}/extensions/mustr/${transportName}`);
  await assert.rejects(access(`${agent}/settings.json`), { code: "ENOENT" });
  await assert.rejects(access(envAgent), { code: "ENOENT" }); // explicit flag takes precedence
  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  assert.equal(await readFile(index, "utf8"), contents);

  await exec(binary, ["install", "pi"], { env });
  await access(`${envAgent}/extensions/mustr/index.ts`);
});

test("upgrade escapes the native import cache and preserves the old entry point/transport", async t => {
  const directory = await mkdtemp("/tmp/mustr-upgrade-test-");
  t.after(() => rm(directory, { recursive: true, force: true }));
  const agent = `${directory}/agent`, env = { ...process.env, MUSTR_CONFIG_DIR: `${directory}/config`, MUSTR_STATE_DIR: `${directory}/state` };
  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  const base = `${agent}/extensions/mustr`;
  const oldIndex = '// mustr-managed adapter v1\nimport "./transport.mjs";\n';
  const oldTransport = 'export const generation = "old";\n';
  await writeFile(`${base}/index.ts`, oldIndex);
  await writeFile(`${base}/transport.mjs`, oldTransport);
  const cached = await import(pathToFileURL(`${base}/transport.mjs`).href);
  assert.equal(cached.generation, "old");
  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  const upgraded = await readFile(`${base}/index.ts`, "utf8");
  const transport = upgraded.match(/from "\.\/(transport\.[a-f0-9]{24}\.mjs)"/)?.[1];
  assert.ok(transport);
  const fresh = await import(pathToFileURL(`${base}/${transport}`).href);
  assert.equal(typeof fresh.insertionEvidence, "function");
  assert.equal(await readFile(`${base}/transport.mjs`, "utf8"), oldTransport);
  const backups = (await readdir(base)).filter(name => name.endsWith(".bak"));
  assert.ok(await Promise.all(backups.map(name => readFile(`${base}/${name}`, "utf8"))).then(values => values.includes(oldIndex)));
  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  assert.equal(await readFile(`${base}/index.ts`, "utf8"), upgraded);
  // A corrupt file at the immutable URL is an error, not permission to overwrite.
  await writeFile(`${base}/${transport}`, "unexpected contents");
  await assert.rejects(exec(binary, ["install", "pi", "--agent-dir", agent], { env }), /immutable transport/);
  assert.equal(await readFile(`${base}/index.ts`, "utf8"), upgraded);
  assert.equal(await readFile(`${base}/${transport}`, "utf8"), "unexpected contents");
});
