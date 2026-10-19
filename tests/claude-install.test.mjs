import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { access, chmod, mkdtemp, mkdir, readFile, realpath, rm, symlink, writeFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import { binary, exec, eventually, stop } from "./support.mjs";

const hash = bytes => createHash("sha256").update(bytes).digest("hex").slice(0, 24);

async function isolated(root, { fakeCli = true, override = true } = {}) {
  const home = `${root}/home`, workspace = `${root}/workspace`, claudeConfig = `${root}/profile`, fakeBin = `${root}/bin`;
  await Promise.all([mkdir(home), mkdir(workspace), mkdir(claudeConfig), mkdir(fakeBin)]);
  const calls = `${root}/claude-calls`;
  if (fakeCli) {
    const script = `#!/usr/bin/env node
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
const argv = process.argv.slice(2);
const scopeAt = argv.indexOf("--scope");
const scope = scopeAt < 0 ? "" : argv[scopeAt + 1];
const name = scopeAt < 0 ? "" : argv[scopeAt + 2];
appendFileSync(process.env.FAKE_CLAUDE_CALLS, "mcp add-json " + scope + " " + name + "\\n");
if (process.env.FAKE_CLAUDE_FAIL === "1") { process.stderr.write("simulated failure\\n"); process.exit(17); }
if (argv[0] !== "mcp" || argv[1] !== "add-json" || scope !== "user" || !name || !argv[scopeAt + 3]) process.exit(64);
const profileDir = process.env.CLAUDE_CONFIG_DIR || process.env.HOME;
const path = join(profileDir, ".claude.json");
mkdirSync(dirname(path), { recursive: true });
const profile = existsSync(path) ? JSON.parse(readFileSync(path, "utf8")) : {};
const servers = profile.mcpServers ?? {};
if (Object.hasOwn(servers, name)) { process.stderr.write("already exists\\n"); process.exit(1); }
profile.mcpServers = { ...servers, [name]: JSON.parse(argv[scopeAt + 3]) };
writeFileSync(path, JSON.stringify(profile, null, 2) + "\\n");
process.stdout.write("Added user MCP server\\n");
`;
    const path = `${fakeBin}/claude`;
    await writeFile(path, script);
    await chmod(path, 0o755);
  }
  const env = {
    ...process.env,
    HOME: home,
    PATH: `${fakeBin}:${process.env.PATH ?? ""}`,
    FAKE_CLAUDE_CALLS: calls,
    MUSTR_CONFIG_DIR: `${root}/mustr-config`,
    MUSTR_STATE_DIR: `${root}/mustr-state`,
  };
  if (override) env.CLAUDE_CONFIG_DIR = claudeConfig;
  else delete env.CLAUDE_CONFIG_DIR;
  return { home, workspace, claudeConfig, fakeBin, calls, env, options: { env, cwd: workspace } };
}

async function callCount(path) {
  try { return (await readFile(path, "utf8")).trim().split("\n").filter(Boolean).length; }
  catch (error) { if (error.code === "ENOENT") return 0; throw error; }
}

test("config-only Claude bundle is idempotent, isolated, and launchable from paths with spaces", async t => {
  const root = await mkdtemp("/tmp/mustr-claude-install-");
  t.after(() => rm(root, { recursive: true, force: true }));
  const directory = `${root}/quoted ' directory`;
  const home = `${root}/home`, workspace = `${root}/workspace`, claudeConfig = `${root}/user-claude`, fakeBin = `${root}/bin`;
  await Promise.all([mkdir(home), mkdir(workspace), mkdir(claudeConfig), mkdir(fakeBin)]);
  const fakeCalls = `${root}/fake-claude-calls`;
  const fakeClaude = `${fakeBin}/claude`;
  await writeFile(fakeClaude, `#!/usr/bin/env node\nimport { writeFileSync } from "node:fs";\nwriteFileSync(process.env.FAKE_CLAUDE_CALLS, "called\\n", { flag: "a" });\nprocess.exit(97);\n`);
  await chmod(fakeClaude, 0o755);
  const settings = '{"permissions":{"deny":["Bash"]}}\n';
  await writeFile(`${claudeConfig}/settings.json`, settings);
  const env = { ...process.env, HOME: home, PATH: `${fakeBin}:${process.env.PATH ?? ""}`, FAKE_CLAUDE_CALLS: fakeCalls, MUSTR_CONFIG_DIR: `${root}/config`, MUSTR_STATE_DIR: `${root}/state`, CLAUDE_CONFIG_DIR: claudeConfig };
  const options = { env, cwd: workspace };
  assert.match((await exec(binary, ["install", "--help"], options)).stdout, /claude\s+Register.*Claude Code MCP adapter/);
  assert.match((await exec(binary, ["install", "claude", "--help"], options)).stdout, /--config-only/);
  const installed = await exec(binary, ["install", "claude", "--config-only", "--directory", directory], options);
  const canonicalDirectory = await realpath(directory);
  assert.ok(installed.stdout.includes(`${canonicalDirectory}/mcp.json`));
  const configPath = `${canonicalDirectory}/mcp.json`;
  const original = await readFile(configPath, "utf8");
  const config = JSON.parse(original).mcpServers.mustr;
  assert.equal(config.command, "node");
  assert.equal(config.args.length, 1);
  const launcherPath = `${canonicalDirectory}/mustr.mjs`;
  assert.equal(config.args[0], launcherPath);
  const launcher = await readFile(launcherPath, "utf8");
  const entryName = launcher.match(/import "\.\/(index\.[a-f0-9]{24}\.mjs)";/)?.[1];
  assert.ok(entryName, "stable launcher points to an immutable adapter asset");
  const adapter = await readFile(`${canonicalDirectory}/${entryName}`, "utf8");
  assert.ok(!adapter.includes('"__MUSTR_BINARY__"'));
  assert.ok(!adapter.includes('"__MUSTR_CONFIG__"'));
  assert.match(adapter, /\.\/transport\.[a-f0-9]{24}\.mjs/);
  assert.equal(await readFile(`${claudeConfig}/settings.json`, "utf8"), settings);
  await assert.rejects(access(`${claudeConfig}/.mcp.json`), { code: "ENOENT" });
  await exec(binary, ["install", "claude", "--config-only", "--directory", directory], options);
  assert.equal(await readFile(configPath, "utf8"), original);
  await assert.rejects(access(fakeCalls), { code: "ENOENT" });

  const child = spawn(config.command, config.args, { env: { ...env, ...config.env }, cwd: workspace, stdio: ["pipe", "pipe", "pipe"] });
  t.after(() => stop(child));
  let stdout = "", stderr = "";
  child.stdout.setEncoding("utf8").on("data", chunk => stdout += chunk);
  child.stderr.setEncoding("utf8").on("data", chunk => stderr += chunk);
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "install-test", version: "1" } } }) + "\n");
  await eventually(() => {
    if (child.exitCode !== null) throw new Error(stderr);
    return stdout.includes("\n");
  });
  const initialized = JSON.parse(stdout.split("\n")[0]);
  assert.equal(initialized.id, 1);
  assert.equal(initialized.result.serverInfo.name, "mustr");
  assert.ok(initialized.result.capabilities.tools);
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} }) + "\n");
  await eventually(() => stdout.trim().split("\n").some(line => JSON.parse(line).id === 2));
  const listed = stdout.trim().split("\n").map(line => JSON.parse(line)).find(response => response.id === 2);
  assert.ok(listed.result.tools.some(tool => tool.name === "mustr_join"));
  assert.ok(listed.result.tools.some(tool => tool.name === "mustr_ack"));
  await stop(child);
  await assert.rejects(access(`${workspace}/.mcp.json`), { code: "ENOENT" });
});

