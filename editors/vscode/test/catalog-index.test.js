// The catalog index (catalogIndex.js) against a real `duckdb` CLI
// ($DUCKDB_CLI): what is read when, what is kept on disk, and what a part
// says of a table. No VS Code and no language server; run-e2e.test.js has
// the tests of what is sent to the server.
//
// "Foreign" is tested three ways: a DuckDB file the test declares foreign
// (the index takes that decision as a function), which runs wherever the
// CLI does; a SQLite file, when DuckDB's sqlite extension can be loaded;
// and a PostgreSQL server, when $PG_TEST_CONN names one.

"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { Session } = require("../duckdb-session");
const { CatalogIndex, CACHE_DIR, MAX_COLUMNS, cacheKey, changesStructure, mentions } = require("../catalogIndex");

const CLI = process.env.DUCKDB_CLI;
const live = CLI && fs.existsSync(CLI) ? test : test.skip;
const PG = process.env.PG_TEST_CONN;
const livePg = CLI && PG && fs.existsSync(CLI) ? test : test.skip;

// Where DuckDB will say the folder is: on macOS the temporary folder is
// behind a link, and Windows may give it by its short name.
const tmp = (name) => fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), `grebe-index-${name}-`)));

/** A session whose statements are recorded, and a way to run one as a
 *  user's run does: the index is told what ran. */
async function harness(t, index, name = "default") {
  const sess = new Session({ cli: CLI, database: ":memory:", cwd: os.tmpdir() });
  await sess.start();
  t.after(() => sess.dispose());
  const asked = [];
  const run = sess.run.bind(sess);
  sess.run = (sql, opts) => {
    asked.push(sql);
    return run(sql, opts);
  };
  return {
    sess,
    asked,
    /** How many times the columns of `db` were read. */
    reads: (db) => asked.filter((q) => q.includes(`FROM duckdb_columns() WHERE database_name IN ('${db}')`)).length,
    /** Run `sql` (a `kind` statement, as the language server names it). */
    async user(sql, kind = "create") {
      const r = await run(sql);
      assert.notEqual(r.kind, "error", `${sql}: ${r.message}`);
      index().noteRun(name, [{ text: sql, kind }]);
      return r;
    },
    /** What a sync does: read what is cheap, then index what is pending. */
    async settle() {
      await index().refresh(name, sess);
      assert.equal(await index().indexPending(name, sess), true, "nothing left to index");
    },
    part: (db) => index().parts(name).find((p) => p.database === db),
  };
}

const tableOf = (part, name) => part.tables.find((x) => x.table === name);

// ------------------------------------------------------------ plain parts --

test("which statements can change a database, and which name it", () => {
  for (const sql of ["CREATE TABLE t (a INT)", "  -- note\n/* more */ alter table t add column b int", "DROP VIEW v", "ATTACH 'x.db' AS x", "DETACH x", "USE pg", "CALL postgres_execute('pg', 'CREATE TABLE t ()')", "SELECT * FROM postgres_execute('pg', 'DROP TABLE t')"]) {
    assert.equal(changesStructure(sql), true, sql);
  }
  for (const sql of ["SELECT 1", "INSERT INTO t VALUES (1)", "UPDATE t SET a = 1", "-- CREATE TABLE no\nSELECT 'DROP TABLE t'", "WITH created AS (SELECT 1) SELECT * FROM created"]) {
    assert.equal(changesStructure(sql), false, sql);
  }
  assert.equal(mentions("CREATE TABLE pg.public.t (a INT)", "pg"), true);
  assert.equal(mentions('CREATE TABLE "PG".t (a INT)', "pg"), true);
  assert.equal(mentions("CREATE TABLE pgx.t (a INT)", "pg"), false);
  assert.equal(mentions("CREATE TABLE my_pg.t (a INT)", "pg"), false);
  assert.equal(mentions("DROP TABLE a.b", "a.b"), true, "a name with a dot in it is taken literally");
  assert.equal(mentions("DROP TABLE axb", "a.b"), false);
});

