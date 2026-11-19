// sessions.js: which session a file asks for, what database a session
// opens, and when it attaches the connections of the setting: files behind
// its start, servers when needed, within a time limit, and in step with the
// setting. The parts that need no DuckDB always run; the rest need
// DUCKDB_CLI (and PG_TEST_CONN for a password-protected PostgreSQL).

"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const net = require("node:net");
const path = require("node:path");
const test = require("node:test");
const { Sessions, directive, databaseFor, resolveDatabase, attachSql, CONNECT_TIMEOUT_S } = require("../sessions");
const { normalize } = require("../connections");
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


test("attachSql: the type and mode it had, and a connect timeout for PostgreSQL", () => {
  assert.equal(attachSql({ alias: "o", path: "/d/it's.duckdb", type: "duckdb", readOnly: false }), `ATTACH '/d/it''s.duckdb' AS "o"`);
  assert.equal(attachSql({ alias: "s", path: "/d/s.db", type: "sqlite", readOnly: true }), `ATTACH '/d/s.db' AS "s" (TYPE sqlite, READ_ONLY)`);
  assert.equal(
    attachSql({ alias: "pg", path: "dbname=ub host=h", type: "postgres", readOnly: true }, "p'w"),
    `ATTACH 'dbname=ub host=h connect_timeout=${CONNECT_TIMEOUT_S} password=''p\\''w''' AS "pg" (TYPE postgres, READ_ONLY)`,
  );
  assert.equal(
    attachSql({ alias: "pg", path: "postgresql://u@h/db?connect_timeout=3", type: "postgres", readOnly: false }),
    `ATTACH 'postgresql://u@h/db?connect_timeout=3' AS "pg" (TYPE postgres)`,
    "one the string sets is kept",
  );
});

// ------------------------------------------------------------ live helpers --

/** A Sessions whose setting is `list` (raw entries, as in settings.json),
 *  which a test edits. */
function manager(list, more = {}) {
  const said = [];
  const m = new Sessions({ connections: () => normalize(list), log: (l) => said.push(l), ...more });
  m.said = said;
  return m;
}

/** The databases `sess` has attached, by name, its own and temp left out. */
async function names(sess) {
  const r = await sess.run("SELECT database_name FROM duckdb_databases() WHERE NOT internal AND database_name <> current_database() ORDER BY 1");
  return r.rows.map((row) => row[0]);
}

/** A TCP server that accepts and never answers: a PostgreSQL server that
 *  hangs. */
async function silentServer(t) {
  const sockets = [];
  const srv = net.createServer((s) => sockets.push(s));
  await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
  t.after(() => {
    for (const s of sockets) s.destroy();
    srv.close();
  });
  return srv.address().port;
}

/** A database file with one table in it. */
async function dbFile(file) {
  const w = new Session({ cli: CLI, database: file, cwd: path.dirname(file) });
  await w.start();
  await w.run("CREATE TABLE t AS SELECT 1 AS i");
  w.dispose();
  await new Promise((r) => setTimeout(r, 200));
}

const PG = process.env.PG_TEST_CONN;

live("a session starts at once: its files are attached behind it, servers wait, and nothing is asked", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), "grebe-sessions-")));
  await dbFile(path.join(dir, "a.duckdb"));
  const port = await silentServer(t);
  const list = [
    { alias: "a", path: path.join(dir, "a.duckdb") },
    { alias: "gone", path: path.join(dir, "no", "such.duckdb") },
    { alias: "det", path: path.join(dir, "det.duckdb"), detached: true },
    { alias: "slow", path: `host=127.0.0.1 port=${port} dbname=x user=u`, type: "postgres" },
    { alias: "other", path: path.join(dir, "o.duckdb"), session: "etl" },
  ];
  let asked = 0;
  const m = manager(list, { askPassword: async () => (asked++, "pw") });
  const sess = await m.open({ cli: CLI, database: ":memory:", cwd: dir });
  // open() came back before the files were attached: they are on their way.
  assert.equal(m.state("default", "a").attaching, true);
  assert.equal(m.state("default", "slow").attaching, false, "a server is not attached at start");
  // A statement asked for now runs; the files' attaches are quick.
  assert.equal((await sess.run("SELECT 42")).kind, "rows");
  await sess.attaching;
  assert.deepEqual(await names(sess), ["a"]);
  assert.equal((await sess.run("SELECT i FROM a.t")).kind, "rows");
  assert.match(m.state("default", "gone").error, /Error/, "one that failed says why");
  assert.equal(m.state("default", "det").error, null, "a detached one is left alone");
  assert.equal(asked, 0, "nothing is asked when a session starts");

  // grebe.duckdb.reattach off: nothing at all.
  m.end();
  const off = manager(list, { reattach: () => false });
  const s2 = await off.open({ cli: CLI, database: ":memory:", cwd: dir });
  await s2.attaching;
  assert.deepEqual(await names(s2), []);
  off.end();
});

