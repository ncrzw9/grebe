// `node --test editors/vscode/test/*.test.js` -- no dependencies, no VS Code.
// The session tests drive a real `duckdb` CLI named by $DUCKDB_CLI and are
// skipped without one.

"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { parse, toTable, readTable, columnar, text } = require("../lenient-json");
const { Session, checkCli, classify, cliVersion, parseVersion } = require("../duckdb-session");

// ---------------------------------------------------------------- JSON --

test("bare NaN and Infinity parse instead of failing the document", () => {
  const t = toTable(parse('[{"a":NaN,"b":Infinity,"c":-Infinity}]'));
  assert.deepEqual(t.columns, ["a", "b", "c"]);
  assert.ok(Number.isNaN(t.rows[0][0]));
  assert.equal(t.rows[0][1], Infinity);
  assert.equal(t.rows[0][2], -Infinity);
});

test("an integer beyond 2^53 keeps its digits", () => {
  const t = toTable(parse('[{"id":9007199254740993,"small":42,"f":1.5}]'));
  assert.deepEqual(t.rows[0], [{ int: "9007199254740993" }, 42, 1.5]);
});

test("display text: what the grid shows for each kind of value", () => {
  assert.equal(text(NaN), "NaN");
  assert.equal(text(Infinity), "inf");
  assert.equal(text(-Infinity), "-inf");
  assert.equal(text(null), null);
  assert.equal(text({ int: "9007199254740993" }), "9007199254740993");
  assert.equal(text("12"), "12");
  assert.equal(
    text({ pairs: [["k", [1, null, Infinity]], ["n", { int: "18446744073709551615" }]] }),
    "{k: [1, NULL, inf], n: 18446744073709551615}",
  );
  assert.equal(text({ a: 1, b: [2] }), "{a: 1, b: [2]}", "a plain object from the fast path");
});

test("columnar: column-major text, capped, numeric columns flagged", () => {
  const t = columnar({ columns: ["a", "b"], rows: [[1, "x"], [null, "y"], [3, null]] }, 2);
  assert.deepEqual(t, { columns: ["a", "b"], num: [true, false], data: [["1", null], ["x", "y"]], total: 3 });
});

test("readTable: the fast path agrees with the careful one", () => {
  const plain = '[{"a":1,"b":"x"},\n{"a":2,"b":"y,}]"}]';
  assert.deepEqual(readTable(plain), toTable(parse(plain)));
  // Each of these must fall back to the careful reader and still be right.
  assert.deepEqual(readTable('[{"a":NaN}]').rows[0][0], NaN);
  assert.deepEqual(readTable('[{"id":9007199254740993}]').rows[0][0], { int: "9007199254740993" });
  assert.deepEqual(readTable('[{"b":1,"b":2}]'), { columns: ["b", "b"], rows: [[1, 2]] });
  assert.deepEqual(readTable('[{"2":"x","1":"y"}]').columns, ["2", "1"], "column order kept");
  assert.deepEqual(readTable("[]"), { columns: [], rows: [] });
});

test("duplicate and integer-like column names keep their order", () => {
  const t = toTable(parse('[{"b":1,"1":2,"b":3}]'));
  assert.deepEqual(t.columns, ["b", "1", "b"]);
  assert.deepEqual(t.rows[0], [1, 2, 3]);
});

test("nested values and escapes", () => {
  const t = toTable(parse('[{"l":[1,null],"s":{"k":"a\\"b\\u00e9"}}]'));
  assert.deepEqual(t.rows[0][0], [1, null]);
  assert.deepEqual(t.rows[0][1], { pairs: [["k", 'a"bé']] });
});

test("an empty result has no columns and no rows", () => {
  assert.deepEqual(toTable(parse("[]")), { columns: [], rows: [] });
});

// ------------------------------------------------------------ classify --

test("classify: the output shapes the protocol probe recorded", () => {
  assert.deepEqual(classify(""), { kind: "ok" });
  assert.equal(classify('[{"a":1}]\n').kind, "rows");
  const parser = classify(
    'Parser Error: {"exception_type":"Parser","exception_message":"syntax error at or near \\"SELEC\\"","error_subtype":"SYNTAX_ERROR","position":"0"}',
  );
  assert.deepEqual(parser, {
    kind: "error",
    type: "Parser",
    subtype: "SYNTAX_ERROR",
    message: 'syntax error at or near "SELEC"',
    position: 0,
  });
  const binder = classify(
    '{"exception_type":"Binder","exception_message":"Referenced column \\"nope\\" not found","position":"7","error_subtype":"COLUMN_NOT_FOUND"}',
  );
  assert.equal(binder.type, "Binder");
  assert.equal(binder.position, 7);
  // 2.0: parser errors are prose even with errors_as_json on.
  const prose = classify('Parser Error: syntax error at or near "id"');
  assert.deepEqual(prose, {
    kind: "error",
    type: "Parser",
    subtype: null,
    message: 'syntax error at or near "id"',
    position: null,
  });
  assert.equal(classify("┌───┐\n│ PROJECTION │").kind, "text");
});

