import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { access, mkdir, mkdtemp, readFile, readdir, rm, stat, symlink, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { binary, exec } from "./support.mjs";

test("install selects an explicit Pi target and touches only the requested temporary directory", async t => {
  const directory = await mkdtemp("/tmp/mustr-install-test-");
  t.after(() => rm(directory, { recursive: true, force: true }));
  const config = `${directory}/config`, agent = `${directory}/agent`, envAgent = `${directory}/env-agent`;
  const env = { ...process.env, MUSTR_CONFIG_DIR: config, MUSTR_STATE_DIR: `${directory}/state`, PI_CODING_AGENT_DIR: envAgent };

  const help = (await exec(binary, ["install", "--help"], { env })).stdout;
  assert.match(help, /<TARGET>/);
  assert.match(help, /pi\s+Install the Pi adapter/);
  const piHelp = (await exec(binary, ["install", "pi", "--help"], { env })).stdout.replace(/\s+/g, " ");
  assert.match(piHelp, /--agent-dir/);
  assert.match(piHelp, /--overwrite-roles/);
  assert.match(piHelp, /all spaces sharing this config directory/);
  assert.match(piHelp, /active prompt snapshots require explicit refresh\/reselection\/rejoin/i);
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
  const contextWindowName = contents.match(/from "\.\/(context-window\.[a-f0-9]{24}\.ts)"/)?.[1];
  assert.ok(contextWindowName, "installed entry point uses an immutable context-window dependency");
  assert.deepEqual(await readFile(`${agent}/extensions/mustr/${contextWindowName}`), await readFile(new URL("../pi/context-window.ts", import.meta.url)));
  await assert.rejects(access(`${agent}/settings.json`), { code: "ENOENT" });
  await assert.rejects(access(envAgent), { code: "ENOENT" }); // explicit flag takes precedence
  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  assert.equal(await readFile(index, "utf8"), contents);

  await exec(binary, ["install", "pi"], { env });
  await access(`${envAgent}/extensions/mustr/index.ts`);
});

test("fresh setup, init and install seed the default crew only when the profile file is missing", async t => {
  const directory = await mkdtemp("/tmp/mustr-profile-install-");
  t.after(() => rm(directory, { recursive: true, force: true }));
  const config = `${directory}/config`, agent = `${directory}/agent`;
  const env = { ...process.env, MUSTR_CONFIG_DIR: config, MUSTR_STATE_DIR: `${directory}/state` };
  const path = `${config}/profiles.toml`;
  const bundled = await readFile(new URL("../profiles/default.toml", import.meta.url));
  // First ordinary setup seeds; later ordinary commands respect deliberate deletion.
  await exec(binary, ["roles", "--json"], { env });
  assert.deepEqual(await readFile(path), bundled);
  assert.equal((await stat(path)).mode & 0o777, 0o600);
  await rm(path);
  await exec(binary, ["roles", "--json"], { env });
  await assert.rejects(access(path), { code: "ENOENT" });
  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  assert.deepEqual(await readFile(path), bundled);
  await rm(path);
  await exec(binary, ["init"], { env });
  assert.deepEqual(await readFile(path), bundled);

  // Independently check these exact defaults against Pi's bundled, offline chat catalog.
  const catalog = JSON.parse(await readFile(new URL("../pi/node_modules/@earendil-works/pi-ai/dist/providers/data/openai-codex.json", import.meta.url), "utf8"));
  const models = Object.values(catalog).flatMap(group => Object.values(group));
  const configured = [...bundled.toString().matchAll(/^model = "([^"]+)"$/gm)];
  assert.equal(configured.length, 4);
  for (const [, value] of configured) {
    const separator = value.indexOf("/"), provider = value.slice(0, separator), id = value.slice(separator + 1);
    assert.ok(models.some(model => model.type === "chat" && model.provider === provider && model.id === id && model.reasoning && model.thinkingLevelMap.high === "high"), `${value} must support high thinking in Pi's catalog`);
  }

  // Do not parse/merge/repair user-owned profiles, even if temporarily invalid.
  const custom = Buffer.from("# custom profile work in progress\r\nnot valid TOML\n");
  await writeFile(path, custom);
  const before = await stat(path);
  for (const args of [["init"], ["roles", "--json"], ["install", "pi", "--agent-dir", agent], ["install", "pi", "--agent-dir", agent, "--overwrite-roles"]]) {
    await exec(binary, args, { env });
    assert.deepEqual(await readFile(path), custom);
    assert.equal((await stat(path)).ino, before.ino);
  }
});

test("Pi --overwrite-roles refreshes only shipped roles with exact backups and no-op repeats", async t => {
  const directory = await mkdtemp("/tmp/mustr-role-refresh-test-");
  t.after(() => rm(directory, { recursive: true, force: true }));
  const config = `${directory}/config`;
  const state = `${directory}/state`;
  const agent = `${directory}/agent`;
  const env = { ...process.env, MUSTR_CONFIG_DIR: config, MUSTR_STATE_DIR: state };
  await exec(binary, ["init"], { env });

  const roles = `${config}/roles`;
  const architect = `${roles}/architect.md`;
  const researcher = `${roles}/researcher.md`;
  const original = Buffer.from("customized shipped role\r\nwith exact bytes\n");
  await writeFile(architect, original);
  await rm(researcher);
  const customRole = `${roles}/customer-domain.md`;
  await writeFile(customRole, "custom role stays\n");
  const configPath = `${config}/config.toml`;
  const configBefore = `${await readFile(configPath, "utf8")}# preserved config sentinel\n`;
  await writeFile(configPath, configBefore);
  await mkdir(state, { recursive: true });
  const history = `${state}/history.sentinel`;
  await writeFile(history, "history stays\n");

  await exec(binary, ["install", "pi", "--agent-dir", agent], { env });
  assert.deepEqual(await readFile(architect), original);
  await assert.rejects(access(researcher), { code: "ENOENT" });
  await assert.rejects(access(`${config}/role-backups`), { code: "ENOENT" });

  const refreshed = await exec(binary, ["install", "pi", "--agent-dir", agent, "--overwrite-roles"], { env });
  const backupName = `architect.md.${createHash("sha256").update(original).digest("hex").slice(0, 24)}.bak`;
  const backup = `${config}/role-backups/${backupName}`;
  assert.match(refreshed.stdout, /Refreshed shipped roles/);
  assert.ok(refreshed.stdout.includes(architect));
  assert.ok(refreshed.stdout.includes(backup));
  assert.ok(!refreshed.stdout.includes(original.toString()));
  assert.deepEqual(await readFile(backup), original);
  assert.deepEqual(await readFile(architect), await readFile(new URL("../roles/architect.md", import.meta.url)));
  assert.deepEqual(await readFile(researcher), await readFile(new URL("../roles/researcher.md", import.meta.url)));
  assert.equal(await readFile(customRole, "utf8"), "custom role stays\n");
  assert.equal(await readFile(configPath, "utf8"), configBefore);
  assert.equal(await readFile(history, "utf8"), "history stays\n");
  assert.equal((await readdir(`${config}/role-backups`)).length, 1);

  const roleStat = await stat(architect);
  const repeated = await exec(binary, ["install", "pi", "--agent-dir", agent, "--overwrite-roles"], { env });
  const repeatedStat = await stat(architect);
  assert.match(repeated.stdout, /already match embedded defaults/);
  assert.equal(repeatedStat.ino, roleStat.ino);
  assert.equal((await readdir(`${config}/role-backups`)).length, 1);
});

test("Pi overwrite does not bypass installer guards or follow unsafe role destinations", async t => {
  const directory = await mkdtemp("/tmp/mustr-role-guard-test-");
  t.after(() => rm(directory, { recursive: true, force: true }));
  const config = `${directory}/config`;
  const env = { ...process.env, MUSTR_CONFIG_DIR: config, MUSTR_STATE_DIR: `${directory}/state` };
  await exec(binary, ["init"], { env });
  const architect = `${config}/roles/architect.md`;
  const original = Buffer.from("role edit must survive installer failure\n");
  await writeFile(architect, original);
  const agent = `${directory}/agent`;
  const unmanaged = `${agent}/extensions/mustr`;
  await mkdir(unmanaged, { recursive: true });
  await writeFile(`${unmanaged}/index.ts`, "user-owned extension\n");
  await assert.rejects(exec(binary, ["install", "pi", "--agent-dir", agent, "--overwrite-roles"], { env }));
  assert.deepEqual(await readFile(architect), original);
  await assert.rejects(access(`${config}/role-backups`), { code: "ENOENT" });
  assert.equal(await readFile(`${unmanaged}/index.ts`, "utf8"), "user-owned extension\n");

  const unsafeDirectory = `${directory}/unsafe`;
  const unsafeConfig = `${unsafeDirectory}/config`;
  const unsafeEnv = { ...process.env, MUSTR_CONFIG_DIR: unsafeConfig, MUSTR_STATE_DIR: `${unsafeDirectory}/state` };
  await exec(binary, ["init"], { env: unsafeEnv });
  const unsafeRole = `${unsafeConfig}/roles/architect.md`;
  await rm(unsafeRole);
  const external = `${unsafeDirectory}/outside.md`;
  await writeFile(external, "outside sentinel\n");
  await symlink(external, unsafeRole);
  const unsafeFailure = await exec(binary, ["install", "pi", "--agent-dir", `${unsafeDirectory}/agent`, "--overwrite-roles"], { env: unsafeEnv }).then(
    () => assert.fail("symlink role destination must fail closed"),
    error => error,
  );
  assert.match(unsafeFailure.stderr, /installer was not run/);
  assert.equal(await readFile(external, "utf8"), "outside sentinel\n");
  await assert.rejects(access(`${unsafeDirectory}/agent`), { code: "ENOENT" });
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
  const contextWindow = upgraded.match(/from "\.\/(context-window\.[a-f0-9]{24}\.ts)"/)?.[1];
  assert.ok(contextWindow);
  assert.deepEqual(await readFile(`${base}/${contextWindow}`), await readFile(new URL("../pi/context-window.ts", import.meta.url)));
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
