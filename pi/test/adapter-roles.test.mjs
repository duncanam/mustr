import test from "node:test";
import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { fixture, binary, eventually, delay } from "../../tests/support.mjs";
import { HubClient } from "../transport.mjs";
import extension from "../index.ts";
import { harness } from "./adapter-harness.mjs";

async function setup(t) {
  const cleanups = [];
  let h;
  // Drain adapter discovery processes before fixture files are removed.
  t.after(async () => {
    if (h) await h.emit("session_shutdown");
    for (const cleanup of cleanups) await cleanup();
  });
  const f = await fixture({ after: cleanup => cleanups.push(cleanup) });
  await f.start();
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "MUSTR_CONFIG_DIR", "MUSTR_STATE_DIR"]) process.env[key] = f.env[key];
  process.env.MUSTR_BIN = binary; process.env.HERDR_PANE_ID = "w1:p1";
  h = harness(f.directory); await extension(h.api); await h.emit("session_start");
  const write = (name, text) => writeFile(`${f.directory}/config/roles/${name}.md`, text);
  return { f, h, write };
}
const active = ["read", "bash", "policy-tool"];
async function prompt(h) {
  const result = await h.prompt("User policy");
  return result.options.sections.mustr_role ? result.text : undefined;
}
async function ready(h) { return eventually(() => h.tools.get("mustr_roster").execute("ready", {}, undefined)); }

// These use the production targeted CLI and real hub admission, not a role stub.
test("targeted selection reports a broken graph while an unrelated composed role remains usable", async t => {
  const { h, write } = await setup(t);
  await write("broken", '+++\ncompose = ["missing"]\n+++\nBroken body');
  await h.commands.get("role").handler("broken", h.context);
  assert.deepEqual(h.active(), active); assert.equal(await prompt(h), undefined);
  assert.ok(h.notifications.some(n => /broken/.test(n.message) && /missing/.test(n.message)));
  await write("shared", "SHARED COMPONENT");
  await write("left", '+++\ncompose = ["shared"]\n+++\nLEFT COMPONENT');
  await write("right", '+++\ncompose = ["shared"]\n+++\nRIGHT COMPONENT');
  await write("custom", '+++\ncompose = ["left", "right"]\n+++\nCUSTOM COMPONENT');
  await h.commands.get("role").handler("custom", h.context);
  await ready(h);
  const text = await prompt(h);
  assert.match(text, /^User policy/); assert.match(text, /protected-path/);
  assert.equal(text.match(/SHARED COMPONENT/g)?.length, 1);
  assert.ok(text.indexOf("LEFT COMPONENT") < text.indexOf("RIGHT COMPONENT"));
  assert.ok(text.indexOf("RIGHT COMPONENT") < text.indexOf("CUSTOM COMPONENT"));
  assert.ok(h.active().includes("mustr_send"));
});

for (const reconnect of [false, true]) test(`late invalid graph rejects ${reconnect ? "reconnect" : "initial Join"} without activation or automatic retry`, async t => {
  const { h, write } = await setup(t);
  const original = HubClient.prototype.connect;
  let attempts = 0, connected;
  HubClient.prototype.connect = async function(...args) {
    attempts++; connected = this;
    assert.deepEqual(h.active(), active, "tools stay inactive throughout admission");
    assert.equal(await prompt(h), undefined, "role prompt stays inactive throughout admission");
    // Change AFTER adapter's targeted resolution: Rust must reject this Join.
    if (attempts === (reconnect ? 2 : 1)) await write("implementor", '+++\ncompose = ["missing"]\n+++\nBroken at admission');
    return original.apply(this, args);
  };
  t.after(() => { HubClient.prototype.connect = original; });
  await h.commands.get("role").handler("implementor", h.context);
  if (reconnect) {
    await ready(h);
    connected.close();
    await eventually(() => !h.active().includes("mustr_roster"));
    assert.equal(await prompt(h), undefined);
    await h.emit("turn_end");
  }
  await eventually(() => h.notifications.some(n => /missing/.test(n.message) && /Participation stopped/.test(n.message)));
  assert.deepEqual(h.active(), active); assert.equal(await prompt(h), undefined);
  await assert.rejects(h.tools.get("mustr_roster").execute("rejected", {}, undefined), /disconnected/);
  const count = attempts;
  await write("implementor", "REPAIRED ROLE");
  await h.emit("turn_end"); await delay(2200);
  assert.equal(attempts, count, "repair alone cannot silently re-admit a rejected participant");
  await h.commands.get("role").handler("implementor", h.context);
  await ready(h); assert.match(await prompt(h), /REPAIRED ROLE/);
});

test("reconnect re-resolves stale local definitions before reactivating prompt and tools", async t => {
  const { h, write } = await setup(t);
  const original = HubClient.prototype.connect;
  let connected;
  HubClient.prototype.connect = async function(...args) { connected = this; return original.apply(this, args); };
  t.after(() => { HubClient.prototype.connect = original; });
  await write("implementor", "OLD ROLE BODY");
  await h.commands.get("role").handler("implementor", h.context); await ready(h);
  assert.match(await prompt(h), /OLD ROLE BODY/);
  await write("implementor", "NEW ROLE BODY");
  connected.close();
  await eventually(() => !h.active().includes("mustr_roster"));
  assert.equal(await prompt(h), undefined);
  await h.emit("turn_end");
  await eventually(async () => /NEW ROLE BODY/.test(await prompt(h)));
  assert.doesNotMatch(await prompt(h), /OLD ROLE BODY/);
  assert.ok(h.active().includes("mustr_roster"));
});

test("invalid local graph on reconnect disables participation before another Join", async t => {
  const { h, write } = await setup(t);
  const original = HubClient.prototype.connect;
  let attempts = 0, connected;
  HubClient.prototype.connect = async function(...args) { attempts++; connected = this; return original.apply(this, args); };
  t.after(() => { HubClient.prototype.connect = original; });
  await h.commands.get("role").handler("implementor", h.context); await ready(h);
  await write("implementor", '+++\ncompose = ["implementor"]\n+++\nCycle');
  connected.close(); await h.emit("turn_end");
  await eventually(() => h.notifications.some(n => /cycle/.test(n.message)));
  assert.equal(attempts, 1); assert.deepEqual(h.active(), active); assert.equal(await prompt(h), undefined);
});