test("default Claude install adds once and upgrades through the stable managed launcher", async t => {
  const root = await mkdtemp("/tmp/mustr-claude-register-");
  t.after(() => rm(root, { recursive: true, force: true }));
  const isolatedPaths = await isolated(root);
  const directory = `${root}/bundle`;
  const homeRegistry = `${isolatedPaths.home}/.claude.json`;
  const homeSentinel = { mcpServers: { mustr: { command: "/usr/bin/true", args: ["home-sentinel"] } }, other: "preserved" };
  await writeFile(homeRegistry, JSON.stringify(homeSentinel, null, 2) + "\n");
  const settings = '{"permissions":{"deny":["Bash"]}}\n';
  await writeFile(`${isolatedPaths.claudeConfig}/settings.json`, settings);
  const overrideSentinel = { mcpServers: { other: { command: "/usr/bin/true", args: ["override-sentinel"] } }, profileSentinel: "preserved" };
  await writeFile(`${isolatedPaths.claudeConfig}/.claude.json`, JSON.stringify(overrideSentinel, null, 2) + "\n");

  await exec(binary, ["install", "claude", "--directory", directory], isolatedPaths.options);
  const canonicalDirectory = await realpath(directory);
  const launcherPath = `${canonicalDirectory}/mustr.mjs`;
  const profilePath = `${isolatedPaths.claudeConfig}/.claude.json`;
  const firstProfile = JSON.parse(await readFile(profilePath, "utf8"));
  assert.deepEqual(firstProfile.mcpServers.other, overrideSentinel.mcpServers.other);
  assert.equal(firstProfile.profileSentinel, overrideSentinel.profileSentinel);
  assert.deepEqual(firstProfile.mcpServers.mustr, {
    type: "stdio",
    command: "node",
    args: [launcherPath],
    env: { MUSTR_MANAGED: "mustr-managed Claude adapter v1" },
  });
  assert.deepEqual(JSON.parse(await readFile(homeRegistry, "utf8")), homeSentinel);
  assert.equal(await callCount(isolatedPaths.calls), 1);
  const oldLauncher = await readFile(launcherPath);
  const oldEntryName = oldLauncher.toString().match(/import "\.\/(index\.[a-f0-9]{24}\.mjs)";/)?.[1];
  assert.ok(oldEntryName);
  const moved = await exec(binary, ["install", "claude", "--directory", `${root}/different-bundle`], isolatedPaths.options).then(
    () => assert.fail("a registered bundle must not be moved implicitly"),
    error => error,
  );
  assert.notEqual(moved.code, 0);
  assert.match(moved.stderr, /original `--directory`/);
  assert.deepEqual(await readFile(launcherPath), oldLauncher);
  assert.equal(await callCount(isolatedPaths.calls), 1);

  const upgradeEnv = { ...isolatedPaths.env, MUSTR_CONFIG_DIR: `${root}/mustr-config-upgrade` };
  await exec(binary, ["install", "claude", "--directory", directory], { env: upgradeEnv, cwd: isolatedPaths.workspace });
  const newLauncher = await readFile(launcherPath);
  assert.notDeepEqual(newLauncher, oldLauncher);
  const newEntryName = newLauncher.toString().match(/import "\.\/(index\.[a-f0-9]{24}\.mjs)";/)?.[1];
  assert.ok(newEntryName);
  assert.notEqual(newEntryName, oldEntryName);
  await access(`${canonicalDirectory}/${oldEntryName}`);
  await access(`${canonicalDirectory}/${newEntryName}`);
  assert.deepEqual(await readFile(`${canonicalDirectory}/mustr.mjs.${hash(oldLauncher)}.bak`), oldLauncher);
  assert.deepEqual(JSON.parse(await readFile(profilePath, "utf8")).mcpServers.mustr, firstProfile.mcpServers.mustr);
  assert.equal(await callCount(isolatedPaths.calls), 1, "repeat/upgrade do not call native add-json again");
  assert.equal(await readFile(`${isolatedPaths.claudeConfig}/settings.json`, "utf8"), settings);
});