test("a connection's cache file is named by a hash that the password is not part of", () => {
  const a = cacheKey("postgres", "dbname=shop host=db.example.com user=me password=hunter2");
  const b = cacheKey("postgres", "dbname=shop host=db.example.com user=me password='another one'");
  const c = cacheKey("postgres", "dbname=shop host=db.example.com user=me");
  assert.match(a, /^[0-9a-f]{32}$/);
  assert.equal(a, b);
  assert.equal(a, c);
  assert.notEqual(a, cacheKey("postgres", "dbname=shop host=other.example.com user=me"));
  assert.notEqual(a, cacheKey("mysql", "dbname=shop host=db.example.com user=me"));
  assert.equal(cacheKey("postgres", "postgresql://me:hunter2@db.example.com/shop"), cacheKey("postgres", "postgresql://me@db.example.com/shop"));
});

// ------------------------------------------------------- local databases --

live("local databases: a part each, with keys, indexes, row estimates and nullability", async (t) => {
  const dir = tmp("local");
  const other = path.join(dir, "other.duckdb");
  let index = new CatalogIndex({});
  const h = await harness(t, () => index);
  await h.user(`ATTACH '${other}' AS other`, "attach");
  await h.user(`CREATE TABLE other.facts (id INTEGER PRIMARY KEY, code VARCHAR NOT NULL, n INTEGER, UNIQUE (code, n))`);
  await h.user(`CREATE INDEX facts_n ON other.facts (n, code)`);
  await h.user(`CREATE UNIQUE INDEX facts_code ON other.facts (code)`);
  await h.user(`INSERT INTO other.facts SELECT i, i::VARCHAR, i FROM range(1200) r(i)`, "insert");
  await h.user(`CREATE VIEW other.recent AS SELECT * FROM other.facts`);
  await h.user(`CREATE TABLE orders (id INTEGER, total DECIMAL(10,2))`);
  await h.user(`CREATE TEMP TABLE scratch (x INTEGER)`);
  await h.user(`CREATE MACRO twice(x) AS x * 2`);
  await h.settle();

  assert.deepEqual(index.parts("default").map((p) => p.database).sort(), ["memory", "other", "temp"]);
  assert.deepEqual(h.part("memory"), {
    database: "memory",
    digest: h.part("memory").digest,
    tables: [{ schema: "main", table: "orders", rows: 0, columns: ["id", "total"], types: ["INTEGER", "DECIMAL(10,2)"], nullable: [true, true] }],
    functions: [{ name: "twice", kind: "macro" }],
  });
  const facts = tableOf(h.part("other"), "facts");
  assert.deepEqual(facts, {
    schema: "main",
    table: "facts",
    rows: 1200,
    primary_key: ["id"],
    unique: [["code", "n"], ["code"]],
    indexes: [["n", "code"]],
    columns: ["id", "code", "n"],
    types: ["INTEGER", "VARCHAR", "INTEGER"],
    nullable: [false, false, true],
  });
  const recent = tableOf(h.part("other"), "recent");
  assert.equal(recent.kind, "view");
  assert.equal("rows" in recent, false, "a view has no row estimate");
  assert.deepEqual(recent.columns, ["id", "code", "n"]);
  assert.equal(index.status("default", "other").foreign, false);

  // One database changes: only its part does.
  const before = Object.fromEntries(index.parts("default").map((p) => [p.database, p.digest]));
  await h.user(`CREATE TABLE other.more (x INTEGER)`);
  await h.settle();
  assert.equal(h.part("memory").digest, before.memory);
  assert.equal(h.part("temp").digest, before.temp);
  assert.notEqual(h.part("other").digest, before.other);

  // Emptied: no part, rather than an empty one. Detached: gone.
  await h.user(`DROP TABLE scratch`, "drop");
  await h.settle();
  assert.equal(h.part("temp"), undefined);
  await h.user(`DETACH other`, "detach");
  await h.settle();
  assert.deepEqual(index.parts("default").map((p) => p.database), ["memory"]);
  assert.equal(index.status("default", "other"), null);
  // No foreign database: nothing was written anywhere.
  assert.deepEqual(fs.readdirSync(dir).filter((f) => !f.endsWith(".wal")), ["other.duckdb"]);
});

// ---------------------------------------- a DuckDB file, declared foreign --

