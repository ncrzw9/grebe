// The Catalog's tree against a stand-in for the `vscode` API and a stand-in
// session, so no duckdb is needed.
const assert = require("node:assert/strict");
const Module = require("node:module");
const { test } = require("node:test");

class EventEmitter {
  constructor() {
    this.listeners = [];
    this.event = (fn) => this.listeners.push(fn);
  }
  fire(e) {
    for (const fn of this.listeners) fn(e);
  }
  dispose() {}
}
const vscode = {
  EventEmitter,
  TreeItem: class { constructor(label, state) { this.label = label; this.collapsibleState = state; } },
  TreeItemCollapsibleState: { None: 0, Collapsed: 1, Expanded: 2 },
  ThemeIcon: class { constructor(id) { this.id = id; } },
  window: { createOutputChannel: () => ({ appendLine() {}, show() {}, dispose() {} }), setStatusBarMessage() {} },
  workspace: { isTrusted: true, getConfiguration: () => ({ get: (k, d) => d }) },
};
const realLoad = Module._load;
Module._load = function (request, ...rest) {
  if (request === "vscode") return vscode;
  return realLoad.call(this, request, ...rest);
};
const { Explorer } = require("../catalog");

/** A session whose answers wait until `answer()` is called, like a duckdb
 *  that is busy with someone else's query. */
function busySession(used) {
  const waiting = [];
  return {
    alive: true,
    asked: 0,
    run() {
      this.asked++;
      return new Promise((resolve) => waiting.push(resolve));
    },
    answer() {
      for (const resolve of waiting.splice(0)) resolve({ kind: "rows", columns: ["used", "limit"], rows: [[used, "8.0 GiB"]] });
    },
  };
}

function explorer(live) {
  return new Explorer({
    liveSession: () => live,
    settings: () => ({ database: ":memory:" }),
    currentSession: async () => live,
    cli: () => "duckdb",
  });
}

test("the session root is drawn while the session is busy", async () => {
  const live = busySession("1.2 GiB");
  const ex = explorer(live);
  let redraws = 0;
  ex.onDidChangeTreeData(() => redraws++);

  // Returns at once: before, it waited for the memory figure behind the
  // busy query, and the whole Catalog stayed empty.
  const roots = await ex.roots();
  assert.equal(roots.length, 1);
  assert.equal(roots[0].item.label, "Session · :memory:");
  assert.equal(roots[0].item.description, undefined);

  live.answer();
  await new Promise((r) => setImmediate(r));
  assert.equal(redraws, 1, "the figure arriving redraws the tree");
  const again = await ex.roots();
  assert.match(again[0].item.description, /1\.2 GiB of 8\.0 GiB in use/);

  // The redraw asks again; the same answer redraws nothing more.
  live.answer();
  await new Promise((r) => setImmediate(r));
  assert.equal(redraws, 1);
});

test("one memory question at a time, however often the tree is drawn", async () => {
  const live = busySession("1 MiB");
  const ex = explorer(live);
  await ex.roots();
  await ex.roots();
  await ex.roots();
  assert.equal(live.asked, 1);
});

test("no session, no memory figure and no question", async () => {
  const ex = explorer(null);
  const roots = await ex.roots();
  assert.equal(roots[0].item.description, undefined);
});

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { Sessions } = require("../sessions");
const { normalize, toSetting } = require("../connections");
const { Session } = require("../duckdb-session");
const CLI = process.env.DUCKDB_CLI;

