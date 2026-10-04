// The Catalog view: what is inside the session's database (`:memory:`
// included, TEMP tables included) and inside any .duckdb file you browse --
// databases, schemas, tables and views, columns with types. Clicking a table
// or view previews it in the results grid.
//
// The session's own contents can only be read through the session (an
// in-memory database exists only in that process), so the session root asks
// it, and refreshes after every run. A browsed file is ATTACHed READ_ONLY in
// a separate in-memory session: browsing can never change it, and never
// waits behind a query running in the session. Levels with one child are
// collapsed (one database shows its schemas; a lone `main` shows its
// tables) to keep the tree short.

"use strict";

const path = require("path");
const vscode = require("vscode");
const { Session } = require("./duckdb-session");
const { columnar } = require("./lenient-json");
const results = require("./results");
const log = require("./log");

const lit = (s) => `'${String(s).replace(/'/g, "''")}'`;
const ident = (s) => `"${String(s).replace(/"/g, '""')}"`;
const qname = (o) => [o.db, o.schema, o.name].map(ident).join(".");

/** All tables and views in `dbs` (every non-internal database when null),
 *  with estimated rows and column counts, in one query. */
const OBJECTS = (dbs) => {
  const where = dbs ? ` AND database_name IN (${dbs.map(lit).join(", ")})` : "";
  return (
    `SELECT database_name AS db, schema_name AS sch, table_name AS name, 'table' AS kind, estimated_size AS est, column_count AS cols, temporary AS tmp ` +
    `FROM duckdb_tables() WHERE NOT internal${where} ` +
    `UNION ALL SELECT v.database_name, v.schema_name, v.view_name, 'view', NULL, ` +
    `(SELECT count(*) FROM duckdb_columns() c WHERE c.database_name = v.database_name AND c.schema_name = v.schema_name AND c.table_name = v.view_name), v.temporary ` +
    `FROM duckdb_views() v WHERE NOT v.internal${where.replace("database_name", "v.database_name")} ` +
    `ORDER BY 1, 2, 4, 3`
  );
};
const DATABASES = `SELECT database_name AS db, path, readonly FROM duckdb_databases() WHERE NOT internal OR database_name = 'temp'`;
const COLUMNS = (o) =>
  `SELECT column_name AS name, data_type AS type, is_nullable AS nullable FROM duckdb_columns() ` +
  `WHERE database_name = ${lit(o.db)} AND schema_name = ${lit(o.schema)} AND table_name = ${lit(o.name)} ORDER BY column_index`;

/** Rows of a `run` result as objects. */
const objects = (r) => (r.kind === "rows" ? r.rows.map((row) => Object.fromEntries(r.columns.map((c, i) => [c, row[i]]))) : null);

class Explorer {
  /**
   * @param {{ currentSession: () => Promise<Session>, liveSession: () => Session|null,
   *           settings: () => object, cli: () => string }} deps
   */
  constructor(deps) {
    this.deps = deps;
    this.changed = new vscode.EventEmitter();
    this.onDidChangeTreeData = this.changed.event;
    this.files = new Map(); // alias -> { file, error }
    this.fileSession = null;
  }

  refresh() {
    this.changed.fire();
  }

  getTreeItem(node) {
    return node.item;
  }

  async getChildren(node) {
    try {
      if (!node) return this.roots();
      if (node.kind === "root") return this.sourceChildren(node);
      if (node.kind === "db") return this.dbChildren(node);
      if (node.kind === "schema") return node.objects.map((o) => this.objectNode(node.source, o));
      if (node.kind === "object") return this.columnNodes(node);
    } catch (e) {
      log.error("catalog", `could not list ${node ? node.item.label : "the catalog"}: ${e.message ?? e}`, e);
      return [message(`Error: ${e.message ?? e}`, "grebe.showOutput", "error")];
    }
    return [];
  }

  roots() {
    const s = this.deps.settings();
    const label = s.database === ":memory:" ? ":memory:" : path.basename(s.database);
    const roots = [
      node("root", `Session · ${label}`, vscode.TreeItemCollapsibleState.Expanded, {
        source: "session",
        icon: "database",
        tooltip: `The database your runs use: ${s.database}`,
        contextValue: "grebe.sessionRoot",
      }),
    ];
    for (const [alias, f] of this.files) {
      roots.push(
        node("root", path.basename(f.file), vscode.TreeItemCollapsibleState.Expanded, {
          source: "file",
          alias,
          icon: "file-binary",
          description: "read-only",
          tooltip: f.file,
          contextValue: "grebe.fileRoot",
        }),
      );
    }
    return roots;
  }

