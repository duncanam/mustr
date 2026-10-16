import test from "node:test";
import assert from "node:assert/strict";
import { access, mkdtemp, readFile, rm } from "node:fs/promises";
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
  await access(`${agent}/extensions/mustr/transport.mjs`);
  await assert.rejects(access(`${agent}/settings.json`), { code: "ENOENT" });
  await assert.rejects(access(envAgent), { code: "ENOENT" }); // explicit flag takes precedence
  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  assert.equal(await readFile(index, "utf8"), contents);

  await exec(binary, ["install", "pi"], { env });
  await access(`${envAgent}/extensions/mustr/index.ts`);
});