(CLI && fs.existsSync(CLI) ? test : test.skip)("Keep, Detach, Attach Again and Forget write the setting, and the tree follows it", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), "grebe-catalog-detach-")));
  // The setting, as the extension keeps it (connectionSettings.js): every
  // write is followed by the change event, which reconciles.
  const list = [];
  const m = new Sessions({ connections: () => normalize(list) });
  const write = async (fn) => {
    fn();
    await m.reconcile();
  };
  const find = (alias) => list.findIndex((x) => x.alias === alias);
  const sess = await m.open({ cli: CLI, database: ":memory:", cwd: dir });
  assert.equal((await sess.run(`ATTACH '${path.join(dir, "side.duckdb")}' AS side`)).kind, "ok");
  assert.equal((await sess.run("CREATE TABLE side.t AS SELECT 1 AS i")).kind, "ok");
  const ex = new Explorer({
    liveSession: () => m.live(),
    liveSessions: () => [],
    settings: () => ({ database: ":memory:" }),
    currentSession: async () => sess,
    cli: () => CLI,
    connections: (name) => m.entries(name).map((e) => ({ ...e, ...m.state(name, e.alias) })),
    attachAgain: async (alias, name) => {
      const error = await m.ensure(name, alias);
      if (!error) await write(() => delete list[find(alias)].detached);
      return error;
    },
    detach: async (alias, name) => {
      const error = await m.detach(name, alias);
      if (!error) await write(() => (list[find(alias)].detached = true));
      return error;
    },
    forget: async (alias, name) => {
      await m.detach(name, alias);
      await write(() => list.splice(find(alias), 1));
    },
    save: (e) => write(() => list.push(toSetting(e))),
  });
  const under = async () => {
    const roots = await ex.getChildren();
    return ex.getChildren(roots.find((r) => r.source === "session"));
  };
  const at = async (label) => (await under()).find((n) => n.item.label === label);

  // Attached in SQL: not saved, and offered to be kept.
  assert.equal((await at("side")).item.contextValue, "grebe.unsavedDb");
  assert.deepEqual(list, []);
  await ex.keep(await at("side"));
  assert.deepEqual(list, [{ alias: "side", path: path.join(dir, "side.duckdb") }]);
  assert.equal((await at("side")).item.contextValue, "grebe.attachedDb");

  await ex.detach(await at("side"));
  assert.deepEqual(list, [{ alias: "side", path: path.join(dir, "side.duckdb"), detached: true }]);
  const waiting = await at("side");
  assert.equal(waiting.item.contextValue, "grebe.detachedDb");
  assert.match(waiting.item.description, /^detached · side\.duckdb$/);
  // Nothing holds the file now: another process can write it.
  const other = new Session({ cli: CLI, database: path.join(dir, "side.duckdb"), cwd: dir });
  await other.start();
  assert.equal((await other.run("INSERT INTO t VALUES (2)")).kind, "ok");
  other.dispose();
  await new Promise((r) => setTimeout(r, 300));

  await ex.reattach(waiting);
  assert.deepEqual(list, [{ alias: "side", path: path.join(dir, "side.duckdb") }]);
  assert.equal((await at("side")).item.contextValue, "grebe.attachedDb");
  assert.equal(Number((await sess.run("SELECT count(*) FROM side.t")).rows[0][0]), 2);

  // Before the session starts, its connections are listed all the same: a
  // file to be attached with it, a detached one, a server not connected.
  await write(() => list.push({ alias: "second", path: path.join(dir, "second.duckdb") }, { alias: "pg", path: "dbname=x host=127.0.0.1 port=1" }));
  assert.equal((await at("second")).item.contextValue, "grebe.attachedDb", "a file added is attached at once");
  await ex.detach(await at("side"));
  let running = false;
  ex.deps.liveSession = () => (running ? m.live() : null);
  assert.deepEqual(
    (await under()).slice(1).map((n) => [n.item.label, n.item.contextValue, n.item.description, n.item.collapsibleState]),
    [
      ["side", "grebe.detachedDb", "detached · side.duckdb", 0],
      ["second", "grebe.pendingDb", "attached when the session starts · second.duckdb", 0],
      ["pg", "grebe.offlineDb", "not connected · x@127.0.0.1:1", 1],
    ],
  );
  running = true;
  // The X: out of the setting, and detached if it is attached.
  await ex.forget(await at("side"));
  await ex.forget(await at("second"));
  assert.deepEqual(list.map((x) => x.alias), ["pg"]);
  assert.equal(await at("side"), undefined);
  assert.equal(await at("second"), undefined);
  assert.equal(Number((await sess.run("SELECT count(*) FROM duckdb_databases() WHERE database_name = 'second'")).rows[0][0]), 0);
});

// ------------------------------------------- the tree from the catalog index --