live("a foreign database is read once; again on DDL that names it, on request and by age", async (t) => {
  const dir = tmp("foreign");
  const storage = tmp("storage");
  const wh = path.join(dir, "wh.duckdb");
  let clock = 1700000000000;
  const said = [];
  const changes = [];
  const index = new CatalogIndex({
    storageDir: storage,
    isForeign: (d) => d.db === "wh",
    maxAgeMs: () => 60 * 60000,
    now: () => clock,
    log: (l) => said.push(l),
    onChange: (name) => changes.push(name),
  });
  const h = await harness(t, () => index);
  await h.user(`ATTACH '${wh}' AS wh`, "attach");
  await h.user(`CREATE TABLE wh.facts (id INTEGER PRIMARY KEY, v VARCHAR NOT NULL)`);
  await h.user(`CREATE TABLE local_t (a INTEGER)`);

  // First seen: noticed by the cheap reading, read by the indexing pass.
  await index.refresh("default", h.sess);
  assert.equal(h.part("wh"), undefined, "not read with the local ones");
  assert.deepEqual(index.status("default", "wh"), { foreign: true, tables: 0, columns: 0, indexedAt: null, indexing: false, stale: null, over: null, limit: MAX_COLUMNS, error: null });
  assert.ok(h.part("memory"));
  await h.settle();
  assert.equal(h.reads("wh"), 1);
  assert.deepEqual(h.part("wh").tables, [{ schema: "main", table: "facts", columns: ["id", "v"], types: ["INTEGER", "VARCHAR"], nullable: [false, false] }]);
  assert.equal(index.status("default", "wh").indexedAt, clock);
  assert.ok(changes.length >= 2, "listeners hear the indexing start and end");
  assert.match(said.at(-1), /^indexed wh \(duckdb, wh\.duckdb\): 1 tables, 2 columns in /);

  // Queries: nothing at all is read while a foreign database is attached.
  const quiet = h.asked.length;
  await h.user(`SELECT * FROM wh.facts`, "select");
  await h.user(`SELECT * FROM local_t`, "select");
  await h.settle();
  assert.equal(h.asked.length, quiet, h.asked.slice(quiet).join("\n"));

  // A statement that changes something else: the local ones are read
  // again, the foreign one is not.
  const memory = h.part("memory").digest;
  await h.user(`CREATE TABLE local_2 (b INTEGER)`);
  await h.user(`INSERT INTO wh.facts VALUES (1, 'x')`, "insert");
  await h.settle();
  assert.notEqual(h.part("memory").digest, memory);
  assert.equal(h.reads("wh"), 1);

  // DDL that names it.
  await h.user(`CREATE TABLE wh.more (x INTEGER)`);
  assert.equal(index.status("default", "wh").stale, "a statement changed it");
  await h.settle();
  assert.equal(h.reads("wh"), 2);
  assert.deepEqual(h.part("wh").tables.map((x) => x.table), ["facts", "more"]);
  assert.equal(index.status("default", "wh").stale, null);

  // Any DDL while it is the current database.
  await h.user(`USE wh`, "use");
  await h.settle();
  assert.equal(h.reads("wh"), 3, "USE names it");
  await h.user(`CREATE TABLE unqualified (y INTEGER)`);
  await h.settle();
  assert.equal(h.reads("wh"), 4);
  assert.ok(tableOf(h.part("wh"), "unqualified"));
  await h.user(`USE memory`, "use");
  await h.settle();
  const settled = h.reads("wh");

  // On request.
  index.reindex("default", "wh");
  await h.settle();
  assert.equal(h.reads("wh"), settled + 1);
  // By age: checked at the next sync, with no run needed.
  clock += 59 * 60000;
  await h.settle();
  assert.equal(h.reads("wh"), settled + 1);
  clock += 2 * 60000;
  await h.settle();
  assert.equal(h.reads("wh"), settled + 2);
  assert.equal(index.status("default", "wh").indexedAt, clock);
});