  async sessionFor(source) {
    if (source === "session") return this.deps.currentSession();
    if (!this.fileSession || !this.fileSession.alive) {
      this.fileSession = new Session({ cli: this.deps.cli(), database: ":memory:", cwd: process.cwd() });
      await this.fileSession.start();
      for (const [alias, f] of this.files) {
        const r = await this.fileSession.run(`ATTACH ${lit(f.file)} AS ${ident(alias)} (READ_ONLY)`);
        f.error = r.kind === "error" ? r.message : null;
      }
    }
    return this.fileSession;
  }

  async sourceChildren(root) {
    if (root.source === "session" && !this.deps.liveSession()) {
      return [message("Not started — run a statement, or click to start", "grebe.catalog.start", "play")];
    }
    if (root.source === "file" && this.files.get(root.alias).error) {
      return [message(this.files.get(root.alias).error)];
    }
    const sess = await this.sessionFor(root.source);
    const dbFilter = root.source === "file" ? [root.alias] : null;
    const objs = objects(await sess.run(OBJECTS(dbFilter))) || [];
    let dbs;
    if (root.source === "file") {
      dbs = [{ db: root.alias }];
    } else {
      dbs = (objects(await sess.run(DATABASES)) || []).filter((d) => d.db !== "temp" || objs.some((o) => o.db === "temp"));
      // The session file's own database first, then attached ones, temp last.
      dbs.sort((a, b) => (a.db === "temp") - (b.db === "temp"));
    }
    const nodes = dbs.map((d) =>
      node("db", d.db, vscode.TreeItemCollapsibleState.Expanded, {
        source: root.source,
        db: d.db,
        objects: objs.filter((o) => o.db === d.db),
        icon: d.db === "temp" ? "clock" : "database",
        description: d.readonly ? "read-only" : d.path ? path.basename(d.path) : "",
      }),
    );
    if (nodes.length === 0) return [message("No tables yet")];
    // One database: show its contents directly.
    return nodes.length === 1 ? this.dbChildren(nodes[0]) : nodes;
  }

  dbChildren(dbNode) {
    const bySchema = new Map();
    for (const o of dbNode.objects) {
      if (!bySchema.has(o.sch)) bySchema.set(o.sch, []);
      bySchema.get(o.sch).push({ db: o.db, schema: o.sch, name: o.name, kind: o.kind, est: o.est, cols: o.cols, tmp: o.tmp });
    }
    if (bySchema.size === 0) return [message("No tables yet")];
    const schemas = [...bySchema.entries()].map(([name, objs]) =>
      node("schema", name, vscode.TreeItemCollapsibleState.Collapsed, {
        source: dbNode.source,
        objects: objs,
        icon: "symbol-namespace",
        description: `${objs.length}`,
      }),
    );
    // A lone `main` schema: show its tables and views directly.
    if (schemas.length === 1 && schemas[0].item.label === "main") {
      return schemas[0].objects.map((o) => this.objectNode(dbNode.source, o));
    }
    return schemas;
  }

  objectNode(source, o) {
    const est = o.est === null || o.est === undefined ? null : Number(o.est);
    const rows = est === null ? "" : `~${est.toLocaleString()} row${est === 1 ? "" : "s"} · `;
    const n = node("object", o.name, vscode.TreeItemCollapsibleState.Collapsed, {
      source,
      object: o,
      icon: o.kind === "view" ? "eye" : "table",
      description: `${rows}${o.cols} col${o.cols === 1 ? "" : "s"}${o.tmp ? " · temp" : ""}`,
      tooltip: `${o.kind} ${qname(o)}`,
      contextValue: o.kind === "view" ? "grebe.view" : "grebe.table",
    });
    n.item.command = { command: "grebe.catalog.preview", title: "Preview", arguments: [n] };
    return n;
  }

  async columnNodes(n) {
    const sess = await this.sessionFor(n.source);
    const cols = objects(await sess.run(COLUMNS(n.object))) || [];
    return cols.map((c) =>
      node("column", c.name, vscode.TreeItemCollapsibleState.None, {
        icon: "symbol-field",
        description: c.type + (c.nullable ? "" : " NOT NULL"),
        contextValue: "grebe.column",
        object: n.object,
      }),
    );
  }

  /** Run `sql` for an object and show the result in the grid. */
  async show(n, title, sql) {
    const o = n.object;
    results.begin({ title: `${o.name} — ${title}`, detail: qname(o) });
    results.running(`${title} of ${o.name}`);
    let r;
    try {
      r = await (await this.sessionFor(n.source)).run(sql);
    } catch (e) {
      r = e && e.cancelled
        ? { kind: "cancelled", reason: "cancel", ended: true, message: e.message }
        : { kind: "error", type: "Session", message: String(e.message ?? e) };
    }
    if (r.kind === "error") log.error("catalog", `${title} of ${qname(o)}: ${r.type} Error: ${r.message}`);
    if (r.kind === "rows") r = { kind: "rows", ms: r.ms, ...columnar(r, 100000) };
    results.add({ id: ++nextId, uri: "", line: null, preview: sql, result: r, exportable: false });
    results.end(qname(o));
  }