/** A session that notes what it is asked and answers at once: the memory
 *  figure, one extension, and one table for a database asked for by name. */
function notingSession() {
  const rows = (columns, data) => Promise.resolve({ kind: "rows", columns, rows: data });
  return {
    alive: true,
    asked: [],
    run(sql, opts) {
      this.asked.push({ sql, background: Boolean(opts && opts.background) });
      if (sql.includes("pragma_database_size")) return rows(["used", "limit"], [["1 MiB", "8 GiB"]]);
      if (sql.includes("duckdb_extensions")) return rows(["name", "version"], [["json", "v1"]]);
      if (sql.includes("FROM duckdb_tables()")) return rows(["db", "sch", "name", "kind", "est", "cols", "tmp"], [["big", "main", "wide", "table", 5, 3, false]]);
      return rows([], []);
    },
    /** What was asked about what a database holds. */
    catalog() {
      return this.asked.filter((q) => /duckdb_(tables|views|columns|functions|sequences|types|databases)\(\)/.test(q.sql));
    },
  };
}

const status = (more = {}) => ({ foreign: false, tables: 0, columns: 0, indexedAt: null, indexing: false, stale: null, over: null, limit: 500000, error: null, ...more });
const table = (schema, name, more = {}) => ({ schema, table: name, columns: ["id", "note"], types: ["BIGINT", "VARCHAR"], nullable: [false, true], ...more });

/** What an index would hold: the session's own database, a PostgreSQL
 *  one of 1,203 tables and 3 views in one schema and 2 tables in another,
 *  one that has not been indexed yet, and a local one too large to index. */
function indexedView(sess) {
  const pad = (i) => String(i).padStart(4, "0");
  const pg = [table("audit", "logins"), table("audit", "changes", { rows: 12 })];
  for (let i = 0; i < 1203; i++) pg.push(table("public", `t${pad(i)}`));
  for (const v of ["v_c", "v_a", "v_b"]) pg.push(table("public", v, { kind: "view", rows: 0 }));
  const db = (name, more) => ({ db: name, type: "duckdb", path: null, readonly: false, current: false, foreign: false, tables: null, extras: [], status: status(), ...more });
  return {
    session: sess,
    read: true,
    dbs: [
      db("pg", {
        type: "postgres",
        path: "dbname=shop host=db.example.com password=hunter2",
        readonly: true,
        foreign: true,
        tables: pg,
        status: status({ foreign: true, tables: pg.length, columns: 2 * pg.length, indexedAt: Date.now() - 3 * 60000 }),
      }),
      db("memory", {
        current: true,
        tables: [table("main", "orders", { rows: 100 }), table("main", "recent", { kind: "view" })],
        extras: [{ db: "memory", sch: "main", name: "add1", kind: "macro", detail: "x", def: "(x + 1)" }],
      }),
      db("lake", { type: "sqlite", path: "lake.sqlite", foreign: true, status: status({ foreign: true }) }),
      db("big", { path: "big.duckdb", status: status({ over: 612345 }) }),
    ],
  };
}

function indexedExplorer(sess, more = {}) {
  return new Explorer({
    liveSession: () => sess,
    settings: () => ({ database: ":memory:" }),
    currentSession: async () => sess,
    cli: () => "duckdb",
    indexView: (name, s) => indexedView(s),
    ...more,
  });
}

const shown = (nodes) => nodes.map((n) => `${n.item.label}${n.item.description ? " (" + n.item.description + ")" : ""}`);
const settled = () => new Promise((r) => setImmediate(r));
const notMemory = (sess) => sess.asked.filter((q) => !q.sql.includes("pragma_database_size"));

