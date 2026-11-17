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
// opened, and when the server restarts; an unchanged catalog is not sent
// again.

"use strict";

const vscode = require("vscode");
const log = require("./log");

// Above this many columns the catalog is not sent: it would be megabytes on
// every run (an attached warehouse with tens of thousands of tables).
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

class CatalogSync {
  /**
   * @param {{ clientReady: () => Promise<object|null>,
   *           liveSessions: () => { name: string, session: object }[],
   *           sessionNameOf: (doc: object) => string }} deps
   */
  constructor(deps) {
    this.deps = deps;
    this.sent = new Map(); // "" (default) or a uri -> the JSON last sent
    this.timer = null;
    this.running = Promise.resolve();
  }

  enabled() {
    return vscode.workspace.getConfiguration("grebe.duckdb").get("catalogInEditor", true) !== false;
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
    this.schedule();
  }

  sync() {
    this.running = this.running.then(() => this.syncNow()).catch((e) => log.error("catalog", `could not send the catalog to the language server: ${e.message ?? e}`, e));
    return this.running;
  }

  async syncNow() {
    const client = await this.deps.clientReady();
    if (!client) return;
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

    const def = await snapOf("default");
    if (def !== undefined) send("", def ? { columns: def.columns, functions: def.functions } : { columns: null });

    const open = new Set();
    for (const doc of vscode.workspace.textDocuments || []) {
      if (doc.languageId !== "sql" || doc.uri.scheme !== "file") continue;
      const uri = doc.uri.toString();
      open.add(uri);
      const name = this.deps.sessionNameOf(doc);
      if (name === "default") {
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

  dispose() {
    clearTimeout(this.timer);
  }
}

function activate(context, deps) {
  const sync = new CatalogSync(deps);
  context.subscriptions.push(
    sync,
    deps.onDidRun(() => sync.schedule()),
    vscode.workspace.onDidOpenTextDocument((doc) => doc.languageId === "sql" && sync.schedule()),
    vscode.workspace.onDidSaveTextDocument((doc) => doc.languageId === "sql" && sync.schedule()),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("grebe.duckdb.catalogInEditor")) sync.schedule();
    }),
  );
  return sync;
}

module.exports = { activate, CatalogSync, snapshot, COLUMNS, FUNCTIONS, MAX_COLUMNS };
