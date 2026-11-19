// The Catalog's tree drawn from the catalog index (catalog.js,
// catalogIndex.js, catalogSync.js) against a real `duckdb` CLI
// ($DUCKDB_CLI): what the tree asks of the session and when, and that it
// shows the same as it does without the index. The `vscode` API is a
// stand-in and there is no language server: the index is kept without one.
// catalog.test.js has the tests that need no duckdb.
//
// "Foreign" is tested as in catalog-index.test.js: a DuckDB file the test
// declares foreign, a SQLite file when DuckDB's sqlite extension can be
// loaded, and a PostgreSQL server when $PG_TEST_CONN names one.

"use strict";

const assert = require("node:assert/strict");
const Module = require("node:module");
const { test } = require("node:test");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

// --------------------------------------------------------- the stand-ins --

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
const settings = {}; // grebe.duckdb.* as a test sets them
const commands = new Map(); // the commands the Catalog registered
const copied = []; // what was put on the clipboard
const grid = []; // what was shown in the results grid
const nothing = () => ({ dispose() {} });
const vscode = {
  EventEmitter,
  TreeItem: class { constructor(label, state) { this.label = label; this.collapsibleState = state; } },
  TreeItemCollapsibleState: { None: 0, Collapsed: 1, Expanded: 2 },
  ThemeIcon: class { constructor(id) { this.id = id; } },
  window: {
    createOutputChannel: () => ({ appendLine() {}, show() {}, dispose() {} }),
    createTreeView: nothing,
    setStatusBarMessage() {},
    showErrorMessage: async (m) => assert.fail(m),
  },
  workspace: {
    isTrusted: true,
    textDocuments: [],
    getConfiguration: () => ({ get: (k, d) => (k in settings ? settings[k] : d) }),
    onDidChangeConfiguration: nothing,
  },
  commands: {
    registerCommand: (name, fn) => {
      commands.set(name, fn);
      return { dispose() {} };
    },
  },
  env: { clipboard: { writeText: async (text) => copied.push(text) } },
};
const results = { begin() {}, running() {}, add: (entry) => grid.push(entry), end() {} };
const realLoad = Module._load;
Module._load = function (request, ...rest) {
  if (request === "vscode") return vscode;
  if (request === "./results") return results;
  return realLoad.call(this, request, ...rest);
};
const catalog = require("../catalog");
const { CatalogIndex, CACHE_DIR } = require("../catalogIndex");
const { CatalogSync } = require("../catalogSync");
const { Session } = require("../duckdb-session");
const conninfo = require("../conninfo");

const CLI = process.env.DUCKDB_CLI;
const live = CLI && fs.existsSync(CLI) ? test : test.skip;
const PG = process.env.PG_TEST_CONN;
const livePg = CLI && PG && fs.existsSync(CLI) ? test : test.skip;

// Where DuckDB will say the folder is: on macOS the temporary folder is
// behind a link, and Windows may give it by its short name.
const tmp = (name) => fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), `grebe-tree-${name}-`)));

// A question about what a database holds, as against one about the
// session (its memory, variables, secrets and extensions).
const CATALOG = /duckdb_(tables|views|columns|functions|sequences|types|databases|constraints|indexes)\(\)|postgres_query/;

/**
 * A session with the Catalog over it, set up as the extension sets it up
 * (catalog.activate, a CatalogSync over a CatalogIndex), and the things
 * VS Code and the user would do to it.
 */