test("with the index, the tree asks the session nothing about its databases", async () => {
  const sess = notingSession();
  // The three attached ones are in the setting.
  const ex = indexedExplorer(sess, { connections: () => ["big", "lake", "pg"].map((alias) => ({ alias, path: `${alias}.db`, type: "duckdb" })) });
  const [root] = await ex.getChildren();
  const top = await ex.getChildren(root);
  assert.deepEqual(shown(top), [
    "memory (in-memory)",
    "big (attached · big.duckdb · not in the editor: too large)",
    "lake (attached · sqlite · lake.sqlite · not indexed yet)",
    "pg (attached · postgres · shop@db.example.com · read-only · indexed 3 min ago)",
    "Extensions (1 loaded)",
  ]);
  const [memory, big, lake, pg] = top;
  // The session's own database is open, as it always was; a foreign one
  // starts closed.
  assert.deepEqual([memory, big, lake, pg].map((n) => n.item.collapsibleState), [2, 2, 1, 1]);
  assert.deepEqual([memory, big, lake, pg].map((n) => n.item.contextValue), ["grebe.db", "grebe.attachedDb", "grebe.attachedDb", "grebe.attachedDb"]);
  assert.match(pg.item.tooltip, /password=\*\*\*/);
  assert.match(pg.item.tooltip, /1,208 tables, 2,416 columns, read 3 min ago/);
  assert.equal(big.item.iconPath.id, "warning");

  // Its own database, from the index: rows, columns, views, macros.
  const inMemory = await ex.getChildren(memory);
  assert.deepEqual(shown(inMemory), ["orders (~100 rows · 2 cols)", "recent (2 cols)", "Macros (1)"]);
  assert.deepEqual(inMemory.slice(0, 2).map((n) => n.item.contextValue), ["grebe.table", "grebe.view"]);
  assert.deepEqual(inMemory.slice(0, 2).map((n) => n.item.tooltip), [
    `"memory"."main"."orders"\ntable\n~100 rows (estimated)\n2 columns`,
    `"memory"."main"."recent"\nview\n2 columns`,
  ]);
  assert.deepEqual(shown(await ex.getChildren(inMemory[0])), ["id (BIGINT NOT NULL)", "note (VARCHAR)"]);
  assert.deepEqual(shown(await ex.getChildren(inMemory[2])), ["add1(x) (macro)"]);

  // The PostgreSQL database: schemas, then folders of 500 by name, built
  // when they are opened.
  const schemas = await ex.getChildren(pg);
  assert.deepEqual(shown(schemas), ["audit (2)", "public (1206)"]);
  assert.deepEqual(shown(await ex.getChildren(schemas[0])), ["changes (~12 rows · 2 cols)", "logins (2 cols)"]);
  const folders = await ex.getChildren(schemas[1]);
  assert.deepEqual(shown(folders), ["t0000 … t0499 (500 tables)", "t0500 … t0999 (500 tables)", "t1000 … t1202 (203 tables)", "v_a … v_c (3 views)"]);
  assert.ok(folders.every((f) => f.item.collapsibleState === 1 && f.item.contextValue === undefined));
  const some = await ex.getChildren(folders[2]);
  assert.equal(some.length, 203);
  const one = some[7];
  assert.equal(one.item.label, "t1007");
  assert.equal(one.item.contextValue, "grebe.table");
  assert.deepEqual(one.object, { db: "pg", schema: "public", name: "t1007", kind: "table", est: null, cols: 2, tmp: false });
  assert.equal(one.item.command.command, "grebe.catalog.preview");
  assert.equal(one.item.command.arguments[0], one);
  assert.equal(one.item.tooltip, '"pg"."public"."t1007"\ntable\n2 columns', "no row estimate when there is none");
  const columns = await ex.getChildren(one);
  assert.deepEqual(shown(columns), ["id (BIGINT NOT NULL)", "note (VARCHAR)"]);
  assert.deepEqual([columns[0].item.contextValue, columns[0].object], ["grebe.column", one.object]);
  const views = await ex.getChildren(folders[3]);
  assert.deepEqual(
    views.map((n) => [n.item.label, n.item.contextValue, n.item.description]),
    [["v_a", "grebe.view", "2 cols"], ["v_b", "grebe.view", "2 cols"], ["v_c", "grebe.view", "2 cols"]],
  );

  // Not indexed yet: said, not read here as well.
  assert.deepEqual(shown(await ex.getChildren(lake)), ["indexing…"]);

  await settled();
  assert.deepEqual(sess.catalog(), [], "nothing about any database was asked");
  assert.equal(sess.asked.length, 4, "the memory figure, variables, secrets, extensions");
  assert.ok(sess.asked.every((q) => q.background), "and a run that starts does not wait for those");

  // Drawn again with nothing new: what the session holds is not asked for
  // again. (The memory figure is: it changes by itself.)
  await ex.getChildren((await ex.getChildren())[0]);
  await settled();
  assert.equal(notMemory(sess).length, 3);

  // The database the index holds no part of is read when it is opened,
  // once for each refresh, and by its name.
  assert.deepEqual(shown(await ex.getChildren(big)), ["wide (~5 rows · 3 cols)"]);
  await ex.getChildren(big);
  assert.equal(sess.catalog().length, 2, "its tables and views, its macros and the rest");
  assert.ok(sess.catalog().every((q) => q.background && q.sql.includes("database_name IN ('big')")));
  ex.refresh();
  await ex.getChildren(big);
  assert.equal(sess.catalog().length, 4);
  ex.dispose();
});

