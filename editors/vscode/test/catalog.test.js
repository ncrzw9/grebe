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
const { Session } = require("../duckdb-session");
const CLI = process.env.DUCKDB_CLI;

(CLI && fs.existsSync(CLI) ? test : test.skip)("Detach keeps the connection in the Catalog, and Attach Again brings it back", async (t) => {
  t.after(() => Session.disposeAll());
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-catalog-detach-"));
  const m = new Sessions({});
  const sess = await m.open({ cli: CLI, database: ":memory:", cwd: dir });
  assert.equal((await sess.run(`ATTACH '${path.join(dir, "side.duckdb")}' AS side`)).kind, "ok");
  assert.equal((await sess.run("CREATE TABLE side.t AS SELECT 1 AS i")).kind, "ok");
  await m.remember();
  const ex = new Explorer({
    liveSession: () => m.live(),
    liveSessions: () => [],
    settings: () => ({ database: ":memory:" }),
    currentSession: async () => sess,
    cli: () => CLI,
    remember: (name) => m.remember(name),
    remembered: () => m.remembered(),
    forget: (alias) => m.forget("default", alias),
    attachAgain: (alias) => m.attach("default", alias),
  });
  const under = async () => {
    const roots = await ex.getChildren();
    return ex.getChildren(roots.find((r) => r.source === "session"));
  };
  const side = (await under()).find((n) => n.item.label === "side");
  assert.equal(side.contextValue, "grebe.attachedDb");

  await ex.detach(side);
  const waiting = (await under()).find((n) => n.item.label === "side");
  assert.equal(waiting.contextValue, "grebe.detachedDb");
  assert.match(waiting.item.description, /^detached · side\.duckdb$/);
  // Nothing holds the file now: another process can write it.
  const other = new Session({ cli: CLI, database: path.join(dir, "side.duckdb"), cwd: dir });
  await other.start();
  assert.equal((await other.run("INSERT INTO t VALUES (2)")).kind, "ok");
  other.dispose();
  await new Promise((r) => setTimeout(r, 300));

  await ex.reattach(waiting);
  assert.equal((await under()).find((n) => n.item.label === "side").contextValue, "grebe.attachedDb");
  assert.equal(Number((await sess.run("SELECT count(*) FROM side.t")).rows[0][0]), 2);

  await ex.detach((await under()).find((n) => n.item.label === "side"));
  await ex.forget((await under()).find((n) => n.item.label === "side"));
  assert.equal((await under()).some((n) => n.item.label === "side"), false);
});