async function open(t, { dir = tmp("work"), storage = null, isForeign, maxColumns } = {}) {
  const sess = new Session({ cli: CLI, database: ":memory:", cwd: dir });
  await sess.start();
  const asked = []; // what the tree and the index asked: { sql, background }
  const run = sess.run.bind(sess);
  sess.run = (sql, opts) => {
    asked.push({ sql, background: Boolean(opts && opts.background) });
    return run(sql, opts);
  };
  let running = false;
  let drawn = 0;
  let explorer = null;
  const ended = [];
  const said = [];
  const index = new CatalogIndex({
    storageDir: storage,
    isForeign,
    maxColumns,
    isBusy: () => running,
    onChange: () => explorer.redraw(),
    onRead: () => explorer.redraw(),
    log: (line) => said.push(line),
  });
  const sync = new CatalogSync({
    clientReady: async () => null,
    liveSessions: () => [{ name: "default", session: sess }],
    sessionNameOf: () => "default",
    isRunning: () => running,
    index,
  });
  // The test says when the index is brought up to date, not a timer.
  sync.schedule = () => {};
  explorer = catalog.activate({ subscriptions: [] }, {
    currentSession: async () => sess,
    liveSession: () => sess,
    liveSessions: () => [{ name: "default", session: sess }],
    sessionName: () => "default",
    settings: () => ({ cli: CLI, database: ":memory:" }),
    cli: () => CLI,
    indexStatus: (name, db) => sync.status(name, db),
    indexView: (name, s) => sync.view(name, s),
    isRunning: () => running,
    reindex: (name, db) => sync.reindex(name, db),
    touched: (name) => sync.touched(name || "default"),
    onDidRun: (fn) => {
      ended.push(fn);
      return { dispose() {} };
    },
  });
  explorer.onDidChangeTreeData(() => drawn++);
  t.after(() => {
    explorer.dispose();
    sync.dispose();
    sess.dispose();
  });

  const line = (n) =>
    `${n.item.label}${n.item.description ? ` (${n.item.description})` : ""}${n.item.contextValue ? ` <${n.item.contextValue}>` : ""}` +
    `${n.item.collapsibleState === 1 ? " +" : ""}`;
  const h = {
    sess,
    index,
    sync,
    explorer,
    asked,
    said,
    line,
    setRunning: (yes) => (running = yes),
    drawn: () => drawn,
    /** Whether the tree was told to draw again since it had been `before`
     *  times (told in a row, it is told once, a moment later). */
    async redrawn(before) {
      for (let i = 0; i < 20 && drawn === before; i++) await new Promise((r) => setTimeout(r, 20));
      return drawn > before;
    },
    /** Everything asked has been answered. */
    idle: () => run("SELECT 1"),
    /** Run `sql` as a user's run does: the index and the tree are told. */
    async user(sql, kind = "create") {
      const r = await run(sql);
      assert.notEqual(r.kind, "error", `${sql}: ${r.message}`);
      sync.ran("default", [{ text: sql, kind }]);
      for (const fn of ended) fn({ session: "default", statements: [{ text: sql, kind }] });
      return r;
    },
    /** Run `sql` without anyone being told. */
    async quiet(sql) {
      const r = await run(sql);
      assert.notEqual(r.kind, "error", `${sql}: ${r.message}`);
      return r;
    },
    /** What a sync does, to its end: read what is cheap, index what is pending. */
    async settle() {
      await sync.sync();
      assert.equal(await index.indexPending("default", sess), true, "nothing left to index");
    },
    /** What was asked while `fn` ran, the memory figure left out. */
    async during(fn) {
      await h.idle();
      const from = asked.length;
      await fn();
      await h.idle();
      return asked.slice(from).filter((q) => !q.sql.includes("pragma_database_size"));
    },
    /** What is under the session's line. */
    async top() {
      const [root] = await explorer.getChildren();
      return explorer.getChildren(root);
    },
    /** The node reached from the session's line by `labels`. */
    async find(...labels) {
      let nodes = await h.top();
      let n = null;
      for (const label of labels) {
        n = nodes.find((x) => x.item.label === label);
        assert.ok(n, `${label} is among ${nodes.map((x) => x.item.label).join(", ")}`);
        nodes = await explorer.getChildren(n);
      }
      return n;
    },
    /**
     * The tree as lines, drawn as VS Code draws it: what is open is asked
     * for its children. `opened` names what the user opened beside (by
     * label), or is "all".
     */
    async draw(opened = []) {
      const out = [];
      const walk = async (nodes, depth) => {
        for (const n of nodes) {
          out.push("  ".repeat(depth) + line(n));
          const state = n.item.collapsibleState;
          if (state === 2 || (state === 1 && (opened === "all" || opened.includes(n.item.label)))) await walk(await explorer.getChildren(n), depth + 1);
        }
      };
      await walk(await h.top(), 0);
      return out;
    },
  };
  return h;
}

const catalogOnly = (asked) => asked.filter((q) => CATALOG.test(q.sql)).map((q) => q.sql);

// ------------------------------------------------------- local databases --

