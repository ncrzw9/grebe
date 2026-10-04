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
const DATABASES =
  `SELECT database_name AS db, path, readonly, type, database_name = current_database() AS current ` +
  `FROM duckdb_databases() WHERE NOT internal OR database_name = 'temp'`;

/** Macros, sequences and user-defined types in `dbs` (all when null). */
const EXTRAS = (dbs) => {
  const where = dbs ? ` AND database_name IN (${dbs.map(lit).join(", ")})` : "";
  return (
    `SELECT database_name AS db, schema_name AS sch, function_name AS name, function_type AS kind, ` +
    `array_to_string(parameters, ', ') AS detail, macro_definition AS def ` +
    `FROM duckdb_functions() WHERE NOT internal AND function_type IN ('macro', 'table_macro')${where} ` +
    `UNION ALL SELECT database_name, schema_name, sequence_name, 'sequence', NULL, NULL FROM duckdb_sequences() WHERE true${where} ` +
    `UNION ALL SELECT database_name, schema_name, type_name, 'type', logical_type, NULL FROM duckdb_types() WHERE NOT internal${where} ` +
    `ORDER BY 1, 2, 4, 3`
  );
};

// What the session holds outside any database. Secrets are listed by name,
// type and scope only: the secret itself is never read.
const VARIABLES = `SELECT name, value, type FROM duckdb_variables() ORDER BY name`;
const SECRETS = `SELECT name, type, provider, storage, array_to_string(scope, ', ') AS scope FROM duckdb_secrets() ORDER BY name`;
const EXTENSIONS = `SELECT extension_name AS name, extension_version AS version FROM duckdb_extensions() WHERE loaded ORDER BY 1`;
// Process-wide figures: every row carries the same two.
const MEMORY = `SELECT memory_usage AS used, memory_limit AS "limit" FROM pragma_database_size() LIMIT 1`;
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
      if (node.kind === "schema") return this.schemaChildren(node.source, node.objects, node.extras);
      if (node.kind === "group") return node.children;
      if (node.kind === "object") return this.columnNodes(node);
    } catch (e) {
      log.error("catalog", `could not list ${node ? node.item.label : "the catalog"}: ${e.message ?? e}`, e);
      return [message(`Error: ${e.message ?? e}`, "grebe.showOutput", "error")];
    }
    return [];
  }

  async roots() {
    const s = this.deps.settings();
    const label = s.database === ":memory:" ? ":memory:" : path.basename(s.database);
    // How much memory the session holds, when it is running. Asked here,
    // not with the children: the root's own line is drawn before them.
    const live = this.deps.liveSession();
    const mem = live ? (objects(await live.run(MEMORY).catch(() => ({ kind: "error" }))) || [])[0] : null;
    const roots = [
      node("root", `Session · ${label}`, vscode.TreeItemCollapsibleState.Expanded, {
        source: "session",
        icon: "database",
        description: mem ? `${mem.used} of ${mem.limit} in use` : undefined,
        tooltip: `The database your runs use: ${s.database}` + (mem ? `\nMemory in use: ${mem.used} (limit ${mem.limit})` : ""),
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
    const extras = objects(await sess.run(EXTRAS(dbFilter))) || [];
    let dbs;
    if (root.source === "file") {
      dbs = [{ db: root.alias, current: true }];
    } else {
      const holds = (db) => objs.some((o) => o.db === db) || extras.some((x) => x.db === db);
      dbs = (objects(await sess.run(DATABASES)) || []).filter((d) => d.db !== "temp" || holds("temp"));
      // The session's own database first, then attached ones by name, temp last.
      const rank = (d) => (d.current ? 0 : d.db === "temp" ? 2 : 1);
      dbs.sort((a, b) => rank(a) - rank(b) || a.db.localeCompare(b.db));
    }
    const nodes = dbs.map((d) => {
      const attached = root.source === "session" && !d.current && d.db !== "temp";
      return node("db", d.db, vscode.TreeItemCollapsibleState.Expanded, {
        source: root.source,
        db: d.db,
        objects: objs.filter((o) => o.db === d.db),
        extras: extras.filter((x) => x.db === d.db),
        icon: d.db === "temp" ? "clock" : "database",
        description: describeDb(d),
        tooltip: d.path ? `${d.db}: ${d.path}` : d.db,
        contextValue: attached ? "grebe.attachedDb" : "grebe.db",
      });
    });
    const session = root.source === "session" ? await this.sessionGroups(sess) : [];
    if (nodes.length === 0) return [message("No tables yet"), ...session];
    // One database: show its contents directly.
    return nodes.length === 1 ? [...this.dbChildren(nodes[0]), ...session] : [...nodes, ...session];
  }

  /** Variables, secrets and extensions: what the session holds outside any
   *  database. */
  async sessionGroups(sess) {
    const vars = objects(await sess.run(VARIABLES)) || [];
    const secrets = objects(await sess.run(SECRETS)) || [];
    const exts = objects(await sess.run(EXTENSIONS)) || [];
    const leaf = (label, icon, description, tooltip) =>
      node("leaf", label, vscode.TreeItemCollapsibleState.None, { icon, description, tooltip });
    const groups = [];
    if (vars.length) {
      groups.push(
        group("Variables", "symbol-variable", vars.map((v) => leaf(v.name, "symbol-variable", `${v.value} · ${v.type}`, `getvariable('${v.name}')`))),
      );
    }
    if (secrets.length) {
      const what = (x) => [x.type, x.provider, x.storage].filter(Boolean).join(" · ");
      groups.push(group("Secrets", "key", secrets.map((x) => leaf(x.name, "key", what(x), x.scope ? `scope: ${x.scope}` : undefined))));
    }
    if (exts.length) {
      groups.push(group("Extensions", "extensions", exts.map((x) => leaf(x.name, "extensions", x.version || "")), "loaded"));
    }
    return groups;
  }

  dbChildren(dbNode) {
    const bySchema = new Map();
    const entry = (sch) => {
      if (!bySchema.has(sch)) bySchema.set(sch, { objects: [], extras: [] });
      return bySchema.get(sch);
    };
    for (const o of dbNode.objects) {
      entry(o.sch).objects.push({ db: o.db, schema: o.sch, name: o.name, kind: o.kind, est: o.est, cols: o.cols, tmp: o.tmp });
    }
    for (const x of dbNode.extras || []) {
      entry(x.sch).extras.push({ db: x.db, schema: x.sch, name: x.name, kind: x.kind, detail: x.detail, def: x.def });
    }
    if (bySchema.size === 0) return [message("No tables yet")];
    const schemas = [...bySchema.entries()].map(([name, { objects: objs, extras }]) =>
      node("schema", name, vscode.TreeItemCollapsibleState.Collapsed, {
        source: dbNode.source,
        objects: objs,
        extras,
        icon: "symbol-namespace",
        description: `${objs.length + extras.length}`,
      }),
    );
    // A lone `main` schema: show what is in it directly.
    if (schemas.length === 1 && schemas[0].item.label === "main") {
      return this.schemaChildren(dbNode.source, schemas[0].objects, schemas[0].extras);
    }
    return schemas;
  }

  /** A schema's tables and views, then a folder each for its macros,
   *  sequences and types. */
  schemaChildren(source, objs, extras = []) {
    const out = objs.map((o) => this.objectNode(source, o));
    const of = (...kinds) => extras.filter((x) => kinds.includes(x.kind));
    const leaf = (x, label, icon, description, tooltip, contextValue) =>
      node("leaf", label, vscode.TreeItemCollapsibleState.None, { icon, description, tooltip, object: x, contextValue });
    const macros = of("macro", "table_macro");
    if (macros.length) {
      const items = macros.map((m) =>
        leaf(m, `${m.name}(${m.detail || ""})`, "symbol-function", m.kind === "table_macro" ? "table macro" : "macro", m.def || undefined, "grebe.macro"),
      );
      out.push(group("Macros", "symbol-function", items));
    }
    const seqs = of("sequence");
    if (seqs.length) {
      out.push(group("Sequences", "symbol-number", seqs.map((q) => leaf(q, q.name, "symbol-number", "", `nextval('${q.name}')`, "grebe.sequence"))));
    }
    const types = of("type");
    if (types.length) {
      out.push(group("Types", "symbol-enum", types.map((y) => leaf(y, y.name, "symbol-enum", y.detail || "", undefined, "grebe.type"))));
    }
    return out;
  }

  /** ATTACH a database file to the session, so its tables can be queried. */
  async attach(uri) {
    if (!uri) {
      const picked = await vscode.window.showOpenDialog({
        canSelectMany: false,
        openLabel: "Attach",
        filters: { Databases: ["duckdb", "ddb", "db", "sqlite", "sqlite3"], "All files": ["*"] },
      });
      uri = picked && picked[0];
    }
    if (!uri) return;
    const mode = await vscode.window.showQuickPick(
      [
        { label: "Read-only", description: "queries can read it; nothing can change it", readOnly: true },
        { label: "Read and write", description: "CREATE, INSERT and the rest work on it too", readOnly: false },
      ],
      { placeHolder: `Attach ${path.basename(uri.fsPath)} to the session` },
    );
    if (!mode) return;
    const sess = await this.deps.currentSession();
    const taken = new Set((objects(await sess.run("SELECT database_name AS db FROM duckdb_databases()")) || []).map((d) => d.db));
    let alias = path.basename(uri.fsPath).replace(/\.[^.]+$/, "").replace(/[^A-Za-z0-9_]/g, "_") || "db";
    if (/^[0-9]/.test(alias)) alias = `db_${alias}`;
    while (taken.has(alias)) alias += "_";
    // A SQLite file needs saying so; DuckDB loads its sqlite extension.
    const opts = [/\.sqlite3?$/i.test(uri.fsPath) ? "TYPE sqlite" : null, mode.readOnly ? "READ_ONLY" : null].filter(Boolean);
    const sql = `ATTACH ${lit(uri.fsPath)} AS ${ident(alias)}${opts.length ? ` (${opts.join(", ")})` : ""}`;
    const r = await sess.run(sql);
    if (r.kind === "error") {
      log.report("catalog", `could not attach ${path.basename(uri.fsPath)}: ${r.type} Error: ${r.message}`);
      return;
    }
    log.info("catalog", sql);
    this.refresh();
    vscode.window.setStatusBarMessage(`grebe: attached as ${alias}; query it as ${alias}.<table>`, 5000);
  }

  /** DETACH an attached database from the session. */
  async detach(n) {
    if (!n || n.kind !== "db") return;
    const sess = await this.deps.currentSession();
    const r = await sess.run(`DETACH ${ident(n.db)}`);
    if (r.kind === "error") {
      log.report("catalog", `could not detach ${n.db}: ${r.type} Error: ${r.message}`);
      return;
    }
    log.info("catalog", `DETACH ${ident(n.db)}`);
    this.refresh();
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

/** What a database is, in a few words: attached or not, its type when it is
 *  not DuckDB, where it lives, and whether it can be written. */
function describeDb(d) {
  if (d.db === "temp") return "TEMP objects";
  const parts = [];
  if (!d.current) parts.push("attached");
  if (d.type && d.type !== "duckdb") parts.push(d.type);
  if (d.path) parts.push(path.basename(d.path));
  else if (!d.type || d.type === "duckdb") parts.push("in-memory");
  if (d.readonly) parts.push("read-only");
  return parts.join(" · ");
}

/** A folder of `children`, labelled with how many there are. */
function group(label, icon, children, note) {
  return node("group", label, vscode.TreeItemCollapsibleState.Collapsed, {
    icon,
    children,
    description: note ? `${children.length} ${note}` : `${children.length}`,
  });
}

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
    on("attach", (uri) => explorer.attach(uri)),
    on("detach", (n) => explorer.detach(n)),
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