live("an attach that hangs holds a run up no longer than its own limit, and a password prompt not at all", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-sessions-"));
  const port = await silentServer(t);
  const list = [{ alias: "slow", path: `host=127.0.0.1 port=${port} dbname=x user=u connect_timeout=2`, type: "postgres" }];
  if (PG) list.push({ alias: "pg", path: conninfo.withoutPassword(PG), type: "postgres", readOnly: true });
  let answer = null;
  let asked = 0;
  const m = manager(list, {
    askPassword: () => {
      asked++;
      return new Promise((resolve) => (answer = resolve));
    },
  });
  const sess = await m.open({ cli: CLI, database: ":memory:", cwd: dir });
  assert.equal((await sess.run("LOAD postgres")).kind, "ok");

  // The server takes the connection and never answers. libpq gives up
  // (after connect_timeout, once with SSL and once without); the run
  // behind it waits for that and no longer, and the session lives on.
  const attaching = m.ensure("default", "slow", { check: false });
  const t0 = Date.now();
  const r = await sess.run("SELECT 42 AS n");
  const waited = Date.now() - t0;
  assert.equal(r.kind, "rows");
  assert.ok(waited < 6000, `the run waited ${waited} ms`);
  assert.match(await attaching, /timeout expired/);
  assert.equal(sess.alive, true);
  assert.match(m.state("default", "slow").error, /timeout expired/);

  if (!PG) return;
  // A server that wants a password: the prompt waits for the user, and a
  // run that does not need that server does not wait for the prompt.
  const pg = m.ensure("default", "pg");
  for (let i = 0; i < 200 && !answer; i++) await new Promise((res) => setTimeout(res, 25));
  assert.equal(asked, 1);
  const t1 = Date.now();
  assert.equal((await sess.run("SELECT 1")).kind, "rows");
  assert.ok(Date.now() - t1 < 1000, "not held up by the prompt");
  answer(conninfo.parse(PG).password);
  assert.equal(await pg, null);
  assert.deepEqual(await names(sess), ["pg"]);
  // Asked once: attached is attached.
  assert.equal(await m.ensure("default", "pg"), null);
  assert.equal(asked, 1);
  m.end();
});

live("a statement that refers to a connection attaches it first; a detached one is left alone", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), "grebe-sessions-")));
  await dbFile(path.join(dir, "x.duckdb"));
  await dbFile(path.join(dir, "d.duckdb"));
  const list = [
    { alias: "x", path: path.join(dir, "x.duckdb") },
    { alias: "d", path: path.join(dir, "d.duckdb"), detached: true },
  ];
  if (PG) list.push({ alias: "pg", path: conninfo.withoutPassword(PG), type: "postgres", readOnly: true });
  let asked = 0;
  const m = manager(list, { reattach: () => false, askPassword: async () => (asked++, conninfo.parse(PG).password) });
  const sess = await m.open({ cli: CLI, database: ":memory:", cwd: dir });
  await sess.attaching;
  assert.deepEqual(await names(sess), []);
  // Not a reference: in a string, in a comment, a column of a table x.
  assert.deepEqual(await m.prepare("default", "SELECT 'x.t' AS s -- x.t\n/* USE x */"), []);
  assert.deepEqual(await names(sess), []);
  assert.deepEqual(await m.prepare("default", `SELECT * FROM "X".t`), []);
  assert.deepEqual(await names(sess), ["x"]);
  assert.deepEqual(await m.prepare("default", "FROM d.t"), []);
  assert.deepEqual(await names(sess), ["x"], "detached: left for DuckDB to say it is not there");
  if (PG) {
    assert.deepEqual(await m.prepare("default", "USE pg"), []);
    assert.equal(asked, 1, "the server asked for its password, once");
    assert.deepEqual(await names(sess), ["pg", "x"]);
  }
  m.end();
});