live("local databases look the same from the index as asked of the session, and cost the tree nothing", async (t) => {
  t.after(() => delete settings.catalogIndex);
  const dir = tmp("local");
  const h = await open(t, { dir });

  // Until the index has read the session, the tree says so and asks for
  // nothing of its own.
  assert.deepEqual(await h.during(async () => assert.deepEqual(await h.draw(), ["Reading the catalog…"])), []);

  // A session with nothing in it, then with one table: drawn directly.
  const both = async () => {
    await h.settle();
    const indexed = await h.draw("all");
    settings.catalogIndex = false;
    const asked = await h.draw("all");
    delete settings.catalogIndex;
    assert.deepEqual(indexed, asked);
    return indexed;
  };
  assert.deepEqual((await both()).slice(0, 2).map((l) => l.replace(/\d+ loaded/, "N loaded")), ["No tables yet", "Extensions (N loaded) +"]);
  await h.user("CREATE TABLE people (id INTEGER PRIMARY KEY, name VARCHAR NOT NULL, born DATE)");
  assert.deepEqual((await both()).slice(0, 4), ["people (~0 rows · 3 cols) <grebe.table> +", "  id (INTEGER NOT NULL) <grebe.column>", "  name (VARCHAR NOT NULL) <grebe.column>", "  born (DATE) <grebe.column>"]);

  // Everything the tree shows of a session.
  for (const sql of [
    "CREATE SCHEMA staging",
    "CREATE TABLE staging.orders AS SELECT range AS id, 'c' || range AS name FROM range(100)",
    "CREATE VIEW v_orders AS SELECT id FROM staging.orders",
    "CREATE TABLE \"Mixed Case\" (\"a b\" INTEGER)",
    "CREATE TEMP TABLE scratch AS SELECT 1 AS a",
    "CREATE TEMP VIEW scratch_v AS SELECT * FROM scratch",
    "CREATE MACRO add1(x) AS x + 1",
    "CREATE MACRO staging.recent(n) AS TABLE SELECT * FROM staging.orders ORDER BY id DESC LIMIT n",
    "CREATE SCHEMA only_macros",
    "CREATE MACRO only_macros.twice(x) AS x * 2",
    "CREATE SEQUENCE order_ids",
    "CREATE TYPE mood AS ENUM ('ok', 'bad')",
    "SET VARIABLE cutoff = DATE '2024-01-01'",
    "ATTACH ':memory:' AS scratchpad",
    "CREATE TABLE scratchpad.notes (body VARCHAR)",
    "ATTACH ':memory:' AS empty",
    `ATTACH '${path.join(dir, "side.duckdb")}' AS side`,
    "CREATE TABLE side.facts AS SELECT range AS id FROM range(1200)",
  ]) {
    await h.user(sql);
  }
  const all = await both();
  for (const expected of [
    "memory (in-memory) <grebe.db>",
    "  main (6) +",
    "    people (~0 rows · 3 cols) <grebe.table> +",
    "    v_orders (1 col) <grebe.view> +",
    "    Macros (1) +",
    "      add1(x) (macro) <grebe.macro>",
    "    Sequences (1) +",
    "      mood (ENUM) <grebe.type>",
    "  staging (2) +",
    "    orders (~100 rows · 2 cols) <grebe.table> +",
    "      recent(n) (table macro) <grebe.macro>",
    "  only_macros (1) +",
    "empty (attached · in-memory) <grebe.memoryDb>",
    "  No tables yet",
    "scratchpad (attached · in-memory) <grebe.memoryDb>",
    "  notes (~0 rows · 1 col) <grebe.table> +",
    "side (attached · side.duckdb) <grebe.unsavedDb>",
    "  facts (~1,200 rows · 1 col) <grebe.table> +",
    "temp (TEMP objects) <grebe.db>",
    "  scratch (~1 row · 1 col · temp) <grebe.table> +",
    "  scratch_v (1 col · temp) <grebe.view> +",
    "Variables (1) +",
    "  cutoff (2024-01-01 · DATE)",
  ]) {
    assert.ok(all.includes(expected), `${expected}\nis not among\n${all.join("\n")}`);
  }

  // From the index the tree asked nothing about a database; asked of the
  // session, it reads them all every time it is drawn.
  h.explorer.refresh();
  assert.deepEqual(catalogOnly(await h.during(() => h.draw("all"))), []);
  settings.catalogIndex = false;
  assert.ok(catalogOnly(await h.during(() => h.draw())).length >= 3);
  delete settings.catalogIndex;

  // A run changes something: the index reads it, says so, and the tree
  // is drawn again with it. The tree itself asks about the session only.
  const before = h.drawn();
  const tree = await h.during(async () => {
    await h.user("INSERT INTO people VALUES (1, 'Ada', NULL), (2, 'Grace', NULL)", "insert");
    await h.draw();
  });
  assert.deepEqual(tree.map((q) => q.sql.match(/duckdb_\w+/)[0]), ["duckdb_variables", "duckdb_secrets", "duckdb_extensions"]);
  await h.settle();
  assert.ok(await h.redrawn(before), "the tree was told to draw again");
  assert.ok((await h.draw(["main"])).includes("    people (~2 rows · 3 cols) <grebe.table> +"));
});