  async browseFile(uri) {
    if (!uri) {
      const picked = await vscode.window.showOpenDialog({ canSelectMany: false, filters: { DuckDB: ["duckdb", "db", "ddb"] } });
      uri = picked && picked[0];
    }
    if (!uri) return;
    const file = uri.fsPath;
    // The session's own file is already shown, through the session.
    const s = this.deps.settings();
    if (s.database !== ":memory:" && path.resolve(s.database) === path.resolve(file)) {
      this.refresh();
      return;
    }
    let alias = path.basename(file).replace(/\.[^.]+$/, "").replace(/[^A-Za-z0-9_]/g, "_") || "db";
    while (this.files.has(alias) && this.files.get(alias).file !== file) alias += "_";
    this.files.set(alias, { file, error: null });
    const sess = await this.sessionFor("file");
    const r = await sess.run(`ATTACH ${lit(file)} AS ${ident(alias)} (READ_ONLY)`);
    // Already attached by an earlier browse: fine.
    this.files.get(alias).error = r.kind === "error" && !/already/i.test(r.message) ? r.message : null;
    this.refresh();
  }

  async closeFile(n) {
    if (!n || n.source !== "file") return;
    const f = this.files.get(n.alias);
    this.files.delete(n.alias);
    if (f && this.fileSession && this.fileSession.alive) await this.fileSession.run(`DETACH ${ident(n.alias)}`);
    this.refresh();
  }

  dispose() {
    this.changed.dispose();
    if (this.fileSession) this.fileSession.dispose();
  }
}

let nextId = 2e9;

function node(kind, label, state, extra = {}) {
  const item = new vscode.TreeItem(label, state);
  if (extra.icon) item.iconPath = new vscode.ThemeIcon(extra.icon);
  if (extra.description !== undefined) item.description = extra.description;
  if (extra.tooltip) item.tooltip = extra.tooltip;
  if (extra.contextValue) item.contextValue = extra.contextValue;
  return { kind, item, ...extra };
}

function message(text, command, icon) {
  const n = node("message", text, vscode.TreeItemCollapsibleState.None, { icon: icon || "info" });
  if (command) n.item.command = { command, title: text };
  return n;
}

function activate(context, deps) {
  const explorer = new Explorer(deps);
  const view = vscode.window.createTreeView("grebe.catalog", { treeDataProvider: explorer, showCollapseAll: true });
  const on = (name, fn) => log.command(`grebe.catalog.${name}`, fn);
  const withObject = (fn) => (n) => n && n.object && fn(n);
  context.subscriptions.push(
    explorer,
    view,
    on("refresh", () => explorer.refresh()),
    on("start", async () => {
      await deps.currentSession();
      explorer.refresh();
    }),
    on("preview", withObject((n) => explorer.show(n, "Preview", `FROM ${qname(n.object)} LIMIT 1000`))),
    on("columns", withObject((n) => explorer.show(n, "Columns", COLUMNS(n.object)))),
    on("stats", withObject((n) => explorer.show(n, "Stats", `SUMMARIZE ${qname(n.object)}`))),
    on("count", withObject((n) => explorer.show(n, "Row count", `SELECT count(*) AS "rows" FROM ${qname(n.object)}`))),
    on("copyName", withObject((n) => vscode.env.clipboard.writeText(qname(n.object)))),
    on("insertName", withObject((n) => {
      const ed = vscode.window.activeTextEditor;
      if (ed) ed.edit((b) => b.replace(ed.selection, qname(n.object)));
    })),
    on("browseFile", (uri) => explorer.browseFile(uri)),
    on("closeFile", (n) => explorer.closeFile(n)),
    on("useAsDatabase", async (uri) => {
      if (!uri) return;
      const target = vscode.workspace.workspaceFolders ? vscode.ConfigurationTarget.Workspace : vscode.ConfigurationTarget.Global;
      await vscode.workspace.getConfiguration("grebe.duckdb").update("database", uri.fsPath, target);
      explorer.refresh();
    }),
    deps.onDidRun(() => explorer.refresh()),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("grebe.duckdb")) explorer.refresh();
    }),
  );
  return explorer;
}

module.exports = { activate, Explorer, OBJECTS, qname };
