// `node --test editors/vscode/test`. Fake binaries are small shell scripts,
// so these run on macOS and Linux; GREBE_BIN names a real grebe to accept.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");
const { candidates, probe, resolveServer, EXE } = require("../server");

const unix = process.platform !== "win32";
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-vscode-"));

function script(name, body) {
  const file = path.join(tmp, name);
  fs.writeFileSync(file, `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  return file;
}

test("accepts a binary that answers like grebe", { skip: !unix }, async () => {
  const fake = script("grebe-ok", 'echo "grebe 0.5.1"');
  assert.deepEqual(await probe(fake), { ok: true, command: fake, version: "0.5.1" });
});

test("accepts the real grebe", { skip: !process.env.GREBE_BIN }, async () => {
  const result = await probe(process.env.GREBE_BIN);
  assert.equal(result.ok, true, result.reason);
  assert.match(result.version, /^\d+\.\d+\.\d+/);
});

// The case that created a database file named `lsp` in the workspace.
test("rejects another program, such as duckdb", { skip: !unix }, async () => {
  const fake = script("duckdb", 'echo "v1.5.5 (Ossivalis) 5657cbdc0b"');
  const result = await probe(fake);
  assert.equal(result.ok, false);
  assert.match(result.reason, /not "grebe <version>"/);
});

test("rejects a binary that never answers", { skip: !unix }, async () => {
  const fake = script("hangs", "sleep 30");
  const started = Date.now();
  const result = await probe(fake, 300);
  assert.equal(result.ok, false);
  assert.match(result.reason, /no answer to --version/);
  assert.ok(Date.now() - started < 5000);
});

test("rejects a binary that reads stdin instead of answering", { skip: !unix }, async () => {
  const fake = script("reads-stdin", "cat >/dev/null; echo done");
  const result = await probe(fake, 2000);
  assert.equal(result.ok, false);
  assert.match(result.reason, /"done"/);
});

test("reports a missing binary as not found", async () => {
  const result = await probe(path.join(tmp, "nope"));
  assert.deepEqual(result, { ok: false, command: path.join(tmp, "nope"), reason: "not found" });
});

test("an explicit path is the only candidate, with ~ expanded", () => {
  assert.deepEqual(candidates("~/bin/grebe", tmp, "/home/z"), [path.join("/home/z", "bin/grebe")]);
  assert.deepEqual(candidates("/opt/grebe", tmp, "/home/z"), ["/opt/grebe"]);
});

test("without one: bundled, then ~/.local/bin, then PATH", () => {
  const ext = fs.mkdtempSync(path.join(tmp, "ext-"));
  const home = fs.mkdtempSync(path.join(tmp, "home-"));
  assert.deepEqual(candidates("", ext, home), [EXE]);
  fs.mkdirSync(path.join(home, ".local", "bin"), { recursive: true });
  fs.writeFileSync(path.join(home, ".local", "bin", EXE), "");
  fs.mkdirSync(path.join(ext, "bin"));
  fs.writeFileSync(path.join(ext, "bin", EXE), "");
  assert.deepEqual(candidates("", ext, home), [
    path.join(ext, "bin", EXE),
    path.join(home, ".local", "bin", EXE),
    EXE,
  ]);
});

test("a wrong explicit path does not fall back to a working grebe", { skip: !unix }, async () => {
  const ext = fs.mkdtempSync(path.join(tmp, "ext-"));
  fs.mkdirSync(path.join(ext, "bin"));
  script(path.join(path.relative(tmp, ext), "bin", EXE), 'echo "grebe 0.5.1"');
  const duckdb = script("duckdb2", 'echo "v1.5.5"');
  const { found, tried } = await resolveServer(duckdb, ext, 1000);
  assert.equal(found, undefined);
  assert.equal(tried.length, 1);
  assert.equal(tried[0].command, duckdb);
});

test("without an explicit path, the bundled binary wins", { skip: !unix }, async () => {
  const ext = fs.mkdtempSync(path.join(tmp, "ext-"));
  fs.mkdirSync(path.join(ext, "bin"));
  const bundled = script(path.join(path.relative(tmp, ext), "bin", EXE), 'echo "grebe 9.9.9"');
  const { found } = await resolveServer("", ext, 1000);
  assert.equal(found.command, bundled);
  assert.equal(found.version, "9.9.9");
});
