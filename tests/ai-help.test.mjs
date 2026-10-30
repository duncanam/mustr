import test from "node:test";
import assert from "node:assert/strict";
import { access, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { binary, exec } from "./support.mjs";

async function fixture(t) {
  const root = await mkdtemp("/tmp/mustr-ai-help-");
  t.after(() => rm(root, { recursive: true, force: true }));
  const bin = join(root, "bin"), marker = join(root, "unexpected-command");
  await mkdir(bin);
  for (const name of ["herdr", "ai-jail", "pi", "gh"]) {
    await writeFile(join(bin, name), `#!/bin/sh\nprintf '%s' '${name}' > '${marker}'\nexit 99\n`, { mode: 0o700 });
  }
  // Deliberately do not inherit the user's environment, configuration or Herdr.
  const env = { PATH: bin, LANG: "C", NO_COLOR: "1" };
  return { root, bin, marker, env, run: (args, overrides = {}) => exec(binary, args, { env: { ...env, ...overrides }, cwd: root, timeout: 5000 }) };
}

async function assertAbsent(path) {
  await assert.rejects(access(path), error => error.code === "ENOENT");
}

test("--ai-help works without HOME or Herdr and initializes no files or external commands", async t => {
  const f = await fixture(t);
  const { stdout, stderr } = await f.run(["--ai-help"]);
  assert.equal(stderr, "");
  assert.match(stdout, /^# mustr .+ — agent guide/);
  assert.match(stdout, /## Public CLI reference/);
  assert.match(stdout, /mustr agent start impl --profile default/);
  assert.match(stdout, /\[commands\.pi\]/);
  assert.match(stdout, /ro_maps = \[/);
  assert.match(stdout, /rw_maps = \[/);
  assert.match(stdout, /no arbitrary ai-jail\s+flag-forwarding option/i);
  assert.match(stdout, /mustr_cancel_owner_tasks/);
  assert.match(stdout, /server-wide Herdr list/);
  assert.match(stdout, /external.*jobs.*stopped/s);
  assert.doesNotMatch(stdout, /\x1b|__profile_bootstrap|--wait-for-pi|mustr-reset-internal/);
  assert.deepEqual(await readdir(f.root), ["bin"]);
  await assertAbsent(f.marker);
});

test("--ai-help bypasses invalid configuration, suppresses environment values and leaves private state unchanged", async t => {
  const f = await fixture(t);
  const config = join(f.root, "config"), state = join(f.root, "state"), home = join(f.root, "missing-home");
  await mkdir(config, { mode: 0o755 });
  await mkdir(state, { mode: 0o755 });
  const configFile = join(config, "config.toml");
  const contents = "INVALID PRIVATE CONFIG SENTINEL not TOML\n";
  await writeFile(configFile, contents, { mode: 0o600 });
  await writeFile(join(state, "private-history"), "PRIVATE HISTORY SENTINEL");
  const before = await stat(config);
  const overrides = {
    HOME: home, MUSTR_CONFIG_DIR: config, MUSTR_STATE_DIR: state,
    PI_CODING_AGENT_DIR: join(f.root, "PRIVATE-PI-DIRECTORY-SENTINEL"),
    ANTHROPIC_API_KEY: "PRIVATE-PROVIDER-KEY-SENTINEL",
    CLICOLOR_FORCE: "1", COLUMNS: "20",
  };
  const baseline = (await f.run(["--ai-help"])).stdout;
  for (const args of [
    ["--ai-help"],
    ["--ai-help", "--config-dir", "relative-path"],
    ["run", "--headless", "--profile", "INVALID PROFILE", "--jailed", "--ai-help"],
    ["agent", "start", "impl", "--ai-help"],
    ["install", "pi", "--overwrite-roles", "--ai-help"],
  ]) {
    const { stdout, stderr } = await f.run(args, overrides);
    assert.equal(stderr, "");
    assert.equal(stdout, baseline, "help must not depend on private environment/configuration/terminal width");
    assert.ok(!stdout.includes(f.root));
    assert.doesNotMatch(stdout, /PRIVATE-.*SENTINEL|INVALID PRIVATE CONFIG|PRIVATE HISTORY|\x1b/);
  }
  assert.equal(await readFile(configFile, "utf8"), contents);
  assert.deepEqual(await readdir(config), ["config.toml"]);
  assert.deepEqual(await readdir(state), ["private-history"]);
  assert.equal((await stat(config)).mode, before.mode);
  assert.equal((await stat(config)).mtimeMs, before.mtimeMs);
  await assertAbsent(home);
  await assertAbsent(f.marker);
});

test("agent reference matches every public command's actual long help, including nested options", async t => {
  const f = await fixture(t);
  const guide = (await f.run(["--ai-help"])).stdout;
  const version = (await f.run(["--version"])).stdout.trim();
  assert.ok(guide.startsWith(`# ${version} — agent guide\n`));
  const sections = new Map([...guide.matchAll(/^### `(mustr[^`]*)`\n\n```text\n([\s\S]*?)\n```/gm)].map(match => [match[1], match[2]]));
  const visited = new Set();
  // Ordinary Clap help includes an empty '=' for unset environment variables;
  // the agent guide intentionally suppresses all environment values.
  const normalize = text => text.replace(/\[env: ([A-Z0-9_]+)=\]/g, "[env: $1]").replace(/\s+/g, " ").trim();
  async function visit(args) {
    const path = ["mustr", ...args].join(" ");
    const { stdout, stderr } = await f.run([...args, "--help"]);
    assert.equal(stderr, "");
    assert.ok(sections.has(path), `missing public command: ${path}`);
    assert.equal(normalize(sections.get(path)), normalize(stdout), `stale or incomplete option reference: ${path}`);
    visited.add(path);
    const commands = stdout.split("\nCommands:\n")[1]?.split(/\n(?=\S)/)[0] ?? "";
    for (const match of commands.matchAll(/^  ([a-z][a-z0-9-]*)\s{2,}/gm)) {
      if (match[1] !== "help") await visit([...args, match[1]]);
    }
  }
  await visit([]);
  assert.deepEqual([...sections.keys()].sort(), [...visited].sort());
  assert.ok(visited.has("mustr install pi"));
  assert.ok(visited.has("mustr agent stop"));
  await assertAbsent(f.marker);
});
