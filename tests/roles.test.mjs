import test from "node:test";
import assert from "node:assert/strict";
import { readFile, rm, writeFile } from "node:fs/promises";
import { binary, exec, fixture } from "./support.mjs";

test("roles CLI resolves one graph independently and warns while listing valid roles", { timeout: 12000 }, async t => {
  const f = await fixture(t);
  const directory = `${f.directory}/config/roles`;
  await writeFile(`${directory}/invalid.md`, "+++\ncompose = [\"missing\"]\n+++\nInvalid.\n");

  const targeted = await exec(binary, ["roles", "architect", "--json"], { env: f.env });
  const [role] = JSON.parse(targeted.stdout);
  assert.equal(JSON.parse(targeted.stdout).length, 1);
  assert.equal(role.name, "architect");
  assert.equal(role.manager, true);
  assert.match(role.prompt, /Role composition adds responsibilities/);
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
