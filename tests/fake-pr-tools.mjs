import { mkdir, readFile, writeFile } from "node:fs/promises";

// Child executables used by the Rust PR watcher. The runner chooses fixture data
// by the exact cwd it received, logs all arguments/cwds, and never contacts GitHub.
export async function installFakePrTools(f, data) {
  const input = `${f.directory}/github.json`;
  const log = `${f.directory}/commands.log`;
  const bin = `${f.directory}/bin`;
  await mkdir(bin);
  await writeFile(input, JSON.stringify(data));
  const program = `#!/usr/bin/env node
const fs = require("node:fs"), path = require("node:path");
const input = JSON.parse(fs.readFileSync(process.env.MUSTR_TEST_GITHUB));
const cwd = process.cwd(), name = path.basename(process.argv[1]), args = process.argv.slice(2);
const fixture = input.checkouts?.[cwd] ?? Object.entries(input.checkouts ?? {}).find(([dir]) => {
  try { return fs.realpathSync(dir) === cwd; } catch { return false; }
})?.[1] ?? input;
fs.appendFileSync(process.env.MUSTR_TEST_LOG, JSON.stringify({name,args,cwd}) + "\\n");
if (name === "git" && JSON.stringify(args) === JSON.stringify(["rev-parse","--show-toplevel"])) console.log(fs.realpathSync(fixture.root ?? cwd));
else if (name === "git" && JSON.stringify(args) === JSON.stringify(["rev-parse","--abbrev-ref","HEAD"])) console.log(fixture.branch);
else if (name === "git" && JSON.stringify(args) === JSON.stringify(["rev-parse","HEAD"])) console.log(fixture.head);
else if (name === "gh" && args[0] === "pr" && args[1] === "view" && args[2] === "--json") {
  if (args[3] !== "url,headRefName,state") { console.error("Unexpected discovery query"); process.exit(99); }
  const wait = fixture.delay_ms ?? input.delay_ms ?? 0;
  if (wait) Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, wait);
  console.log(JSON.stringify({url:fixture.raw.url,headRefName:fixture.raw.headRefName,state:fixture.raw.state}));
}
else if (name === "gh" && args[0] === "pr" && args[1] === "view" && args[2] === fixture.raw.url && args[3] === "--json" && args[4] === "url,state,headRefName,headRefOid,comments,reviews,statusCheckRollup") console.log(JSON.stringify(fixture.raw));
else if (name === "gh" && args[0] === "pr" && args[1] === "view" && args[2] === fixture.raw.url && args[3] === "--json" && args[4] === "headRefOid,headRefName,state") console.log(JSON.stringify({headRefOid:fixture.raw.headRefOid,headRefName:fixture.raw.headRefName,state:fixture.raw.state}));
else if (name === "gh" && args[0] === "api") {
  const parts = new URL(fixture.raw.url).pathname.split("/");
  const endpoint = "repos/" + parts[1] + "/" + parts[2] + "/pulls/" + parts[4] + "/comments?per_page=100";
  if (JSON.stringify(args) !== JSON.stringify(["api",endpoint,"--paginate","--slurp"])) { console.error("Unexpected API query"); process.exit(99); }
  console.log(JSON.stringify(fixture.pages));
}
else { console.error("Unexpected " + name + " command: " + JSON.stringify(args)); process.exit(99); }
`;
  for (const name of ["git", "gh"]) await writeFile(`${bin}/${name}`, program, { mode: 0o700 });
  Object.assign(f.env, {
    PATH: `${bin}:${f.env.PATH}`,
    MUSTR_TEST_GITHUB: input,
    MUSTR_TEST_LOG: log,
  });
  return {
    input,
    log,
    async calls() {
      try {
        return (await readFile(log, "utf8")).trim().split("\n").filter(Boolean).map(line => JSON.parse(line));
      } catch (error) {
        if (error.code === "ENOENT") return [];
        throw error;
      }
    },
    async update(next) {
      await writeFile(input, JSON.stringify(next));
    },
  };
}
