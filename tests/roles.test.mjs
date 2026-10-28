import test from "node:test";
import assert from "node:assert/strict";
import { access, chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { binary, delay, eventually, exec, fixture } from "./support.mjs";

const profilePty = fileURLToPath(new URL("./profile-startup-pty.py", import.meta.url));
const duplicateHerdr = String.raw`#!/usr/bin/env node
const args = process.argv.slice(2);
if (args[0] === "--version") process.stdout.write("herdr 0.9.5\n");
else if (args[0] === "tab" && args[1] === "create") {
  const workspace_id = args[args.indexOf("--workspace") + 1];
  process.stdout.write(JSON.stringify({ id: "opaque", result: { type: "tab_created", tab: { id: "tab" }, root_pane: { pane_id: "w1:p9", workspace_id } } }) + "\n");
} else if (args[0] === "agent" && args[1] === "start") {
  process.stderr.write(JSON.stringify({ id: "opaque", error: { code: "agent_name_taken", message: "fixture duplicate" } }) + "\n");
  process.exitCode = 1;
} else process.exitCode = 99;
`;

async function startProfilePty(t, f, args, env) {
  const project = join(f.directory, "uv-role-profile-pty");
  await mkdir(project, { recursive: true });
  await writeFile(join(project, "pyproject.toml"), '[project]\nname = "mustr-role-profile-pty-test"\nversion = "0.0.0"\nrequires-python = ">=3.9"\ndependencies = []\n');
  const child = spawn("uv", ["run", "--project", project, profilePty, binary, ...args], {
    cwd: f.directory,
    env: { ...env, TERM: "xterm-256color", MUSTR_TEST_PTY_TIMEOUT: "30" },
    stdio: ["pipe", "pipe", "pipe"],
  });
  f.track(child);
  let output = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", chunk => { output += chunk; });
  child.stderr.on("data", chunk => { output += chunk; });
  const exited = once(child, "exit");
  const ready = await eventually(() => {
    if (child.exitCode !== null || child.signalCode !== null) return "exited";
    return output.includes("mustr  /  agent coordination") ? "ready" : false;
  }, 20000);
  if (ready !== "ready") throw new Error(`profile PTY exited early: ${output}`);
  return {
    child,
    exited,
    output: () => output,
    screen: () => {
      const start = output.lastIndexOf("__MSTR_SCREEN__\n");
      if (start < 0) return "";
      const content = start + "__MSTR_SCREEN__\n".length;
      const end = output.indexOf("\n__MSTR_END_SCREEN__", content);
      return end < 0 ? "" : output.slice(content, end);
    },
    send(key) { child.stdin.write(`${key}\n`); },
  };
}

async function stopProfilePty(tui) {
  if (tui.child.exitCode !== null || tui.child.signalCode !== null) return;
  tui.send("QUIT");
  await Promise.race([tui.exited, delay(8000).then(() => { throw new Error("profile PTY did not quit"); })]);
}

test("profile config precedence selects the actual ordered roles and entry count", { timeout: 120000 }, async t => {
  const f = await fixture(t);
  f.panes.set("w1:p9", { pane_id: "w1:p9", workspace_id: "w1", agent: null });
  const root = f.directory;
  const explicitConfig = join(root, "explicit-config");
  const environmentConfig = join(root, "environment-config");
  const xdgHome = join(root, "xdg");
  const xdgConfig = join(xdgHome, "mustr");
  const homeConfig = join(root, "home", ".config", "mustr");
  const configText = "version = 1\n";
  const profiles = [
    [explicitConfig, ["explicit-only"]],
    [environmentConfig, ["environment-first", "environment-second"]],
    [xdgConfig, ["xdg-first", "xdg-second", "xdg-third"]],
    [homeConfig, ["home-fallback"]],
  ];
  const before = new Map();
  for (const [directory, roles] of profiles) {
    const profileText = `[profiles.feature]\nroles = [${roles.map(role => `"${role}"`).join(", ")}]\n`;
    await mkdir(join(directory, "roles"), { recursive: true });
    await writeFile(join(directory, "config.toml"), configText);
    await writeFile(join(directory, "profiles.toml"), profileText);
    for (const role of roles) await writeFile(join(directory, "roles", `${role}.md`), `${role} prompt.\n`);
    before.set(directory, profileText);
  }

  const fakeBin = join(root, "profile-herdr-bin");
  await mkdir(fakeBin);
  const fakeHerdr = join(fakeBin, "herdr");
  await writeFile(fakeHerdr, duplicateHerdr);
  await chmod(fakeHerdr, 0o700);
  const baseEnv = {
    ...f.env,
    PATH: `${fakeBin}${process.platform === "win32" ? ";" : ":"}${process.env.PATH || ""}`,
    HOME: join(root, "home"),
    XDG_CONFIG_HOME: xdgHome,
    MUSTR_CONFIG_DIR: environmentConfig,
  };

  const runAndCheck = async (args, expected, excluded) => {
    const tui = await startProfilePty(t, f, args, baseEnv);
    await eventually(() => {
      const screen = tui.screen();
      return screen.includes("failed") && expected.every(role => screen.includes(role));
    }, 15000);
    const screen = tui.screen();
    for (const role of expected) assert.ok(screen.includes(role), `selected profile omitted ${role}:\n${screen}`);
    for (const role of excluded) assert.ok(!screen.includes(role), `lower-precedence profile leaked ${role}:\n${screen}`);
    const rows = screen.split("\n").filter(line => /^\s*\d+\/\d+\s/.test(line));
    assert.equal(rows.length, expected.length, `profile must render all ${expected.length} result rows:\n${screen}`);
    for (const [index, role] of expected.entries()) {
      assert.ok(rows[index].includes(role), `profile entry ${index + 1} should be ${role}:\n${screen}`);
    }
    assert.ok(rows[0].includes("failed"), `fixture rejection must be reported on the attempted row:\n${screen}`);
    for (const row of rows.slice(1)) {
      assert.ok(row.includes("not attempted after"), `unattempted row needs an explicit reason:\n${screen}`);
    }
    assert.ok(screen.includes(`/${expected.length}`), `profile status must report ${expected.length} ordered entries:\n${screen}`);
    await stopProfilePty(tui);
  };

  await runAndCheck(
    ["--profile", "feature", "--config-dir", explicitConfig],
    ["explicit-only"],
    ["environment-first", "environment-second", "xdg-first", "xdg-second", "xdg-third", "home-fallback"],
  );
  await runAndCheck(
    ["--profile", "feature"],
    ["environment-first", "environment-second"],
    ["explicit-only", "xdg-first", "xdg-second", "xdg-third", "home-fallback"],
  );
  delete baseEnv.MUSTR_CONFIG_DIR;
  await runAndCheck(
    ["--profile", "feature"],
    ["xdg-first", "xdg-second", "xdg-third"],
    ["explicit-only", "environment-first", "environment-second", "home-fallback"],
  );
  delete baseEnv.XDG_CONFIG_HOME;
  await runAndCheck(
    ["--profile", "feature"],
    ["home-fallback"],
    ["explicit-only", "environment-first", "environment-second", "xdg-first", "xdg-second", "xdg-third"],
  );

  for (const [directory] of profiles) {
    assert.equal(await readFile(join(directory, "config.toml"), "utf8"), configText);
    assert.equal(await readFile(join(directory, "profiles.toml"), "utf8"), before.get(directory));
  }
});

test("invalid profile commands stop before configuration initialization", { timeout: 12000 }, async t => {
  const root = await mkdtemp("/tmp/mustr-invalid-profile-command-");
  t.after(() => rm(root, { recursive: true, force: true }));
  const env = {
    ...process.env,
    HOME: join(root, "home"),
    XDG_CONFIG_HOME: join(root, "xdg"),
    MUSTR_CONFIG_DIR: join(root, "config"),
    MUSTR_STATE_DIR: join(root, "state"),
  };
  delete env.HERDR_ENV;
  delete env.HERDR_SOCKET_PATH;
  delete env.HERDR_PANE_ID;

  for (const args of [
    ["init", "--profile", "feature"],
    ["run", "--headless", "--profile", "feature"],
  ]) {
    let error;
    try {
      await exec(binary, args, { env });
    } catch (cause) {
      error = cause;
    }
    assert.ok(error, `mustr ${args.join(" ")} should reject --profile`);
    assert.match(`${error.stderr ?? ""}\n${error.stdout ?? ""}`, /--profile/i);
    await assert.rejects(access(env.MUSTR_CONFIG_DIR), { code: "ENOENT" });
  }
});

test("roles CLI resolves one graph independently and warns while listing valid roles", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const directory = `${f.directory}/config/roles`;
  await writeFile(`${directory}/invalid.md`, "+++\ncompose = [\"missing\"]\n+++\nInvalid.\n");

  const targeted = await exec(binary, ["roles", "architect", "--json"], { env: f.env });
  const [role] = JSON.parse(targeted.stdout);
  assert.equal(JSON.parse(targeted.stdout).length, 1);
  assert.equal(role.name, "architect");
  assert.equal(role.manager, true);
  assert.match(role.prompt, /A task may narrow that boundary, never widen it/);
  assert.equal(targeted.stderr, "", "targeted resolution must not inspect unrelated invalid roles");

  const listed = await exec(binary, ["roles", "--json"], { env: f.env });
  const roles = JSON.parse(listed.stdout);
  assert.ok(roles.some(role => role.name === "architect"));
  assert.ok(roles.some(role => role.name === "base"));
  assert.ok(!roles.some(role => role.name === "invalid"));
  assert.match(listed.stderr, /warning: invalid role invalid:.*invalid -> missing/);

  await assert.rejects(
    exec(binary, ["roles", "invalid", "--json"], { env: f.env }),
    error => /invalid -> missing/.test(error.stderr),
  );
});