live("a foreign index is kept between windows, without the path, and goes when the connection is forgotten", async (t) => {
  const dir = tmp("kept");
  const storage = tmp("storage");
  const wh = path.join(dir, "wh.duckdb");
  let remembered = [{ type: "duckdb", path: wh }];
  const said = [];
  const make = () => new CatalogIndex({ storageDir: storage, isForeign: (d) => d.db === "wh", connections: () => remembered, log: (l) => said.push(l) });
  let index = make();
  const first = await harness(t, () => index);
  await first.user(`ATTACH '${wh}' AS wh`, "attach");
  await first.user(`CREATE TABLE wh.facts (id INTEGER PRIMARY KEY, v VARCHAR)`);
  await first.settle();
  const file = path.join(storage, CACHE_DIR, `${cacheKey("duckdb", wh)}.json`);
  assert.deepEqual(fs.readdirSync(path.join(storage, CACHE_DIR)), [path.basename(file)]);
  const text = fs.readFileSync(file, "utf8");
  const kept = JSON.parse(text);
  assert.deepEqual(Object.keys(kept), ["version", "type", "where", "indexedAt", "columns", "tables"]);
  assert.deepEqual([kept.version, kept.type, kept.where, kept.columns], [1, "duckdb", "wh.duckdb", 2]);
  assert.deepEqual(kept.tables, first.part("wh").tables);
  assert.ok(!text.includes(dir), "no path of this machine is written");
  first.sess.dispose();

  // The next window: the names are there before anything is read.
  index = make();
  const second = await harness(t, () => index);
  await second.user(`ATTACH '${wh}' AS warehouse_again`, "attach");
  index = new CatalogIndex({ storageDir: storage, isForeign: (d) => d.db === "warehouse_again", connections: () => remembered, log: (l) => said.push(l) });
  await index.refresh("default", second.sess);
  assert.deepEqual(second.part("warehouse_again").tables, kept.tables, "by connection, whatever it is called now");
  assert.equal(index.status("default", "warehouse_again").indexedAt, kept.indexedAt);
  await second.settle();
  assert.equal(second.reads("warehouse_again"), 0);
  assert.match(said.at(-1), /using the index of /);

  // A file that is not an index is ignored, and replaced.
  fs.writeFileSync(file, "{ not json");
  index = new CatalogIndex({ storageDir: storage, isForeign: (d) => d.db === "warehouse_again", connections: () => remembered });
  await second.settle();
  assert.equal(second.reads("warehouse_again"), 1);
  assert.equal(JSON.parse(fs.readFileSync(file, "utf8")).version, 1);

  // Detached but still remembered: kept. Forgotten: deleted.
  fs.writeFileSync(path.join(storage, CACHE_DIR, "notes.txt"), "not the index's");
  await second.user(`DETACH warehouse_again`, "detach");
  await second.settle();
  assert.ok(fs.existsSync(file));
  remembered = [];
  index.touch("default");
  await second.settle();
  assert.equal(fs.existsSync(file), false);
  assert.deepEqual(fs.readdirSync(path.join(storage, CACHE_DIR)), ["notes.txt"], "only its own files are deleted");
});

live("a database over the limit is the only one left out, and says so", async (t) => {
  const dir = tmp("cap");
  const storage = tmp("storage");
  const said = [];
  const index = new CatalogIndex({ storageDir: storage, maxColumns: 4, isForeign: (d) => d.db === "wh", log: (l) => said.push(l) });
  const h = await harness(t, () => index);
  await h.user(`ATTACH '${path.join(dir, "wh.duckdb")}' AS wh`, "attach");
  await h.user(`ATTACH '${path.join(dir, "wide.duckdb")}' AS wide`, "attach");
  await h.user(`CREATE TABLE wh.a (c1 INT, c2 INT, c3 INT)`);
  await h.user(`CREATE TABLE wh.b (c1 INT, c2 INT)`);
  await h.user(`CREATE TABLE wide.w (c1 INT, c2 INT, c3 INT, c4 INT, c5 INT)`);
  await h.user(`CREATE TABLE small (c1 INT, c2 INT, c3 INT, c4 INT)`);
  await h.settle();
  // The foreign one and the local one that are too large: no part, a
  // status that says why, one line in the log each. The rest: as ever.
  assert.deepEqual(index.parts("default").map((p) => p.database), ["memory"]);
  assert.equal(index.status("default", "wh").over, 5);
  assert.equal(index.status("default", "wh").limit, 4);
  assert.equal(index.status("default", "wide").over, 5);
  assert.equal(index.status("default", "memory").over, null);
  const lines = said.filter((l) => /over the limit/.test(l));
  assert.deepEqual(lines.map((l) => l.split(" ")[0]).sort(), ["wh", "wide"]);
  assert.match(lines[0], /has 5 columns, over the limit of 4 for one database: it is left out of completion and the CAT checks\. The other databases are not affected\./);
  assert.equal(h.reads("wh"), 0, "counted, not read");
  assert.equal(fs.existsSync(path.join(storage, CACHE_DIR)), false, "and not kept");
  // Said once, not after every run.
  await h.user(`CREATE TEMP TABLE small2 (c1 INT)`);
  await h.settle();
  assert.equal(said.filter((l) => /over the limit/.test(l)).length, 2, said.join('\n'));
  // Back under the limit: it is indexed.
  await h.user(`DROP TABLE wh.a`, "drop");
  await h.settle();
  assert.deepEqual(h.part("wh").tables.map((x) => x.table), ["b"]);
  assert.equal(index.status("default", "wh").over, null);
});