test("default Claude profile uses HOME when CLAUDE_CONFIG_DIR is unset", async t => {
  const root = await mkdtemp("/tmp/mustr-claude-default-profile-");
  t.after(() => rm(root, { recursive: true, force: true }));
  const isolatedPaths = await isolated(root, { override: false });
  await exec(binary, ["install", "claude", "--directory", `${root}/bundle`], isolatedPaths.options);
  const userRegistry = JSON.parse(await readFile(`${isolatedPaths.home}/.claude.json`, "utf8"));
  assert.equal(userRegistry.mcpServers.mustr.env.MUSTR_MANAGED, "mustr-managed Claude adapter v1");
  await assert.rejects(access(`${isolatedPaths.claudeConfig}/.claude.json`), { code: "ENOENT" });
  assert.equal(await callCount(isolatedPaths.calls), 1);
});

test("unmanaged user-scope mustr conflict refuses before switching the launcher or invoking Claude", async t => {
  const root = await mkdtemp("/tmp/mustr-claude-conflict-");
  t.after(() => rm(root, { recursive: true, force: true }));
  const isolatedPaths = await isolated(root);
  const directory = `${root}/bundle`;
  await mkdir(directory);
  const previousAsset = Buffer.from("previous immutable adapter");
  const previousName = `index.${hash(previousAsset)}.mjs`;
  await writeFile(`${directory}/${previousName}`, previousAsset);
  const previousLauncher = `// mustr-managed Claude launcher v1\nimport "./${previousName}";\n`;
  await writeFile(`${directory}/mustr.mjs`, previousLauncher);
  const conflict = { type: "stdio", command: "/usr/bin/true", args: ["external"], env: { OWNER: "external" } };
  const profile = { mcpServers: { mustr: conflict, other: { command: "/usr/bin/true", args: [] } }, sentinel: "preserved" };
  const profilePath = `${isolatedPaths.claudeConfig}/.claude.json`;
  const profileBytes = JSON.stringify(profile, null, 2) + "\n";
  await writeFile(profilePath, profileBytes);

  const error = await exec(binary, ["install", "claude", "--directory", directory], isolatedPaths.options).then(
    () => assert.fail("unmanaged collision should fail"),
    error => error,
  );
  assert.notEqual(error.code, 0);
  assert.match(error.stderr, /already registered with different settings/);
  assert.equal(await readFile(`${directory}/mustr.mjs`, "utf8"), previousLauncher);
  assert.equal(await readFile(profilePath, "utf8"), profileBytes);
  await assert.rejects(access(`${directory}/mcp.json`), { code: "ENOENT" });
  assert.equal(await callCount(isolatedPaths.calls), 0);
});

