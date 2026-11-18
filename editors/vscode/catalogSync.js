// Tell the language server what the running session holds, so the editor
// completes table and column names and marks the ones the session does not
// have (the server's CAT rules). The server embeds no database: this is the
// only place it learns of one, and without it nothing changes.
//
//   - The shared session's tables are the default catalog, for every file.
//   - A file with a session of its own (`-- grebe: session NAME`) gets that
//     session's tables for itself; while that session is not running it has
//     no catalog at all, rather than another session's.
//   - A session with no tables yet says nothing: a file that is about to
//     create them should not be marked up for using them.
//
// Sent after every run, when a session starts or ends, when a file is
// opened, and when the server restarts; what has not changed is not sent
// again.
//
// With `grebe.duckdb.catalogIndex` on (the default) the catalog is kept and
// sent database by database (catalogIndex.js): a part per database, each
// sent only when it changed, so a large attached database is read and sent
// once, and one that is too large to send is the only one left out. A
// session of its own is sent once under its name, and its files are told
// to use it. With the setting off, every database is read after every run
// and sent as one snapshot, and past MAX_COLUMNS columns nothing is sent.

"use strict";

const vscode = require("vscode");
const log = require("./log");
const { CatalogIndex } = require("./catalogIndex");

// Without the index: above this many columns the catalog is not sent. It
// would be megabytes on every run (an attached warehouse with tens of
// thousands of tables).
const MAX_COLUMNS = 100000;

const COLUMNS =
  `SELECT database_name AS "database", schema_name AS "schema", table_name AS "table", column_name AS "column", data_type AS "type" ` +
  `FROM duckdb_columns() WHERE NOT internal LIMIT ${MAX_COLUMNS + 1}`;
const FUNCTIONS =
  `SELECT DISTINCT function_name AS "name", function_type AS "kind" FROM duckdb_functions() ` +
  `WHERE NOT internal AND function_type IN ('macro', 'table_macro')`;

const objects = (r) => r.rows.map((row) => Object.fromEntries(r.columns.map((c, i) => [c, row[i]])));

/** What `sess` holds, as the server takes it: `{ columns, functions }`, or
 *  null when it holds no tables, is busy, or holds too many to send. */
async function snapshot(sess) {
  if (!sess || !sess.alive || sess.busy) return undefined; // nothing new to say
  const cols = await sess.run(COLUMNS);
  if (cols.kind !== "rows") return null;
  if (cols.rows.length === 0 || cols.rows.length > MAX_COLUMNS) return null;
  const fns = await sess.run(FUNCTIONS);
  return { columns: objects(cols), functions: fns.kind === "rows" ? objects(fns) : [] };
}

const DEFAULT = "default";

class CatalogSync {
  /**
   * @param {{ clientReady: () => Promise<object|null>,
   *           liveSessions: () => { name: string, session: object }[],
   *           sessionNameOf: (doc: object) => string,
   *           index?: CatalogIndex }} deps
   */
  constructor(deps) {
    this.deps = deps;
    // What the server was last told. Without the index: "" (default) or a
    // uri -> the JSON sent. With it: "part\0CATALOG\0DATABASE" -> the
    // part's digest, and "doc\0URI" -> the catalog the file was told to
    // use.
    this.sent = new Map();
    this.indexed = null; // which of the two ways `sent` was written in
    this.timer = null;
    this.running = Promise.resolve();
    this.index = deps.index || new CatalogIndex({ onChange: () => this.schedule() });
  }

  setting(key, fallback) {
    return vscode.workspace.getConfiguration("grebe.duckdb").get(key, fallback);
  }

  enabled() {
    return this.setting("catalogInEditor", true) !== false;
  }

  /** Sync soon: several triggers in a row (a run ending, the Catalog
   *  refreshing) become one. */
  schedule() {
    clearTimeout(this.timer);
    this.timer = setTimeout(() => this.sync(), 100);
    if (this.timer.unref) this.timer.unref();
  }

  /** The server is new (started, restarted): it knows nothing. */
  reset() {
    this.sent.clear();
    this.indexed = null;
    this.schedule();
  }

  sync() {
    this.running = this.running.then(() => this.syncNow()).catch((e) => log.error("catalog", `could not send the catalog to the language server: ${e.message ?? e}`, e));
    return this.running;
  }

  async syncNow() {
    const client = await this.deps.clientReady();
    if (!client) return;
    const indexed = this.setting("catalogIndex", true) !== false;
    if (this.indexed !== null && this.indexed !== indexed) this.forgetAll(client);
    this.indexed = indexed;
    return indexed ? this.syncParts(client) : this.syncWhole(client);
  }

  /** The other way of sending takes over: the server forgets what it was
   *  told in this one. */
  forgetAll(client) {
    const named = new Set();
    for (const key of this.sent.keys()) {
      const [kind, a] = key.split("\u0000");
      if (kind === "part" && a !== "") named.add(a);
      else if (kind === "doc") client.sendNotification("grebe/catalog", { uri: a, default: true });
      else if (kind !== "part" && key !== "") client.sendNotification("grebe/catalog", { uri: key, default: true });
    }
    for (const name of named) client.sendNotification("grebe/catalog", { catalog: name, columns: null });
    client.sendNotification("grebe/catalog", { columns: null });
    this.sent.clear();
  }

  // --- database by database -------------------------------------------------

