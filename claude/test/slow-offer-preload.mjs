// Test-only preload used to deterministically cancel after the hub has offered
// an inbox message but before the MCP adapter receives the response.
import { appendFileSync } from "node:fs";
import crypto from "node:crypto";
import { syncBuiltinESMExports } from "node:module";
import { HubClient, HubRemoteError } from "../../pi/transport.mjs";

if (process.env.MUSTR_TEST_AGENT_ID) {
  const randomUUID = crypto.randomUUID;
  let useInjectedId = true;
  crypto.randomUUID = (...args) => {
    if (useInjectedId) {
      useInjectedId = false;
      return process.env.MUSTR_TEST_AGENT_ID;
    }
    return randomUUID(...args);
  };
  syncBuiltinESMExports();
}

const delayMs = Number(process.env.MUSTR_TEST_DELAY_OFFER_MS || 0);
if (Number.isInteger(delayMs) && delayMs > 0) {
  const request = HubClient.prototype.request;
  HubClient.prototype.request = async function (command, timeoutMs) {
    const result = await request.call(this, command, timeoutMs);
    if (command?.op === "sync" && command.receive === true && result?.inbox) {
      await new Promise(resolve => setTimeout(resolve, delayMs));
    }
    return result;
  };
}

if (process.env.MUSTR_TEST_RETIRE_OP) {
  const request = HubClient.prototype.request;
  const retireAfter = Math.max(1, Number(process.env.MUSTR_TEST_RETIRE_AFTER || 1));
  let matchingRequests = 0;
  let retired = false;
  HubClient.prototype.request = async function (command, timeoutMs) {
    if (process.env.MUSTR_TEST_REQUEST_LOG) appendFileSync(process.env.MUSTR_TEST_REQUEST_LOG, `${command?.op ?? "unknown"}\n`);
    if (command?.op === process.env.MUSTR_TEST_RETIRE_OP) matchingRequests++;
    if (!retired && matchingRequests >= retireAfter && command?.op === process.env.MUSTR_TEST_RETIRE_OP) {
      retired = true;
      throw new HubRemoteError("connection is retired; rejoin explicitly from the correct session", "connection_retired");
    }
    return request.call(this, command, timeoutMs);
  };
}

if (process.env.MUSTR_TEST_FAIL_WAIT_OUTPUT === "1") {
  const write = process.stdout.write.bind(process.stdout);
  process.stdout.write = function (chunk, encoding, callback) {
    const text = Buffer.isBuffer(chunk) ? chunk.toString("utf8") : String(chunk);
    const done = typeof encoding === "function" ? encoding : callback;
    if (text.includes("acknowledged") && text.includes("message") && text.includes("\"isError\":false")) {
      process.nextTick(() => done?.(new Error("fixture stdio output failure")));
      return false;
    }
    return write(chunk, encoding, callback);
  };
}