test("manager metadata is root-only, defaults false, and must be boolean", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const roles = `${f.directory}/config/roles`;
  await writeFile(`${roles}/manager-fragment.md`, `+++
manager = true
+++
Component prompt.
`);
  await writeFile(`${roles}/composed.md`, `+++
compose = ["manager-fragment"]
+++
Root prompt.
`);
  await writeFile(`${roles}/manager-root.md`, `+++
manager = true
compose = ["manager-fragment"]
+++
Root prompt.
`);
  await writeFile(`${roles}/invalid-manager.md`, `+++
manager = "yes"
+++
Root prompt.
`);

  const resolve = async name => JSON.parse((await exec(binary, ["roles", name, "--json"], { env: f.env })).stdout)[0];
  assert.equal((await resolve("composed")).manager, false, "component metadata is ignored");
  assert.equal((await resolve("manager-root")).manager, true);
  await assert.rejects(exec(binary, ["roles", "invalid-manager", "--json"], { env: f.env }));
});

test("shipped role prompts contain the scoped functional contracts", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const output = await exec(binary, ["roles", "--json"], { env: f.env });
  const roles = Object.fromEntries(JSON.parse(output.stdout).map(role => [role.name, role]));
  const sharedBoundary = "A task may narrow that boundary, never widen it.";
  for (const name of ["base", "architect", "researcher", "implementor", "reviewer"]) {
    assert.ok(roles[name], `missing shipped role ${name}`);
    assert.ok(roles[name].prompt.includes(sharedBoundary), `${name} must compose the shared boundary`);
  }

  assert.match(roles.base.prompt, /After blocked, waiting, or done, stop modifying only that assigned scope/);
  assert.match(roles.base.prompt, /separately authorized, addressed child-result or review handoffs, or manager coordination within your selected functional role/);
  assert.match(roles.base.prompt, /a report, result, or checkpoint neither reopens the reported assignment nor widens role authority or resumes stopped work/);
  assert.match(roles.base.prompt, /Architects route independent review to a reviewer and do not perform the review themselves/);
  assert.match(roles.base.prompt, /Existing checks may create normal build, cache, and test artifacts/);

  assert.equal(roles.architect.manager, true);
  assert.match(roles.architect.description, /high-level context/);
  assert.match(roles.architect.prompt, /Route open questions, root-cause diagnosis, API exploration, and comparisons to a researcher/);
  assert.match(roles.architect.prompt, /do not conduct deep investigation, debugging, implementation, or full-suite verification yourself/);
  assert.doesNotMatch(roles.architect.prompt, /perform explicitly non-overlapping work/);

  assert.match(roles.researcher.description, /Markdown findings/);
  assert.match(roles.researcher.prompt, /Write findings, context, or memory artifacts as Markdown only/);
  assert.match(roles.researcher.prompt, /Do not edit or author source, tests, scripts, fixtures, configuration, role files, or patches/);
  assert.doesNotMatch(roles.researcher.prompt, /unless explicitly included in the assignment/);

  assert.match(roles.implementor.prompt, /Read nearby definitions and call sites/);
  assert.match(roles.implementor.prompt, /debug your own changes/);
  assert.match(roles.implementor.prompt, /The brief specifies how to measure and decide, not a predetermined result/);

  assert.match(roles.reviewer.prompt, /Do not edit or author source, tests, scripts, fixtures, configuration, role files, or patches/);
  assert.match(roles.reviewer.prompt, /Write review findings, context, or memory artifacts as Markdown only/);
  assert.match(roles.reviewer.prompt, /Existing regression tests supplied with the implementation and ordinary safe query\/filter commands are allowed/);
  const markdownArtifactLimit = "This exception permits findings/context/memory artifacts, not maintained or production documentation, role prompts, configuration, or generated maintained outputs—even when those outputs are Markdown.";
  assert.ok(roles.researcher.prompt.includes(markdownArtifactLimit));
  assert.ok(roles.reviewer.prompt.includes(markdownArtifactLimit));
  assert.doesNotMatch(roles.reviewer.prompt, /only the reviewed scope/);
});