live("a database too large for the index is asked of the session when it is opened", async (t) => {
  const h = await open(t, { maxColumns: 4 });
  await h.user("CREATE TABLE small (a INTEGER)");
  await h.user("ATTACH ':memory:' AS wide");
  await h.user("CREATE TABLE wide.t (a INTEGER, b INTEGER, c INTEGER, d INTEGER, e INTEGER NOT NULL)");
  await h.user("CREATE SEQUENCE wide.s");
  await h.settle();
  const asked = await h.during(async () => {
    assert.deepEqual(await h.draw(["t", "Sequences"]), [
      "memory (in-memory) <grebe.db>",
      "  small (~0 rows · 1 col) <grebe.table> +",
      "wide (attached · in-memory · not in the editor: too large) <grebe.memoryDb>",
      "  t (~0 rows · 5 cols) <grebe.table> +",
      "    a (INTEGER) <grebe.column>",
      "    b (INTEGER) <grebe.column>",
      "    c (INTEGER) <grebe.column>",
      "    d (INTEGER) <grebe.column>",
      "    e (INTEGER NOT NULL) <grebe.column>",
      "  Sequences (1) +",
      "    s <grebe.sequence>",
      (await h.draw()).at(-1),
    ]);
  });
  const about = catalogOnly(asked);
  assert.equal(about.length, 3, "its tables and views, its macros and the rest, one table's columns");
  assert.ok(about.every((sql) => sql.includes("'wide'") && !sql.includes("'memory'")), about.join("\n"));
});

// ------------------------------------------------------ foreign databases --

