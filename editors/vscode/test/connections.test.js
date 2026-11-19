// connections.js: the grebe.duckdb.connections setting as data -- reading
// it from its scopes, where a new entry is written, what a change to it
// asks of a running session, and which connections a statement refers to.
// No DuckDB, no VS Code.

"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");
const c = require("../connections");

const root = path.resolve("/proj");

test("normalize: well-formed entries only, no password, files against the folder, the type from a URI", () => {
  const got = c.normalize(
    [
      { alias: "sales", path: "data/sales.duckdb" },
      { alias: "wh", path: "postgresql://me:secret@db/wh", readOnly: true },
      { alias: "pg", path: "dbname=x host=h password=pw", type: "postgres", detached: true },
      { alias: "lite", path: "/abs/app.sqlite", type: "SQLite", session: "etl" },
      { alias: "dflt", path: "/abs/d.duckdb", session: "default" },
      { alias: "", path: "x" },
      { path: "x" },
      "junk",
    ],
    root,
  );
  assert.deepEqual(got, [
    { alias: "sales", path: path.join(root, "data/sales.duckdb"), type: "duckdb", readOnly: false },
    { alias: "wh", path: "postgresql://me@db/wh", type: "postgres", readOnly: true },
    { alias: "pg", path: "dbname=x host=h", type: "postgres", readOnly: false, detached: true },
    { alias: "lite", path: "/abs/app.sqlite", type: "sqlite", readOnly: false, session: "etl" },
    { alias: "dflt", path: "/abs/d.duckdb", type: "duckdb", readOnly: false },
  ]);
  assert.equal(c.isServer(got[0]), false);
  assert.equal(c.isServer(got[1]), true);
  assert.equal(c.isServer(got[3]), false, "a SQLite file is a file");
  assert.equal(c.isServer({ path: "/x", type: "mysql" }), true);
});

test("merge: every scope's entries together, the most specific one winning for the same alias", () => {
  const info = {
    globalValue: [{ alias: "a", path: "/user/a.duckdb" }, { alias: "B", path: "/user/b.duckdb" }],
    workspaceValue: [{ alias: "b", path: "/ws/b.duckdb", readOnly: true }],
    workspaceFolderValue: [{ alias: "a", path: "/folder/a.duckdb", session: "etl" }],
  };
  assert.deepEqual(
    c.merge(info, root).map((e) => [e.session || "", e.alias, e.path, e.scope]),
    [
      ["etl", "a", "/folder/a.duckdb", "workspaceFolder"],
      ["", "b", "/ws/b.duckdb", "workspace"],
      ["", "a", "/user/a.duckdb", "global"],
    ],
  );
  assert.deepEqual(c.merge(undefined, root), []);
});

test("chooseScope: the one scope that has the setting, else the user's", () => {
  assert.equal(c.chooseScope({}), "global");
  assert.equal(c.chooseScope({ globalValue: [] }), "global");
  assert.equal(c.chooseScope({ workspaceValue: [{ alias: "a", path: "/a" }] }), "workspace");
  assert.equal(c.chooseScope({ workspaceFolderValue: [] }), "workspaceFolder");
  assert.equal(c.chooseScope({ globalValue: [], workspaceValue: [] }), "global", "two scopes: the user's");
  assert.equal(c.chooseScope(undefined), "global");
});

test("scopesWith, editItem and toSetting: an entry found, changed and written in every scope that has it", () => {
  const info = {
    globalValue: [{ alias: "pg", path: "dbname=x" }, { alias: "keep", path: "/k" }],
    workspaceValue: [{ alias: "PG", path: "dbname=y" }, { not: "an entry" }],
    workspaceFolderValue: [{ alias: "pg", path: "dbname=z", session: "etl" }],
  };
  assert.deepEqual(c.scopesWith(info, "default", "pg"), ["workspace", "global"]);
  assert.deepEqual(c.scopesWith(info, "etl", "pg"), ["workspaceFolder"]);
  assert.deepEqual(c.editItem(info.workspaceValue, "default", "pg", () => null), [{ not: "an entry" }], "what is not an entry is kept");
  assert.deepEqual(c.editItem(info.globalValue, "default", "pg", (i) => ({ ...i, detached: true })), [
    { alias: "pg", path: "dbname=x", detached: true },
    { alias: "keep", path: "/k" },
  ]);
  assert.deepEqual(c.toSetting({ alias: "w", path: "postgresql://u:pw@h/db", type: "postgres", readOnly: true, detached: true, session: "etl" }), {
    alias: "w",
    path: "postgresql://u@h/db",
    readOnly: true,
    detached: true,
    session: "etl",
  });
  assert.deepEqual(c.toSetting({ alias: "s", path: "dbname=x host=h", type: "postgres", readOnly: false }), { alias: "s", path: "dbname=x host=h", type: "postgres" });
  assert.deepEqual(c.toSetting({ alias: "f", path: path.join(root, "data", "f.duckdb"), type: "duckdb" }, root), { alias: "f", path: "data/f.duckdb" });
  assert.deepEqual(c.toSetting({ alias: "f", path: "/elsewhere/f.duckdb", type: "duckdb" }, root), { alias: "f", path: "/elsewhere/f.duckdb" });
});