test("parseVersion", () => {
  assert.deepEqual(parseVersion("v1.5.5 (Variegata) d8cdaa33fd"), [1, 5, 5]);
  assert.deepEqual(parseVersion("v2.0.0-alpha43089"), [2, 0, 0]);
  assert.equal(parseVersion("nonsense"), null);
});

// ------------------------------------------------------ real CLI session --

const CLI = process.env.DUCKDB_CLI;
const live = CLI && fs.existsSync(CLI) ? test : test.skip;

function session(database = ":memory:") {
  return new Session({ cli: CLI, database, cwd: os.tmpdir() });
}

live("state persists across statements, and each result is its own", async () => {
  const s = session();
  await s.start();
  try {
    assert.equal((await s.run("CREATE TEMP TABLE x AS SELECT range AS i FROM range(3);")).kind, "ok");
    const r = await s.run("SELECT i, i * 2 AS twice FROM x ORDER BY i");
    assert.equal(r.kind, "rows");
    assert.deepEqual(r.columns, ["i", "twice"]);
    assert.deepEqual(r.rows, [[0, 0], [1, 2], [2, 4]]);
    const empty = await s.run("SELECT * FROM x WHERE i > 10;");
    assert.deepEqual([empty.kind, empty.rows], ["rows", []]);
    assert.equal(typeof r.ms, "number");
  } finally {
    s.dispose();
  }
});

live("an error is reported and the session keeps going", async () => {
  const s = session();
  await s.start();
  try {
    const e = await s.run("SELECT nope;");
    assert.equal(e.kind, "error");
    assert.equal(e.type, "Binder");
    assert.equal(e.position, 7);
    const ok = await s.run("SELECT 42 AS answer");
    assert.deepEqual(ok.rows, [[42]]);
  } finally {
    s.dispose();
  }
});

live("a trailing line comment cannot swallow the terminator", async () => {
  const s = session();
  await s.start();
  try {
    const r = await s.run("SELECT 1 AS a -- no semicolon here");
    assert.deepEqual(r.rows, [[1]]);
  } finally {
    s.dispose();
  }
});

live("an rc file cannot change the protocol", async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-rc-"));
  fs.writeFileSync(path.join(home, ".duckdbrc"), ".mode box\n.timer on\n.echo on\nSELECT 'hello from rc';\n");
  const s = new Session({ cli: CLI, database: ":memory:", cwd: home });
  const prevHome = process.env.HOME;
  process.env.HOME = home; // the child inherits it
  try {
    await s.start();
    const r = await s.run("SELECT 7 AS n");
    assert.deepEqual([r.kind, r.rows], ["rows", [[7]]]);
  } finally {
    process.env.HOME = prevHome;
    s.dispose();
  }
});

live("a file the CLI cannot open fails the start with the CLI's own words", async () => {
  const bad = path.join(os.tmpdir(), `grebe-not-a-db-${process.pid}.duckdb`);
  fs.writeFileSync(bad, "this is not a database file at all, not even close");
  const s = session(bad);
  try {
    await assert.rejects(s.start(), (e) => /error/i.test(e.message));
  } finally {
    s.dispose();
    fs.rmSync(bad, { force: true });
  }
});

live("cliVersion reads the binary's version", async () => {
  const v = await cliVersion(CLI);
  assert.ok(parseVersion(v), v);
});

// ------------------------------------------- CLI problems, reported early --

// A program that is not DuckDB, and one that never answers. Plain shell, so
// these run without a DuckDB CLI.
const unix = process.platform === "win32" ? test.skip : test;
function script(name, body) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-cli-"));
  const file = path.join(dir, name);
  fs.writeFileSync(file, `#!/bin/sh\n${body}\n`);
  fs.chmodSync(file, 0o755);
  return file;
}

test("checkCli: a missing CLI is named, and marked missing", async () => {
  const r = await checkCli(path.join(os.tmpdir(), "no-such-dir", "duckdb"));
  assert.equal(r.text, null);
  assert.equal(r.missing, true);
  assert.match(r.error, /^No duckdb CLI at ".*no-such-dir.*" \(not found\)\.$/);
});

unix("checkCli: a file that is not executable says so", async () => {
  const file = script("duckdb", "");
  fs.chmodSync(file, 0o644);
  const r = await checkCli(file);
  assert.match(r.error, /is not executable\.$/);
  assert.notEqual(r.missing, true);
});

unix("checkCli: a program that is not DuckDB is refused by its --version", async () => {
  const r = await checkCli(script("yes", 'echo "yes (GNU coreutils) 9.4"'));
  assert.equal(r.text, null);
  assert.match(r.error, /does not look like the DuckDB CLI \("--version" printed "yes \(GNU coreutils\) 9\.4"\)/);
});

unix("a start that never answers fails after the timeout, not never", async () => {
  const s = new Session({ cli: script("sleeper", "exec sleep 60"), database: ":memory:", cwd: os.tmpdir(), startTimeoutMs: 300 });
  const t0 = Date.now();
  await assert.rejects(s.start(), /did not answer within 0\.3 s\. Is it the DuckDB CLI\?/);
  assert.ok(Date.now() - t0 < 5000);
  s.dispose();
});