live("a foreign database is drawn from the index: lazily, with no question about it, and its nodes work", async (t) => {
  const dir = tmp("foreign");
  const storage = tmp("storage");
  const file = path.join(dir, "wh.duckdb");
  const h = await open(t, { dir, storage, isForeign: (d) => d.db === "wh" });
  await h.quiet(`ATTACH '${file}' AS wh`);
  await h.quiet("CREATE TABLE wh.facts (id INTEGER PRIMARY KEY, code VARCHAR NOT NULL, n INTEGER)");
  await h.quiet("INSERT INTO wh.facts SELECT i, i::VARCHAR, i FROM range(40) r(i)");
  await h.quiet("CREATE TABLE wh.dim (id INTEGER)");
  await h.quiet("CREATE VIEW wh.recent AS SELECT * FROM wh.facts");
  await h.quiet("CREATE TABLE here (a INTEGER)");

  // Seen, not indexed yet: a line that says so, closed, and when it is
  // opened a placeholder. Nothing is asked about it meanwhile.
  await h.index.refresh("default", h.sess);
  let asked = await h.during(async () => {
    assert.deepEqual(await h.draw(["wh"]), [
      "memory (in-memory) <grebe.db>",
      "  here (~0 rows · 1 col) <grebe.table> +",
      "wh (attached · wh.duckdb · not indexed yet) <grebe.unsavedDb> +",
      "  indexing…",
      (await h.draw()).at(-1),
    ]);
  });
  assert.deepEqual(catalogOnly(asked), []);

  // Indexed: the tree is told, and draws it from the index.
  const told = h.drawn();
  await h.settle();
  assert.ok(await h.redrawn(told));
  asked = await h.during(async () => {
    assert.deepEqual((await h.draw()).slice(0, 3), [
      "memory (in-memory) <grebe.db>",
      "  here (~0 rows · 1 col) <grebe.table> +",
      "wh (attached · wh.duckdb · indexed just now) <grebe.unsavedDb> +",
    ]);
  });
  assert.deepEqual(catalogOnly(asked), [], "closed: nothing asked");
  asked = await h.during(async () => {
    assert.deepEqual((await h.draw(["wh", "facts", "recent"])).slice(2, -1), [
      "wh (attached · wh.duckdb · indexed just now) <grebe.unsavedDb> +",
      "  dim (1 col) <grebe.table> +",
      "  facts (3 cols) <grebe.table> +",
      "    id (INTEGER NOT NULL) <grebe.column>",
      "    code (VARCHAR NOT NULL) <grebe.column>",
      "    n (INTEGER) <grebe.column>",
      "  recent (3 cols) <grebe.view> +",
      "    id (INTEGER) <grebe.column>",
      "    code (VARCHAR) <grebe.column>",
      "    n (INTEGER) <grebe.column>",
    ]);
  });
  assert.deepEqual(catalogOnly(asked), [], "opened, down to the columns: nothing asked");

  // A run of nothing but queries: neither the tree nor the index asks
  // about a database.
  asked = await h.during(async () => {
    await h.user("SELECT count(*) FROM wh.facts", "select");
    await h.draw(["wh"]);
    await h.settle();
    await h.draw(["wh"]);
  });
  assert.deepEqual(catalogOnly(asked), []);
  assert.deepEqual(asked.map((q) => q.sql.match(/duckdb_\w+/)[0]), ["duckdb_variables", "duckdb_secrets", "duckdb_extensions"], "what the session holds, once");
  assert.ok(asked.every((q) => q.background), "as background statements");

  // A local CREATE: the local databases are read again, by the index and
  // once; the foreign one is not, and the tree asks nothing.
  asked = await h.during(async () => {
    await h.user("CREATE TABLE here_too (a INTEGER)");
    await h.draw(["wh"]);
  });
  assert.deepEqual(catalogOnly(asked), [], "the tree");
  asked = await h.during(() => h.settle());
  const about = catalogOnly(asked);
  assert.equal(about.length, 4, "the index: databases, columns, tables, macros and the rest");
  assert.ok(about.slice(1).every((sql) => sql.includes("'memory'") && !sql.includes("'wh'")), about.join("\n"));
  assert.ok((await h.draw()).includes("  here_too (~0 rows · 1 col) <grebe.table> +"));

  // During a run the tree asks nothing at all, and still shows it.
  h.setRunning(true);
  h.explorer.refresh();
  await h.idle();
  const from = h.asked.length;
  assert.equal((await h.draw(["wh"])).filter((l) => l.startsWith("  ")).length, 5);
  await h.idle();
  assert.deepEqual(h.asked.slice(from), []);
  h.setRunning(false);
  h.explorer.refresh();

  // --- the commands on its nodes.
  const facts = await h.find("wh", "facts");
  assert.equal(facts.item.command.command, "grebe.catalog.preview");
  grid.length = 0;
  await commands.get("grebe.catalog.preview")(facts);
  assert.equal(grid[0].preview, 'FROM "wh"."main"."facts" LIMIT 1000');
  assert.deepEqual([grid[0].result.kind, grid[0].result.total], ["rows", 40]);
  await commands.get("grebe.catalog.count")(facts);
  assert.deepEqual(grid[1].result.data, [["40"]]);
  await commands.get("grebe.catalog.columns")(facts);
  assert.deepEqual(grid[2].result.data[0], ["id", "code", "n"]);
  await commands.get("grebe.catalog.stats")(await h.find("wh", "recent"));
  assert.equal(grid[3].result.kind, "rows");
  copied.length = 0;
  await commands.get("grebe.catalog.copyName")(facts);
  await commands.get("grebe.catalog.copyName")((await h.explorer.getChildren(facts))[1]);
  assert.deepEqual(copied, ['"wh"."main"."facts"', '"wh"."main"."facts"']);
  let inserted = null;
  vscode.window.activeTextEditor = { selection: "here", edit: (fn) => fn({ replace: (at, text) => (inserted = [at, text]) }) };
  await commands.get("grebe.catalog.insertName")(await h.find("wh", "recent"));
  assert.deepEqual(inserted, ["here", '"wh"."main"."recent"']);
  delete vscode.window.activeTextEditor;

  // Refresh Connection Index on the database, and the view's Refresh:
  // the index reads it again, and the tree follows.
  await h.quiet("CREATE TABLE wh.later (x INTEGER)");
  h.explorer.refresh();
  assert.ok(!(await h.draw(["wh"])).includes("  later (1 col) <grebe.table> +"), "not known until it is read again");
  await commands.get("grebe.catalog.reindex")(await h.find("wh"));
  await h.settle();
  assert.ok((await h.draw(["wh"])).includes("  later (1 col) <grebe.table> +"));
  await h.quiet("DROP TABLE wh.later");
  await commands.get("grebe.catalog.refresh")();
  await h.settle();
  assert.ok(!(await h.draw(["wh"])).includes("  later (1 col) <grebe.table> +"));

  // The next window: a new session and a new index over the same storage
  // draw it from what was kept, before anything has been indexed.
  await commands.get("grebe.catalog.detach")(await h.find("wh"));
  await h.settle();
  assert.ok(!(await h.draw()).some((l) => l.startsWith("wh ")));
  h.sess.dispose();
  const next = await open(t, { dir, storage, isForeign: (d) => d.db === "wh" });
  await next.quiet(`ATTACH '${file}' AS wh (READ_ONLY)`);
  await next.index.refresh("default", next.sess);
  asked = await next.during(async () => {
    assert.deepEqual((await next.draw(["wh"])).slice(2, -1), [
      "wh (attached · wh.duckdb · read-only · indexed just now) <grebe.unsavedDb> +",
      "  dim (1 col) <grebe.table> +",
      "  facts (3 cols) <grebe.table> +",
      "  recent (3 cols) <grebe.view> +",
    ]);
  });
  assert.deepEqual(catalogOnly(asked), []);
  assert.match(next.said.join("\n"), /wh \(duckdb\): using the index of /);
  assert.equal(fs.readdirSync(path.join(storage, CACHE_DIR)).length, 1);
});

