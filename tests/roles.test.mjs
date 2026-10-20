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