test("with the index, nothing is asked of a session while a run has it", async () => {
  const sess = notingSession();
  let running = true;
  const ex = indexedExplorer(sess, { isRunning: () => running });
  const [root] = await ex.getChildren();
  const top = await ex.getChildren(root);
  // The databases are drawn all the same: they need no answer.
  assert.deepEqual(top.map((n) => n.item.label), ["memory", "big", "lake", "pg"]);
  assert.equal((await ex.getChildren(top[3])).length, 2);
  assert.deepEqual(shown(await ex.getChildren(top[1])), ["Shown when the running statement ends"]);
  await settled();
  assert.deepEqual(sess.asked, []);

  // The run ends: the tree is refreshed, and asks then.
  running = false;
  ex.refresh();
  const after = await ex.getChildren((await ex.getChildren())[0]);
  assert.equal(shown(after).at(-1), "Extensions (1 loaded)");
  assert.deepEqual(shown(await ex.getChildren(after[1])), ["wide (~5 rows · 3 cols)"]);
  await settled();
  assert.equal(sess.asked.length, 6);
  ex.dispose();
});

test("with the index, a session that has not been read yet says so and is not asked", async () => {
  const sess = notingSession();
  const ex = indexedExplorer(sess, { indexView: () => ({ read: false }) });
  const [root] = await ex.getChildren();
  assert.deepEqual(shown(await ex.getChildren(root)), ["Reading the catalog…"]);
  await settled();
  assert.deepEqual(sess.catalog(), []);
  ex.dispose();
});

test("without the index, the tree asks the session, as it always did", async () => {
  const sess = notingSession();
  const ex = indexedExplorer(sess, { indexView: () => null });
  const [root] = await ex.getChildren();
  const top = await ex.getChildren(root);
  assert.deepEqual(shown(top), ["No tables yet", "Extensions (1 loaded)"]);
  const asked = sess.catalog().map((q) => q.sql);
  assert.equal(asked.length, 3, "tables and views, macros and the rest, databases");
  assert.ok(asked[0].includes("FROM duckdb_tables() WHERE NOT internal UNION ALL"), "of every database");
  assert.ok(notMemory(sess).every((q) => !q.background));
  ex.dispose();
});

test("refreshes in a row are drawn together", async () => {
  const sess = notingSession();
  const ex = indexedExplorer(sess);
  let drawn = 0;
  ex.onDidChangeTreeData(() => drawn++);
  ex.refresh();
  assert.equal(drawn, 1, "the first at once");
  for (let i = 0; i < 5; i++) ex.refresh();
  ex.redraw();
  assert.equal(drawn, 1);
  await new Promise((r) => setTimeout(r, 120));
  assert.equal(drawn, 2, "the rest as one");
  await new Promise((r) => setTimeout(r, 120));
  assert.equal(drawn, 2);

  // And however many times one refresh is drawn, the session is asked once.
  const [root] = await ex.getChildren();
  await Promise.all([ex.getChildren(root), ex.getChildren(root), ex.getChildren(root)]);
  await settled();
  assert.equal(notMemory(sess).length, 3);
  ex.dispose();
});