live("with grebe.duckdb.catalogIndex off, a foreign database is read by the tree after every run, as before", async (t) => {
  t.after(() => delete settings.catalogIndex);
  settings.catalogIndex = false;
  const dir = tmp("off");
  const h = await open(t, { dir, isForeign: (d) => d.db === "wh" });
  await h.quiet(`ATTACH '${path.join(dir, "wh.duckdb")}' AS wh`);
  await h.quiet("CREATE TABLE wh.facts (id INTEGER PRIMARY KEY, code VARCHAR NOT NULL)");
  await h.quiet("CREATE TABLE here (a INTEGER)");
  const draw = async () => {
    const asked = await h.during(async () => {
      assert.deepEqual((await h.draw(["facts"])).slice(0, -1), [
        "memory (in-memory) <grebe.db>",
        "  here (~0 rows · 1 col) <grebe.table> +",
        "wh (attached · wh.duckdb) <grebe.unsavedDb>",
        "  facts (~0 rows · 2 cols) <grebe.table> +",
        "    id (INTEGER NOT NULL) <grebe.column>",
        "    code (VARCHAR NOT NULL) <grebe.column>",
      ]);
    });
    // Tables and views, macros and the rest, the databases: of them all.
    // Then the three questions about the session, and one table's columns.
    assert.deepEqual(asked.map((q) => q.sql.match(/duckdb_\w+/)[0]), ["duckdb_tables", "duckdb_functions", "duckdb_databases", "duckdb_variables", "duckdb_secrets", "duckdb_extensions", "duckdb_columns"]);
    assert.ok(!asked[0].sql.includes("database_name IN"), "every database");
    assert.ok(asked.every((q) => !q.background));
  };
  await draw();
  await h.user("SELECT 1", "select");
  await draw();
  assert.equal(h.sync.view("default", h.sess), null);
});