  async syncParts(client) {
    const on = this.enabled();
    const live = on ? this.deps.liveSessions() : [];
    this.index.keep(live);
    const partKey = (catalog, database) => `part\u0000${catalog}\u0000${database}`;
    const have = new Set(); // the part keys that should be there
    for (const { name, session } of live) {
      await this.index.refresh(name, session);
      const catalog = name === DEFAULT ? "" : name;
      const target = catalog ? { catalog } : {};
      for (const part of this.index.parts(name)) {
        const key = partKey(catalog, part.database);
        have.add(key);
        if (this.sent.get(key) === part.digest) continue;
        this.sent.set(key, part.digest);
        client.sendNotification("grebe/catalog", { ...target, database: part.database, tables: part.tables, functions: part.functions });
      }
    }
    // Parts of databases that are gone (detached, emptied, too large now,
    // their session ended).
    for (const key of [...this.sent.keys()]) {
      const [kind, catalog, database] = key.split("\u0000");
      if (kind !== "part" || have.has(key)) continue;
      this.sent.delete(key);
      client.sendNotification("grebe/catalog", { ...(catalog ? { catalog } : {}), database, columns: null });
    }

    // Which catalog each open file uses.
    const open = new Set();
    for (const doc of vscode.workspace.textDocuments || []) {
      if (doc.languageId !== "sql" || doc.uri.scheme !== "file") continue;
      const uri = doc.uri.toString();
      const key = `doc\u0000${uri}`;
      open.add(key);
      const name = this.deps.sessionNameOf(doc);
      if (name === DEFAULT) {
        // No longer (or never) a file with a session of its own.
        if (this.sent.delete(key)) client.sendNotification("grebe/catalog", { uri, default: true });
      } else if (this.sent.get(key) !== name) {
        this.sent.set(key, name);
        client.sendNotification("grebe/catalog", { uri, catalog: name });
      }
    }
    // The server drops a closed document's catalog itself.
    for (const key of [...this.sent.keys()]) if (key.startsWith("doc\u0000") && !open.has(key)) this.sent.delete(key);

    // The foreign databases that are not indexed yet, or not any more: in
    // the background, one at a time; each one that finishes syncs again.
    for (const { name, session } of live) {
      this.index.indexPending(name, session).catch((e) => log.error("catalog", `could not index the databases of ${name}: ${e.message ?? e}`, e));
    }
  }

  // --- as one snapshot --------------------------------------------------------

  async syncWhole(client) {
    const on = this.enabled();
    const sessions = new Map(this.deps.liveSessions().map((x) => [x.name, x.session]));
    const snaps = new Map(); // session name -> snapshot, asked once each
    const snapOf = async (name) => {
      if (!on) return null;
      if (!sessions.has(name)) return null;
      if (!snaps.has(name)) snaps.set(name, await snapshot(sessions.get(name)));
      return snaps.get(name);
    };
    const send = (key, params) => {
      const body = JSON.stringify(params);
      if (this.sent.get(key) === body) return;
      this.sent.set(key, body);
      client.sendNotification("grebe/catalog", params);
    };

    const def = await snapOf(DEFAULT);
    if (def !== undefined) send("", def ? { columns: def.columns, functions: def.functions } : { columns: null });

    const open = new Set();
    for (const doc of vscode.workspace.textDocuments || []) {
      if (doc.languageId !== "sql" || doc.uri.scheme !== "file") continue;
      const uri = doc.uri.toString();
      open.add(uri);
      const name = this.deps.sessionNameOf(doc);
      if (name === DEFAULT) {
        // No longer (or never) a file with a session of its own.
        if (this.sent.has(uri)) {
          this.sent.delete(uri);
          client.sendNotification("grebe/catalog", { uri, default: true });
        }
        continue;
      }
      const snap = await snapOf(name);
      if (snap === undefined) continue;
      send(uri, snap ? { uri, columns: snap.columns, functions: snap.functions } : { uri, columns: null });
    }
    // The server drops a closed document's catalog itself.
    for (const key of [...this.sent.keys()]) if (key !== "" && !open.has(key)) this.sent.delete(key);
  }

  /** A run of `statements` ended in session `name`. */
  ran(name, statements) {
    this.index.noteRun(name, statements);
    this.schedule();
  }

  /** The Catalog attached or detached something in session `name`. */
  touched(name) {
    this.index.touch(name);
    this.schedule();
  }

  /** Index `db` of session `name` again (all its foreign ones when null). */
  reindex(name, db = null) {
    this.index.reindex(name, db);
    this.schedule();
  }

  /** What the Catalog shows of `db` in session `name`, or null (also when
   *  the index is off). */
  status(name, db) {
    return this.indexed ? this.index.status(name, db) : null;
  }

  dispose() {
    clearTimeout(this.timer);
  }
}

function activate(context, deps) {
  const maxAgeMs = () => Math.max(0, Number(vscode.workspace.getConfiguration("grebe.duckdb").get("catalogIndexMaxAge", 60)) || 0) * 60000;
  let sync = null;
  const index = new CatalogIndex({
    storageDir: context.storageUri ? context.storageUri.fsPath : null,
    maxAgeMs,
    isBusy: deps.isRunning,
    connections: deps.connections,
    onChange: (name) => {
      if (sync) sync.schedule();
      if (deps.onDidIndex) deps.onDidIndex(name);
    },
    log: (line) => log.info("catalog", line),
  });
  sync = new CatalogSync({ ...deps, index });
  context.subscriptions.push(
    sync,
    deps.onDidRun((run) => (run && run.statements ? sync.ran(run.session, run.statements) : sync.schedule())),
    vscode.workspace.onDidOpenTextDocument((doc) => doc.languageId === "sql" && sync.schedule()),
    vscode.workspace.onDidSaveTextDocument((doc) => doc.languageId === "sql" && sync.schedule()),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("grebe.duckdb.catalogInEditor") || e.affectsConfiguration("grebe.duckdb.catalogIndex")) sync.schedule();
    }),
  );
  return sync;
}

module.exports = { activate, CatalogSync, snapshot, COLUMNS, FUNCTIONS, MAX_COLUMNS };
