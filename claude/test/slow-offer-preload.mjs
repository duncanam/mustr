// Test-only preload used to deterministically cancel after the hub has offered
// an inbox message but before the MCP adapter receives the response.
import { HubClient } from "../../pi/transport.mjs";

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