live("SQLite: tables and views from the index, a view as a view", async (t) => {
  const dir = tmp("sqlite");
  const h = await open(t, { dir, storage: tmp("storage") });
  let loaded = await h.sess.run("LOAD sqlite");
  if (loaded.kind === "error") {
    await h.sess.run("INSTALL sqlite");
    loaded = await h.sess.run("LOAD sqlite");
  }
  if (loaded.kind === "error") return t.skip(`DuckDB's sqlite extension is not available: ${loaded.message}`);
  await h.quiet(`ATTACH '${path.join(dir, "app.sqlite")}' AS app (TYPE sqlite)`);
  await h.quiet("CREATE TABLE app.notes (id INTEGER PRIMARY KEY, body VARCHAR NOT NULL)");
  // A view in SQLite is made from inside it.
  await h.quiet("USE app");
  await h.quiet("CREATE VIEW recent AS SELECT id FROM notes");
  await h.quiet("USE memory");
  await h.settle();
  const asked = await h.during(async () => {
    assert.deepEqual((await h.draw(["app", "notes"])).slice(0, -1), [
      "memory (in-memory) <grebe.db>",
      "  No tables yet",
      "app (attached · sqlite · app.sqlite · indexed just now) <grebe.unsavedDb> +",
      "  notes (2 cols) <grebe.table> +",
      "    id (BIGINT) <grebe.column>",
      "    body (VARCHAR NOT NULL) <grebe.column>",
      "  recent (0 cols) <grebe.view> +",
    ]);
  });
  assert.deepEqual(catalogOnly(asked), []);
  // The view is for the tree: the language server is told of tables with
  // their columns.
  assert.deepEqual(h.index.parts("default").find((p) => p.database === "app").tables.map((x) => x.table), ["notes"]);
});

// ------------------------------------------------------------ PostgreSQL --