// What is attached, as Sessions.attachedNow gives it.
const on = (...aliases) => new Map(aliases.map((a) => [a.toLowerCase(), { alias: a }]));
const file = (alias, more = {}) => ({ alias, path: `/d/${alias}.duckdb`, type: "duckdb", readOnly: false, ...more });
const server = (alias, more = {}) => ({ alias, path: `dbname=${alias} host=h`, type: "postgres", readOnly: true, ...more });
const show = (actions) => actions.map((a) => (a.do === "detach" ? `detach ${a.alias}` : `attach ${a.entry.alias}`));

test("plan: what a change to the setting asks of a running session", () => {
  // Added: a file is attached, a server waits.
  assert.deepEqual(show(c.plan([], [file("a"), server("pg")], on())), ["attach a"]);
  // Added and attached already (the Catalog attached it, Keep): nothing.
  assert.deepEqual(show(c.plan([], [file("a")], on("a"))), []);
  // Taken out: detached if attached.
  assert.deepEqual(show(c.plan([file("a"), server("pg")], [], on("a", "pg"))), ["detach a", "detach pg"]);
  assert.deepEqual(show(c.plan([file("a")], [], on())), []);
  // Marked detached, and unmarked.
  assert.deepEqual(show(c.plan([file("a")], [file("a", { detached: true })], on("a"))), ["detach a"]);
  assert.deepEqual(show(c.plan([file("a")], [file("a", { detached: true })], on())), [], "the Catalog detached it first");
  assert.deepEqual(show(c.plan([file("a", { detached: true })], [file("a")], on())), ["attach a"]);
  assert.deepEqual(show(c.plan([file("a", { detached: true })], [file("a")], on("a"))), [], "Attach Again attached it first");
  assert.deepEqual(show(c.plan([server("pg", { detached: true })], [server("pg")], on())), [], "a server waits");
  // Now something else: detached and attached again; a server only if it
  // was attached.
  assert.deepEqual(show(c.plan([file("a")], [file("a", { readOnly: true })], on("a"))), ["detach a", "attach a"]);
  assert.deepEqual(show(c.plan([file("a")], [file("a", { path: "/d/b.duckdb" })], on())), ["attach a"]);
  assert.deepEqual(show(c.plan([server("pg")], [server("pg", { path: "dbname=other" })], on("pg"))), ["detach pg", "attach pg"]);
  assert.deepEqual(show(c.plan([server("pg")], [server("pg", { path: "dbname=other" })], on())), []);
  assert.deepEqual(show(c.plan([file("a")], [file("a", { path: "/d/b.duckdb", detached: true })], on("a"))), ["detach a"]);
  // Nothing changed: nothing, whatever is attached.
  assert.deepEqual(show(c.plan([file("a"), server("pg")], [file("a"), server("pg")], on())), []);
  // An alias in another case is the same one.
  assert.deepEqual(show(c.plan([file("A")], [file("a", { detached: true })], on("A"))), ["detach a"]);
});

test("references: `alias.` and `USE alias`, outside strings and comments, in any case", () => {
  const refs = (sql, aliases = ["pg", "wh", "x"]) => c.references(sql, aliases).sort();
  assert.deepEqual(refs("SELECT * FROM pg.public.t JOIN WH.sales s ON true"), ["pg", "wh"]);
  assert.deepEqual(refs(`SELECT * FROM "PG"."public"."t"`), ["pg"]);
  assert.deepEqual(refs("SELECT * FROM pg . public . t"), ["pg"]);
  assert.deepEqual(refs("USE wh"), ["wh"]);
  assert.deepEqual(refs("-- a note\nuse \"X\";"), ["x"]);
  assert.deepEqual(refs("SELECT 'pg.t', $$wh.t$$, $q$x.t$q$ -- pg.t\n/* wh.t */"), []);
  assert.deepEqual(refs("SELECT 'it''s pg.t'"), []);
  assert.deepEqual(refs("SELECT pg FROM t"), [], "a word, not a qualified name");
  assert.deepEqual(refs("SELECT a.pg.t FROM a"), [], "the middle of a name is not its database");
  assert.deepEqual(refs("SELECT t.x FROM t"), [], "x after the dot");
  assert.deepEqual(refs("SELECT 1 USE wh"), [], "USE only as the statement");
  assert.deepEqual(refs("FROM pg.t", []), []);
});

test("migrate: the old per-workspace store, as entries, once", () => {
  const old = {
    default: [
      { alias: "kept", path: "/d/kept.duckdb", type: "duckdb", readOnly: true },
      { alias: "off", path: "/d/off.duckdb", type: "duckdb", readOnly: false, detached: true },
      { alias: "pg", path: "dbname=x host=h", type: "postgres", readOnly: false, error: "IO Error: down" },
      { alias: "mem", path: ":memory:", type: "duckdb" },
      { alias: "broken", error: "IO Error: no such file" },
      { alias: "have", path: "/d/have.duckdb", type: "duckdb" },
    ],
    etl: [{ alias: "stage", path: "/d/stage.duckdb", type: "duckdb", readOnly: false }],
  };
  const got = c.migrate(old, [{ alias: "HAVE", path: "/elsewhere.duckdb", type: "duckdb" }]);
  assert.deepEqual(got, [
    { alias: "kept", path: "/d/kept.duckdb", type: "duckdb", readOnly: true },
    { alias: "off", path: "/d/off.duckdb", type: "duckdb", readOnly: false, detached: true },
    { alias: "pg", path: "dbname=x host=h", type: "postgres", readOnly: false },
    { alias: "stage", path: "/d/stage.duckdb", type: "duckdb", readOnly: false, session: "etl" },
  ]);
  assert.deepEqual(c.migrate({}, []), []);
  assert.deepEqual(c.migrate(null, []), []);
});