test("explicit init restores only missing defaults; ordinary commands respect deletions", { timeout: 15000 }, async t => {
  const f = await fixture(t);
  const roles = `${f.directory}/config/roles`;
  const configPath = `${f.directory}/config/config.toml`;
  const shipped = name => readFile(new URL(`../roles/${name}.md`, import.meta.url), "utf8");
  const configBefore = await readFile(configPath, "utf8");

  for (const name of ["base", "architect", "implementor", "reviewer", "researcher"]) {
    assert.equal(await readFile(`${roles}/${name}.md`, "utf8"), await shipped(name), `fresh init seeds ${name}`);
  }

  const edited = "Hand-edited architect role; preserve exactly.\n";
  const legacy = "+++\nextends = \"implementor\"\n+++\nLegacy custom content.\n";
  const custom = "My custom role; preserve exactly.\n";
  await writeFile(`${roles}/architect.md`, edited);
  await writeFile(`${roles}/reviewer.md`, legacy);
  await writeFile(`${roles}/custom.md`, custom);
  for (const name of ["base", "implementor", "researcher"]) await rm(`${roles}/${name}.md`);

  await exec(binary, ["init"], { env: f.env });
  for (const name of ["base", "implementor", "researcher"]) {
    assert.equal(await readFile(`${roles}/${name}.md`, "utf8"), await shipped(name), `restores missing ${name}`);
  }
  assert.equal(await readFile(`${roles}/architect.md`, "utf8"), edited);
  assert.equal(await readFile(`${roles}/reviewer.md`, "utf8"), legacy);
  assert.equal(await readFile(`${roles}/custom.md`, "utf8"), custom);
  assert.equal(await readFile(configPath, "utf8"), configBefore);

  const existing = await Promise.all(["base", "architect", "implementor", "reviewer", "researcher", "custom"].map(async name => [name, await readFile(`${roles}/${name}.md`, "utf8") ]));
  await exec(binary, ["init"], { env: f.env });
  for (const [name, content] of existing) assert.equal(await readFile(`${roles}/${name}.md`, "utf8"), content);
  assert.equal(await readFile(configPath, "utf8"), configBefore);

  // Explicit init also repairs an empty roles directory despite an existing config.
  await rm(roles, { recursive: true, force: true });
  await exec(binary, ["init"], { env: f.env });
  for (const name of ["base", "architect", "implementor", "reviewer", "researcher"]) {
    assert.equal(await readFile(`${roles}/${name}.md`, "utf8"), await shipped(name));
  }
  assert.equal(await readFile(configPath, "utf8"), configBefore);

  await rm(`${roles}/researcher.md`);
  const listed = await exec(binary, ["roles", "--json"], { env: f.env });
  assert.ok(!JSON.parse(listed.stdout).some(role => role.name === "researcher"));
  await exec(binary, ["context"], { env: f.env });
  await assert.rejects(readFile(`${roles}/researcher.md`), { code: "ENOENT" });
});
