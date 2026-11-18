// sessions.js: which session a file asks for, what database a session
// opens, and what is remembered about its attachments. The parts that need
// no DuckDB always run; the rest need DUCKDB_CLI.

"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");
const { Sessions, directive, databaseFor, resolveDatabase, attachSql } = require("../sessions");
const conninfo = require("../conninfo");
const { Session } = require("../duckdb-session");

const CLI = process.env.DUCKDB_CLI;
const live = CLI && fs.existsSync(CLI) ? test : test.skip;

test("directive: only the leading comment block names a session", () => {
  assert.equal(directive("SELECT 1;"), null);
  assert.deepEqual(directive("-- grebe: session prod\nSELECT 1;"), { name: "prod", database: null });
  assert.deepEqual(directive("\n-- notes\n--grebe:session   etl-1  data/etl.duckdb  \nSELECT 1;"), { name: "etl-1", database: "data/etl.duckdb" });
  assert.deepEqual(directive("-- GREBE: SESSION Prod :workspace:\r\nSELECT 1;"), { name: "Prod", database: ":workspace:" });
  assert.equal(directive("SELECT 1;\n-- grebe: session prod\n"), null, "after a statement it is only a comment");
  assert.equal(directive("-- grebe: ignore[MOD001]\nSELECT 1;"), null);
  assert.equal(directive("-- grebe: session\nSELECT 1;"), null, "a name is needed");
  assert.equal(directive("-- grebe: session ../x\nSELECT 1;"), null, "a name is a word, not a path");
});

test("a named session never shares the default session's database file", () => {
  assert.equal(databaseFor("default", "wh.duckdb", null), "wh.duckdb");
  assert.equal(databaseFor("prod", "wh.duckdb", null), ":memory:");
  assert.equal(databaseFor("prod", "wh.duckdb", "prod.duckdb"), "prod.duckdb");
  assert.equal(databaseFor("prod", ":workspace:", null), ":workspace:");
  assert.equal(databaseFor("prod", ":memory:", null), ":memory:");
});

test("resolveDatabase: memory, a workspace file per session, paths against the folder", () => {
  const at = { cwd: "/proj", storageDir: "/store" };
  assert.equal(resolveDatabase(":memory:", at), ":memory:");
  assert.equal(resolveDatabase(":workspace:", at), path.join("/store", "session.duckdb"));
  assert.equal(resolveDatabase(":workspace:", { ...at, name: "prod" }), path.join("/store", "session-prod.duckdb"));
  assert.equal(resolveDatabase(":workspace:", { cwd: "/proj" }), ":memory:", "no storage: nowhere to keep it");
  assert.equal(resolveDatabase("wh.duckdb", at), path.join("/proj", "wh.duckdb"));
  assert.equal(resolveDatabase("/abs/wh.duckdb", at), "/abs/wh.duckdb");
});

test("what is kept of a connection has no password in it", () => {
  for (const [given, kept] of [
    ["dbname=ub host=h password=pw user=u", "dbname=ub host=h user=u"],
    ["host=h password='a b' dbname=x", "host=h dbname=x"],
    ["postgresql://u:secret@h:5432/db", "postgresql://u@h:5432/db"],
    ["postgresql://u@h/db?password=x&sslmode=require", "postgresql://u@h/db?sslmode=require"],
    ["postgresql://u@h/db?sslmode=require&password=x", "postgresql://u@h/db?sslmode=require"],
    ["/tmp/a b/x.duckdb", "/tmp/a b/x.duckdb"],
  ]) {
    assert.equal(conninfo.withoutPassword(given), kept);
    assert.doesNotMatch(conninfo.withoutPassword(given), /secret|pw\b|'a b'/);
  }
});

test("attachSql: the type and mode it had", () => {
  assert.equal(attachSql({ alias: "o", path: "/d/it's.duckdb", type: "duckdb", readOnly: false }), `ATTACH '/d/it''s.duckdb' AS "o"`);
  assert.equal(attachSql({ alias: "s", path: "/d/s.db", type: "sqlite", readOnly: true }), `ATTACH '/d/s.db' AS "s" (TYPE sqlite, READ_ONLY)`);
  assert.equal(
    attachSql({ alias: "pg", path: "dbname=ub host=h", type: "postgres", readOnly: true }, "p'w"),
    `ATTACH 'dbname=ub host=h password=''p\\''w''' AS "pg" (TYPE postgres, READ_ONLY)`,
  );
});