live("reconcile: a running session follows the setting, and a change it made already asks for nothing", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), "grebe-sessions-")));
  for (const f of ["x", "y", "y2", "z"]) await dbFile(path.join(dir, `${f}.duckdb`));
  const list = [{ alias: "x", path: path.join(dir, "x.duckdb") }];
  const m = manager(list);
  const sess = await m.open({ cli: CLI, database: ":memory:", cwd: dir });
  await sess.attaching;
  assert.deepEqual(await names(sess), ["x"]);
  const statements = [];
  const run = sess.run.bind(sess);
  sess.run = (sql, opts) => (statements.push(sql), run(sql, opts));

  list.push({ alias: "y", path: path.join(dir, "y.duckdb"), readOnly: true });
  await m.reconcile();
  assert.deepEqual(await names(sess), ["x", "y"]);
  list[0] = { ...list[0], detached: true };
  await m.reconcile();
  assert.deepEqual(await names(sess), ["y"]);
  list[1] = { ...list[1], path: path.join(dir, "y2.duckdb") };
  await m.reconcile();
  const at = await sess.run("SELECT path FROM duckdb_databases() WHERE database_name = 'y'");
  assert.equal(at.rows[0][0], path.join(dir, "y2.duckdb"), "attached again where it is now");
  list.splice(1, 1);
  await m.reconcile();
  assert.deepEqual(await names(sess), []);
  list[0] = { alias: "x", path: path.join(dir, "x.duckdb") };
  await m.reconcile();
  assert.deepEqual(await names(sess), ["x"]);
  // A server waits until it is needed.
  list.push({ alias: "far", path: "host=127.0.0.1 port=1 dbname=x user=u", type: "postgres" });
  await m.reconcile();
  assert.deepEqual(await names(sess), ["x"]);

  // Attached first (the Catalog, or an ATTACH in SQL that is kept), then
  // added: nothing more is done.
  assert.equal((await sess.run(`ATTACH '${path.join(dir, "z.duckdb")}' AS z`)).kind, "ok");
  statements.length = 0;
  list.push({ alias: "z", path: path.join(dir, "z.duckdb") });
  await m.reconcile();
  assert.deepEqual(statements.filter((s) => /ATTACH|DETACH/.test(s)), []);
  // No change, no statement at all.
  statements.length = 0;
  await m.reconcile();
  assert.deepEqual(statements, []);
  // Detached first (the Catalog's Detach), then marked: nothing more.
  assert.equal((await sess.run("DETACH z")).kind, "ok");
  list[list.length - 1] = { ...list[list.length - 1], detached: true };
  await m.reconcile();
  assert.deepEqual(statements.filter((s) => /ATTACH|DETACH/.test(s)), ["DETACH z"]);
  assert.deepEqual(await names(sess), ["x"]);
  m.end();
});

test("a failed attach keeps the reason without the password in it", async () => {
  const list = [{ alias: "pg", path: "dbname=ub host=h user=u", type: "postgres", readOnly: true }];
  const m = manager(list, { askPassword: async () => "SECRETPW" });
  // DuckDB repeats the connection string in its error, password included.
  const fake = {
    alive: true,
    opts: { database: ":memory:" },
    run: async (sql) =>
      /^SELECT/.test(sql)
        ? { kind: "rows", columns: ["alias", "path", "type", "readonly"], rows: [] }
        : {
            kind: "error",
            type: "Connection",
            message: /SECRETPW/.test(sql)
              ? `Unable to connect to Postgres at "dbname=ub host=h user=u password='SECRETPW'": FATAL: password authentication failed`
              : "fe_sendauth: no password supplied",
          },
  };
  m.sessions.set("default", fake);
  const error = await m.ensure("default", "pg");
  assert.doesNotMatch(error, /SECRETPW/);
  assert.match(error, /password=\*\*\*/);
  assert.match(error, /password authentication failed/);
  assert.equal(m.state("default", "pg").error, error);
  assert.doesNotMatch(m.said.join("\n"), /SECRETPW/);
  // No password given: said so, and nothing tried with one.
  const m2 = manager(list, { askPassword: async () => undefined });
  m2.sessions.set("default", fake);
  assert.equal(await m2.ensure("default", "pg"), "no password was given");
});