test("unsafe or malformed Claude user profiles fail closed without exposing contents", async t => {
  for (const kind of ["relative-override", "malformed-registry", "symlink-registry"]) {
    const root = await mkdtemp(`/tmp/mustr-claude-profile-${kind}-`);
    t.after(() => rm(root, { recursive: true, force: true }));
    const isolatedPaths = await isolated(root);
    const directory = `${root}/bundle`;
    const registry = `${isolatedPaths.claudeConfig}/.claude.json`;
    if (kind === "relative-override") {
      isolatedPaths.env.CLAUDE_CONFIG_DIR = "relative-profile";
    } else if (kind === "malformed-registry") {
      await writeFile(registry, '{"secret":"must-not-appear",');
    } else {
      const target = `${root}/profile-target.json`;
      await writeFile(target, '{"mcpServers":{}}\n');
      await symlink(target, registry);
    }

    const error = await exec(binary, ["install", "claude", "--directory", directory], isolatedPaths.options).then(
      () => assert.fail(`${kind} should fail closed`),
      error => error,
    );
    assert.notEqual(error.code, 0);
    assert.doesNotMatch(error.stderr, /must-not-appear/);
    await assert.rejects(access(`${directory}/mustr.mjs`), { code: "ENOENT" });
    assert.equal(await callCount(isolatedPaths.calls), 0);
  }
});

test("missing and failing Claude CLIs fail clearly while leaving a truthful bundle-only recovery", async t => {
  const root = await mkdtemp("/tmp/mustr-claude-cli-failure-");
  t.after(() => rm(root, { recursive: true, force: true }));
  const isolatedPaths = await isolated(root);
  const directory = `${root}/failed-bundle`;
  const failingEnv = { ...isolatedPaths.env, FAKE_CLAUDE_FAIL: "1" };
  const failing = await exec(binary, ["install", "claude", "--directory", directory], { env: failingEnv, cwd: isolatedPaths.workspace }).then(
    () => assert.fail("failed Claude CLI should produce nonzero status"),
    error => error,
  );
  assert.notEqual(failing.code, 0);
  assert.match(failing.stderr, /Claude Code registration failed/);
  assert.match(failing.stderr, /--config-only/);
  assert.doesNotMatch(failing.stdout, /Registered the user-scoped/);
  await access(`${directory}/mustr.mjs`);
  assert.equal(await callCount(isolatedPaths.calls), 1);

  const emptyBin = `${root}/empty-bin`;
  await mkdir(emptyBin);
  const missingDirectory = `${root}/missing-cli-bundle`;
  const missing = await exec(binary, ["install", "claude", "--directory", missingDirectory], {
    env: { ...isolatedPaths.env, PATH: emptyBin },
    cwd: isolatedPaths.workspace,
  }).then(
    () => assert.fail("missing Claude CLI should produce nonzero status"),
    error => error,
  );
  assert.notEqual(missing.code, 0);
  assert.match(missing.stderr, /Claude Code CLI could not be started/);
  assert.match(missing.stderr, /--config-only/);
  await access(`${missingDirectory}/mustr.mjs`);
});