// ------------------------------------------------ not in the user's way --

const interruptible = CLI && fs.existsSync(CLI) && process.platform !== "win32" ? test : test.skip;

interruptible("a run does not wait for the index: its query is interrupted and tried again later", async (t) => {
  let busy = false;
  const index = new CatalogIndex({ isForeign: (d) => d.db === "wh", isBusy: () => busy });
  const h = await harness(t, () => index);
  await h.user(`ATTACH ':memory:' AS wh`, "attach");
  await h.user(`CREATE TABLE wh.facts (id INTEGER)`);
  await index.refresh("default", h.sess);

  // While a run is in progress nothing is started.
  busy = true;
  assert.equal(await index.indexPending("default", h.sess), false);
  assert.equal(h.reads("wh"), 0);
  busy = false;

  // A slow index query (the first thing the index asks is made slow), and
  // a run arriving in the middle of it.
  const run = h.sess.run;
  let slowed = 0;
  h.sess.run = (sql, opts) => (opts && opts.background && slowed++ === 0 ? run("SELECT count(*) FROM range(100000000000) r(i) WHERE i % 7 = 3", opts) : run(sql, opts));
  const pass = index.indexPending("default", h.sess);
  await new Promise((r) => setTimeout(r, 150));
  assert.equal(h.sess.busy, true, "the index query is in flight");
  const t0 = Date.now();
  assert.equal(h.sess.giveWay(), true);
  const r = await h.sess.run("SELECT 42 AS answer");
  assert.deepEqual(r.rows, [[42]]);
  assert.ok(Date.now() - t0 < 2000, `the run waited ${Date.now() - t0} ms`);
  assert.equal(await pass, false, "the pass stopped, to be taken up again");
  assert.equal(h.part("wh"), undefined);
  assert.equal(h.sess.alive, true, "and the session is unharmed");

  // After the run: indexed.
  await h.settle();
  assert.deepEqual(h.part("wh").tables.map((x) => x.table), ["facts"]);

  // The session ending under a pass ends the pass, quietly.
  index.reindex("default", "wh");
  slowed = 0;
  const doomed = index.indexPending("default", h.sess);
  await new Promise((r) => setTimeout(r, 100));
  h.sess.dispose();
  assert.equal(await doomed, false);
});

// ------------------------------------------------------------- SQLite --