livePg("PostgreSQL: the tree is drawn from the index, in folders when a schema is large", async (t) => {
  const schema = `grebe_tree_${process.pid}`;
  const h = await open(t, { storage: tmp("storage") });
  let loaded = await h.sess.run("LOAD postgres");
  if (loaded.kind === "error") {
    await h.sess.run("INSTALL postgres");
    loaded = await h.sess.run("LOAD postgres");
  }
  assert.notEqual(loaded.kind, "error", loaded.message);
  // Made and tidied up through a session of its own.
  const setup = new Session({ cli: CLI, database: ":memory:", cwd: os.tmpdir() });
  await setup.start();
  const server = async (sql) => {
    const r = await setup.run(`CALL postgres_execute('pg', '${sql.replace(/'/g, "''")}')`);
    assert.notEqual(r.kind, "error", `${sql}: ${r.message}`);
  };
  t.after(async () => {
    await setup.run(`CALL postgres_execute('pg', 'DROP SCHEMA IF EXISTS ${schema} CASCADE')`);
    setup.dispose();
  });
  assert.notEqual((await setup.run("LOAD postgres")).kind, "error");
  assert.notEqual((await setup.run(`ATTACH '${PG}' AS pg (TYPE postgres)`)).kind, "error");
  await server(`DROP SCHEMA IF EXISTS ${schema} CASCADE`);
  await server(`CREATE SCHEMA ${schema}`);
  await server(
    `DO $$ BEGIN FOR i IN 1..520 LOOP EXECUTE format('CREATE TABLE ${schema}.t_%s (id bigint PRIMARY KEY, note text)', lpad(i::text, 4, '0')); END LOOP; END $$`,
  );
  await server(`INSERT INTO ${schema}.t_0007 SELECT i, 'n' FROM generate_series(1, 3000) i`);
  await server(`ANALYZE ${schema}.t_0007`);
  await server(`CREATE VIEW ${schema}.big AS SELECT * FROM ${schema}.t_0007 WHERE id > 100`);

  await h.quiet("CREATE TABLE here (a INTEGER)");
  await h.quiet(`ATTACH '${PG}' AS pg (TYPE postgres, READ_ONLY)`);
  await h.settle();

  // Closed, with what the index says of it; nothing asked.
  const c = conninfo.parse(PG);
  const where = `${c.dbname}@${c.host}${c.port ? ":" + c.port : ""}`;
  let asked = await h.during(async () => {
    assert.deepEqual((await h.draw()).slice(0, 3), [
      "memory (in-memory) <grebe.db>",
      "  here (~0 rows · 1 col) <grebe.table> +",
      `pg (attached · postgres · ${where} · read-only · indexed just now) <grebe.unsavedDb> +`,
    ]);
  });
  assert.deepEqual(catalogOnly(asked), []);

  // Opened: its schemas, without the system ones; a large schema in
  // folders by name; a table's columns. Still nothing asked.
  asked = await h.during(async () => {
    const schemas = await h.explorer.getChildren(await h.find("pg"));
    const names = schemas.map((n) => n.item.label);
    assert.ok(names.includes(schema), names.join(", "));
    assert.ok(!names.includes("pg_catalog") && !names.includes("information_schema"), names.join(", "));
    const mine = schemas.find((n) => n.item.label === schema);
    assert.equal(h.line(mine), `${schema} (521) +`);
    const folders = await h.explorer.getChildren(mine);
    assert.deepEqual(folders.map(h.line), ["t_0001 … t_0500 (500 tables) +", "t_0501 … t_0520 (20 tables) +", "big (1 view) +"]);
    const tables = await h.explorer.getChildren(folders[0]);
    assert.equal(tables.length, 500);
    assert.deepEqual(tables.slice(5, 8).map(h.line), ["t_0006 (2 cols) <grebe.table> +", "t_0007 (~3,000 rows · 2 cols) <grebe.table> +", "t_0008 (2 cols) <grebe.table> +"]);
    assert.deepEqual((await h.explorer.getChildren(tables[6])).map(h.line), ["id (BIGINT NOT NULL) <grebe.column>", "note (VARCHAR) <grebe.column>"]);
    assert.deepEqual((await h.explorer.getChildren(folders[2])).map(h.line), ["big (2 cols) <grebe.view> +"]);
  });
  assert.deepEqual(catalogOnly(asked), []);

  // A node from the index is a table like any other.
  const table = (await h.explorer.getChildren((await h.explorer.getChildren(await h.find("pg", schema)))[0]))[6];
  grid.length = 0;
  await commands.get("grebe.catalog.count")(table);
  assert.deepEqual(grid[0].result.data, [["3000"]]);
  copied.length = 0;
  await commands.get("grebe.catalog.copyName")(table);
  assert.deepEqual(copied, [`"pg"."${schema}"."t_0007"`]);

  // Runs: of queries, nothing is asked about any database; a local
  // CREATE has the index read the local ones, not PostgreSQL.
  asked = await h.during(async () => {
    await h.user(`SELECT count(*) FROM pg.${schema}.t_0007`, "select");
    await h.draw(["pg", schema]);
    await h.settle();
    await h.draw(["pg", schema]);
  });
  assert.deepEqual(catalogOnly(asked), []);
  asked = await h.during(async () => {
    await h.user("CREATE TABLE here_too (a INTEGER)");
    await h.draw(["pg", schema]);
    await h.settle();
    await h.draw(["pg", schema]);
  });
  const about = catalogOnly(asked);
  assert.equal(about.length, 4, about.join("\n"));
  assert.ok(about.every((sql) => !sql.includes("'pg'") && !sql.includes("postgres_query")), about.join("\n"));
  assert.ok((await h.draw()).includes("  here_too (~0 rows · 1 col) <grebe.table> +"));

  // A table made on the server by someone else: there after Refresh
  // Connection Index.
  await server(`CREATE TABLE ${schema}.t_0521 (id bigint)`);
  const last = async () => (await h.explorer.getChildren(await h.find("pg", schema)))[1];
  assert.equal(h.line(await last()), "t_0501 … t_0520 (20 tables) +");
  await commands.get("grebe.catalog.reindex")(await h.find("pg"));
  await h.settle();
  assert.equal(h.line(await last()), "t_0501 … t_0521 (21 tables) +");

  // Nowhere in the tree is the password.
  const password = c.password;
  assert.ok(password, "PG_TEST_CONN carries a password");
  const everything = [...(await h.draw("all")), ...(await h.top()).map((n) => String(n.item.tooltip)), ...h.said].join("\n");
  assert.ok(!everything.includes(password));

  // Detach: gone from the tree once the index has seen it go.
  await commands.get("grebe.catalog.detach")(await h.find("pg"));
  await h.settle();
  assert.ok(!(await h.draw()).some((l) => l.startsWith("pg ")));
});