live("two callers opening at once share one session; the working directory follows", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-sessions-"));
  // A double quote cannot be in a Windows file name.
  const sub = path.join(dir, process.platform === "win32" ? "it's here" : `it's "here"`);
  fs.mkdirSync(sub);
  fs.writeFileSync(path.join(sub, "d.csv"), "n\n1\n");
  const said = [];
  const m = new Sessions({ log: (l) => said.push(l) });
  const spec = { cli: CLI, database: ":memory:", cwd: dir };
  const [a, b] = await Promise.all([m.open(spec), m.open(spec)]);
  assert.equal(a, b);
  assert.equal(said.filter((l) => l.startsWith("starting")).length, 1);
  assert.equal((await a.run("SELECT * FROM 'd.csv'")).kind, "error", "not in the first folder");
  assert.equal(await m.open({ ...spec, cwd: sub }), a);
  assert.equal((await a.run("SELECT * FROM 'd.csv'")).kind, "rows", "found from the folder with quotes in its name");
  // A folder that is not there leaves the session where it was.
  assert.equal(await a.chdir(path.join(dir, "missing")), false);
  assert.equal((await a.run("SELECT * FROM 'd.csv'")).kind, "rows");
  // Another database under the same name is another session.
  const c = await m.open({ ...spec, database: path.join(dir, "x.duckdb") });
  assert.notEqual(c, a);
  assert.equal(a.alive, false);
  m.end();
  assert.deepEqual(m.names(), []);
});

live("remember: files and their mode, not in-memory attachments, not the session's own file", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-sessions-"));
  const own = path.join(dir, "own.duckdb");
  // DuckDB reports a database where it really is (on macOS the temporary
  // folder is behind a link).
  const real = fs.realpathSync(dir);
  const store = new Map();
  const m = new Sessions({ store: { get: (n) => store.get(n) || [], set: (n, r) => store.set(n, r) } });
  let s = await m.open({ cli: CLI, database: own, cwd: dir });
  for (const sql of [
    `ATTACH '${path.join(dir, "rw.duckdb")}' AS rw`,
    "CREATE TABLE rw.t AS SELECT 1 AS i",
    `ATTACH ':memory:' AS mem2`,
    "USE rw",
  ]) {
    assert.notEqual((await s.run(sql)).kind, "error", sql);
  }
  await m.remember();
  // With `rw` in USE the session's own file looks attached; it is not kept.
  assert.deepEqual(store.get("default"), [{ alias: "rw", path: path.join(real, "rw.duckdb"), type: "duckdb", readOnly: false }]);
  m.end();
  await new Promise((r) => setTimeout(r, 300));

  const seen = [];
  const m2 = new Sessions({ store: { get: (n) => store.get(n) || [], set: (n, r) => store.set(n, r) }, onReattach: (n, rec, e) => seen.push([rec.alias, e]) });
  s = await m2.open({ cli: CLI, database: own, cwd: dir });
  assert.deepEqual(seen, [["rw", null]]);
  assert.equal((await s.run("SELECT i FROM rw.t")).kind, "rows");
  // Detached: still listed, as detached, and attached again when asked.
  await s.run("DETACH rw");
  await m2.remember();
  assert.deepEqual(store.get("default"), [{ alias: "rw", path: path.join(real, "rw.duckdb"), type: "duckdb", readOnly: false, detached: true }]);
  assert.equal(await m2.attach("default", "rw"), null);
  assert.equal((await s.run("SELECT i FROM rw.t")).kind, "rows");
  assert.deepEqual(store.get("default"), [{ alias: "rw", path: path.join(real, "rw.duckdb"), type: "duckdb", readOnly: false }]);
  // Forgotten: off the list.
  await s.run("DETACH rw");
  await m2.remember();
  m2.forget("default", "rw");
  assert.deepEqual(store.get("default"), []);
  // grebe.duckdb.reattach off: nothing is attached again.
  store.set("default", [{ alias: "rw", path: path.join(real, "rw.duckdb"), type: "duckdb", readOnly: true }]);
  m2.end();
  await new Promise((r) => setTimeout(r, 300));
  const m3 = new Sessions({ store: { get: (n) => store.get(n) || [], set: (n, r) => store.set(n, r) }, reattach: () => false });
  s = await m3.open({ cli: CLI, database: own, cwd: dir });
  assert.equal((await s.run("SELECT i FROM rw.t")).kind, "error");
  // It stays listed, unmarked, and can be attached by hand.
  await m3.remember();
  assert.deepEqual(store.get("default"), [{ alias: "rw", path: path.join(real, "rw.duckdb"), type: "duckdb", readOnly: true }]);
  assert.equal(await m3.attach("default", "rw"), null);
  assert.equal((await s.run("SELECT i FROM rw.t")).kind, "rows");
  m3.end();
});