live("SQLite: a foreign database by its type, read once and again on DDL", async (t) => {
  const dir = tmp("sqlite");
  const storage = tmp("storage");
  const file = path.join(dir, "app.sqlite");
  const said = [];
  const index = new CatalogIndex({ storageDir: storage, log: (l) => said.push(l) });
  const h = await harness(t, () => index);
  // The extension is not part of the CLI: it has to be installed already,
  // or installable from here.
  let loaded = await h.sess.run("LOAD sqlite");
  if (loaded.kind === "error") {
    await h.sess.run("INSTALL sqlite");
    loaded = await h.sess.run("LOAD sqlite");
  }
  if (loaded.kind === "error") return t.skip(`DuckDB's sqlite extension is not available: ${loaded.message}`);

  await h.user(`ATTACH '${file}' AS app (TYPE sqlite)`, "attach");
  await h.user(`CREATE TABLE app.notes (id INTEGER PRIMARY KEY, body VARCHAR NOT NULL, noted TIMESTAMP)`);
  await h.user(`CREATE TABLE here (a INTEGER)`);
  await h.settle();
  assert.equal(index.status("default", "app").foreign, true, "by its type, with nothing said");
  assert.equal(h.reads("app"), 1);
  const notes = tableOf(h.part("app"), "notes");
  assert.deepEqual([notes.schema, notes.columns], ["main", ["id", "body", "noted"]]);
  assert.equal(notes.types.length, 3);
  assert.match(said.at(-1), /^indexed app \(sqlite, app\.sqlite\): 1 tables, 3 columns/);
  assert.ok(fs.existsSync(path.join(storage, CACHE_DIR, `${cacheKey("sqlite", file)}.json`)));

  await h.user(`SELECT * FROM app.notes`, "select");
  await h.user(`CREATE TABLE here_too (a INTEGER)`);
  await h.user(`INSERT INTO app.notes VALUES (1, 'x', NULL)`, "insert");
  await h.settle();
  assert.equal(h.reads("app"), 1);
  await h.user(`CREATE TABLE app.tags (name VARCHAR)`);
  await h.settle();
  assert.equal(h.reads("app"), 2);
  assert.deepEqual(h.part("app").tables.map((x) => x.table).sort(), ["notes", "tags"]);
});

// --------------------------------------------------------- PostgreSQL --