unix("a start that prints without end is cut off, and does not hoard it", async () => {
  const s = new Session({ cli: "yes", database: ":memory:", cwd: os.tmpdir(), startTimeoutMs: 300 });
  await assert.rejects(s.start(), /did not answer/);
  assert.ok(s.chunks.length <= 2, `${s.chunks.length} chunks kept`);
  s.dispose();
});

live("checkCli: a broken ~/.duckdbrc is reported by --version already", async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-rc-"));
  fs.writeFileSync(path.join(home, ".duckdbrc"), "SELEC 1;\n");
  const prevHome = process.env.HOME;
  process.env.HOME = home;
  try {
    const r = await checkCli(CLI);
    assert.equal(r.text, null);
    assert.match(r.error, /Parser Error: syntax error at or near "SELEC"/);
    assert.match(r.error, /Encountered errors while executing init file/);
    assert.doesNotMatch(r.error, /Loading resources|\x1b\[/);
    // The session's start fails the same way, in the same words.
    const s = new Session({ cli: CLI, database: ":memory:", cwd: home });
    await assert.rejects(s.start(), (e) => /^Parser Error: syntax error at or near "SELEC"/.test(e.message));
    s.dispose();
  } finally {
    process.env.HOME = prevHome;
  }
});

live("stderr outside a statement is heard; a statement's own is not", async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-rc-"));
  fs.writeFileSync(path.join(home, ".duckdbrc"), "SELECT 1;\n");
  const heard = [];
  const s = new Session({ cli: CLI, database: ":memory:", cwd: home, onStderr: (t) => heard.push(t) });
  const prevHome = process.env.HOME;
  process.env.HOME = home;
  try {
    await s.start();
    assert.match(heard.join(""), /Loading resources from .*\.duckdbrc/);
    assert.doesNotMatch(heard.join(""), /\x1b\[/);
    heard.length = 0;
    assert.equal((await s.run("SELECT nope;")).kind, "error");
    assert.deepEqual(heard, []);
  } finally {
    process.env.HOME = prevHome;
    s.dispose();
  }
});

live("a session that dies between runs is reported when it dies", async () => {
  const ended = [];
  const s = new Session({ cli: CLI, database: ":memory:", cwd: os.tmpdir(), onExit: (r) => ended.push(r) });
  await s.start();
  s.proc.kill("SIGKILL");
  await new Promise((resolve) => s.proc.once("exit", () => setImmediate(resolve)));
  assert.deepEqual(ended, ["duckdb exited (SIGKILL)"]);
  assert.equal(s.alive, false);
  // dispose() is not news.
  const quiet = [];
  const t = new Session({ cli: CLI, database: ":memory:", cwd: os.tmpdir(), onExit: (r) => quiet.push(r) });
  await t.start();
  t.dispose();
  await new Promise((resolve) => t.proc.once("exit", () => setImmediate(resolve)));
  assert.deepEqual(quiet, []);
});

live("describe gives column types without running the query", async () => {
  const s = session();
  await s.start();
  try {
    await s.run("CREATE TABLE t AS SELECT range AS a, 'x' || range AS b FROM range(3)");
    assert.deepEqual(await s.describe("SELECT a, b FROM t WHERE a > 100 -- none match\n"), [
      { name: "a", type: "BIGINT" },
      { name: "b", type: "VARCHAR" },
    ]);
    // SUMMARIZE cannot be described directly; the FROM (...) wrapper can.
    const summary = await s.describe("SUMMARIZE t;");
    assert.ok(summary && summary.some((c) => c.name === "column_name"), JSON.stringify(summary));
    assert.equal(await s.describe("SELECT nope FROM t"), null);
  } finally {
    s.dispose();
  }
});

live("export writes the full result in each format", async () => {
  const s = session();
  await s.start();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-export-'q-"));
  try {
    await s.run("CREATE TABLE t AS SELECT range AS a, 'x' || range AS b FROM range(5)");
    for (const fmt of ["csv", "tsv", "parquet", "json"]) {
      const file = path.join(dir, `out.${fmt}`);
      const r = await s.exportTo("SELECT * FROM t ORDER BY a -- trailing comment", file, fmt);
      assert.notEqual(r.kind, "error", JSON.stringify(r));
      assert.ok(fs.existsSync(file), fmt);
    }
    assert.equal(fs.readFileSync(path.join(dir, "out.tsv"), "utf8").split("\n")[1], "0\tx0");
    assert.equal(fs.readFileSync(path.join(dir, "out.csv"), "utf8").split("\n")[0], "a,b");
    const back = await s.run(`SELECT count(*) AS n FROM '${path.join(dir, "out.parquet").replace(/'/g, "''")}'`);
    assert.deepEqual(back.rows, [[5]]);
    const summary = await s.exportTo("SUMMARIZE t;", path.join(dir, "summary.csv"), "csv");
    assert.notEqual(summary.kind, "error", JSON.stringify(summary));
  } finally {
    s.dispose();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