test("a re-attach that fails keeps the reason without the password in it", async () => {
  const store = new Map([["default", [{ alias: "pg", path: "dbname=ub host=h user=u", type: "postgres", readOnly: true }]]]);
  const m = new Sessions({
    store: { get: (n) => store.get(n) || [], set: (n, r) => store.set(n, r) },
    askPassword: async () => "SECRETPW",
  });
  // DuckDB repeats the connection string in its error, password included.
  const fake = {
    opts: { database: ":memory:" },
    run: async (sql) => ({
      kind: "error",
      type: "Connection",
      message: /SECRETPW/.test(sql)
        ? `Unable to connect to Postgres at "dbname=ub host=h user=u password='SECRETPW'": FATAL: password authentication failed`
        : "fe_sendauth: no password supplied",
    }),
  };
  await m.reattach("default", fake);
  const kept = JSON.stringify(store.get("default"));
  assert.doesNotMatch(kept, /SECRETPW/);
  assert.match(kept, /password=\*\*\*/);
  assert.match(kept, /password authentication failed/);
});

test("a detached connection waits across a restart; a declared one is always listed", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-detached-"));
  const file = (n) => path.join(dir, `${n}.duckdb`);
  const store = new Map();
  const deps = {
    store: { get: (n) => store.get(n) || [], set: (n, r) => store.set(n, r) },
    declared: () => [{ alias: "decl", path: file("decl"), type: "duckdb", readOnly: false }],
  };
  let m = new Sessions(deps);
  let s = await m.open({ cli: CLI, database: ":memory:", cwd: dir });
  // Declared in settings: attached with the session, nothing stored yet.
  assert.equal((await s.run("CREATE TABLE decl.t AS SELECT 1 AS i")).kind, "ok");
  assert.equal((await s.run(`ATTACH '${file("kept")}' AS kept`)).kind, "ok");
  await s.run("DETACH decl");
  await m.remember();
  assert.deepEqual(m.remembered().map((r) => [r.alias, !!r.detached, !!r.declared]), [["decl", true, true], ["kept", false, false]]);
  assert.ok(store.get("default").every((r) => !("declared" in r)));
  m.end();
  await new Promise((r) => setTimeout(r, 300));

  // A new session: `kept` comes back, `decl` waits, and another program
  // could have had the file to itself meanwhile.
  m = new Sessions(deps);
  s = await m.open({ cli: CLI, database: ":memory:", cwd: dir });
  const names = async () => (await s.run("SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY 1")).rows.map((r) => r[0]);
  assert.deepEqual(await names(), ["kept", "memory"]);
  assert.equal(await m.attach("default", "decl"), null);
  assert.deepEqual(await names(), ["decl", "kept", "memory"]);
  assert.equal((await s.run("SELECT i FROM decl.t")).kind, "rows");
  // Forgetting a declared one does not take it off the list.
  m.forget("default", "decl");
  assert.equal(m.remembered().some((r) => r.alias === "decl" && r.declared), true);
  // One that cannot be attached says why, and stays.
  store.set("default", [...store.get("default"), { alias: "gone", path: path.join(dir, "no", "such.duckdb"), type: "duckdb", readOnly: true, detached: true }]);
  assert.match(await m.attach("default", "gone"), /Error/);
  assert.match(m.remembered().find((r) => r.alias === "gone").error, /Error/);
  m.end();
});