livePg("PostgreSQL: indexed once without its system schemas, with keys, indexes and row estimates from the server", async (t) => {
  const storage = tmp("storage");
  const schema = `grebe_index_${process.pid}`;
  const said = [];
  const index = new CatalogIndex({ storageDir: storage, log: (l) => said.push(l) });
  const h = await harness(t, () => index);
  let loaded = await h.sess.run("LOAD postgres");
  if (loaded.kind === "error") {
    await h.sess.run("INSTALL postgres");
    loaded = await h.sess.run("LOAD postgres");
  }
  assert.notEqual(loaded.kind, "error", loaded.message);
  await h.user(`ATTACH '${PG}' AS pg (TYPE postgres)`, "attach");
  const server = (sql) => h.sess.run(`CALL postgres_execute('pg', '${sql.replace(/'/g, "''")}')`);
  // Tidied up through a session of its own: this test's one is gone by then.
  t.after(async () => {
    const s = new Session({ cli: CLI, database: ":memory:", cwd: os.tmpdir() });
    await s.start();
    await s.run(`ATTACH '${PG}' AS pg (TYPE postgres)`);
    await s.run(`CALL postgres_execute('pg', 'DROP SCHEMA IF EXISTS ${schema} CASCADE')`);
    s.dispose();
  });
  for (const sql of [
    `DROP SCHEMA IF EXISTS ${schema} CASCADE`,
    `CREATE SCHEMA ${schema}`,
    `CREATE TABLE ${schema}.orders (id int PRIMARY KEY, code text NOT NULL, customer int, "Placed At" timestamptz, UNIQUE (code, customer))`,
    `CREATE INDEX orders_customer ON ${schema}.orders (customer, "Placed At")`,
    `CREATE INDEX orders_lower ON ${schema}.orders (lower(code))`,
    `CREATE UNIQUE INDEX orders_code ON ${schema}.orders (code) INCLUDE (customer)`,
    `CREATE INDEX orders_partial ON ${schema}.orders (customer) WHERE customer > 10`,
    `INSERT INTO ${schema}.orders SELECT i, i::text, i % 50, now() FROM generate_series(1, 3000) i`,
    `ANALYZE ${schema}.orders`,
    `CREATE TABLE ${schema}.never_analyzed (x int)`,
    `CREATE VIEW ${schema}.big AS SELECT * FROM ${schema}.orders WHERE id > 100`,
  ]) {
    const r = await server(sql);
    assert.notEqual(r.kind, "error", `${sql}: ${r.message}`);
  }
  // DuckDB read the schema when it attached; what was made since on the
  // server it only sees once asked to look again, as Refresh does.
  await index.refresh("default", h.sess);
  index.reindex("default", "pg");
  await h.settle();

  const part = h.part("pg");
  assert.ok(part, said.join("\n"));
  const schemas = new Set(part.tables.map((x) => x.schema));
  assert.ok(schemas.has(schema));
  assert.ok(!schemas.has("pg_catalog") && !schemas.has("information_schema"), [...schemas].join(", "));
  const mine = (name) => part.tables.find((x) => x.schema === schema && x.table === name);
  assert.deepEqual(mine("orders"), {
    schema,
    table: "orders",
    rows: 3000,
    primary_key: ["id"],
    unique: [["code"], ["code", "customer"]],
    indexes: [["customer", "Placed At"], ["lower(code)"]],
    columns: ["id", "code", "customer", "Placed At"],
    types: mine("orders").types,
    nullable: [false, false, true, true],
  });
  assert.deepEqual(mine("orders").types.slice(0, 3), ["INTEGER", "VARCHAR", "INTEGER"]);
  assert.equal(mine("big").kind, "view");
  assert.deepEqual(mine("big").columns, ["id", "code", "customer", "Placed At"]);
  assert.equal("rows" in mine("never_analyzed"), false, "no estimate until PostgreSQL has one");
  assert.equal(index.status("default", "pg").foreign, true);
  assert.match(said.at(-1), /^indexed pg \(postgres, [^)]*\): [\d,]+ tables, [\d,]+ columns in [\d.]+ s$/);

  // Read once: queries and unrelated statements ask nothing of it.
  const reads = h.reads("pg");
  const quiet = h.asked.length;
  await h.user(`SELECT count(*) FROM pg.${schema}.orders`, "select");
  await h.settle();
  assert.equal(h.asked.length, quiet);
  await h.user(`CREATE TABLE local_t (a INTEGER)`);
  await h.settle();
  assert.equal(h.reads("pg"), reads);

  // DDL through DuckDB, and DDL handed to the server: both are seen.
  await h.user(`CREATE TABLE pg.${schema}.made_here (a INTEGER)`);
  await h.settle();
  assert.equal(h.reads("pg"), reads + 1);
  assert.ok(h.part("pg").tables.some((x) => x.schema === schema && x.table === "made_here"));
  await h.user(`CALL postgres_execute('pg', 'CREATE TABLE ${schema}.made_there (a int)')`, "call");
  await h.settle();
  assert.equal(h.reads("pg"), reads + 2);
  assert.ok(h.part("pg").tables.some((x) => x.schema === schema && x.table === "made_there"), "DuckDB's copy of the schema was thrown away first");

  // What is kept: not the password, not the connection string.
  const files = fs.readdirSync(path.join(storage, CACHE_DIR));
  assert.deepEqual(files, [`${cacheKey("postgres", PG)}.json`]);
  const text = fs.readFileSync(path.join(storage, CACHE_DIR, files[0]), "utf8");
  const password = /password\s*=\s*('(?:[^'\\]|\\.)*'|\S+)/i.exec(PG);
  if (password) assert.ok(!text.includes(password[1].replace(/^'|'$/g, "")), "the password is not in the cache");
  assert.ok(!text.includes("password"), "nor the word");
  assert.ok(!said.join("\n").includes(password ? password[1] : "\u0000"), "nor in the log");
  assert.equal(JSON.parse(text).type, "postgres");

  // The server refusing the extra questions costs only what they add.
  const run = h.sess.run;
  h.sess.run = (sql, opts) => run(sql.includes("postgres_query(") ? sql.replace("pg_catalog.pg_namespace", "pg_catalog.no_such_table") : sql, opts);
  index.reindex("default", "pg");
  await h.settle();
  const bare = h.part("pg").tables.find((x) => x.schema === schema && x.table === "orders");
  assert.deepEqual(bare.columns, ["id", "code", "customer", "Placed At"]);
  assert.deepEqual(bare.nullable, [false, false, true, true], "DuckDB itself knows this much");
  assert.equal("primary_key" in bare || "rows" in bare, false);
  assert.match(said.at(-1), /keys, indexes or row counts could not be read: .*no_such_table/);
});
