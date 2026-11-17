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
//
// The catalog is kept from one window to the next: what is attached to a
// session is attached again when the session restarts (sessions.js), an
// attachment that could not be brought back stays listed with the reason,
// and the files being browsed are browsed again. When files run in sessions
// of their own (`-- grebe: session NAME`), every running session has a root
// here; the one the file in front uses is the open one.

"use strict";

const path = require("path");
const vscode = require("vscode");
const { Session } = require("./duckdb-session");
const { columnar } = require("./lenient-json");
const results = require("./results");
const log = require("./log");
const trust = require("./trust");
const conninfo = require("./conninfo");

const lit = (s) => `'${String(s).replace(/'/g, "''")}'`;
const ident = (s) => `"${String(s).replace(/"/g, '""')}"`;
const qname = (o) => [o.db, o.schema, o.name].map(ident).join(".");

// A command's argument when it is a file. From the Explorer's context menu
// it is the file's Uri; from a view's title bar VS Code passes the view's
// selected item instead, which is not a file and means "ask".
const fileArg = (a) => (a && typeof a.fsPath === "string" ? a : undefined);

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
    // alias -> { file, error }: the files being browsed, kept with the
    // workspace so they are still here after a reload.
    this.state = deps.state || null;
    this.files = new Map((this.state ? this.state.get(FILES_KEY, []) : []).map((f) => [f.alias, { file: f.file, error: null }]));
    this.fileSession = null;
  }

  saveFiles() {
    if (this.state) this.state.update(FILES_KEY, [...this.files].map(([alias, f]) => ({ alias, file: f.file })));
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
    const current = this.deps.sessionName ? this.deps.sessionName() : "default";
    const labelOf = (database) => (database === ":memory:" || database === ":workspace:" ? database : path.basename(database));
    // How much memory a session holds, when it is running. Asked here,
    // not with the children: the root's own line is drawn before them.
    const memoryOf = async (sess) => (sess ? (objects(await sess.run(MEMORY).catch(() => ({ kind: "error" }))) || [])[0] : null);
    const live = this.deps.liveSession();
    const mem = await memoryOf(live);
    // The running session knows what it opened; before it starts, the
    // setting is what it will open.
    const database = live ? this.shownDatabase(live, s) : s.database;
    const named = current === "default" ? "" : ` ${current}`;
    const roots = [
      node("root", `Session${named} · ${labelOf(database)}`, vscode.TreeItemCollapsibleState.Expanded, {
        source: "session",
        icon: "database",
        description: mem ? `${mem.used} of ${mem.limit} in use` : undefined,
        tooltip: `The database your runs use: ${database}` + (mem ? `\nMemory in use: ${mem.used} (limit ${mem.limit})` : ""),
        contextValue: "grebe.sessionRoot",
      }),
    ];
    // Every other session that is running: files that asked for one of
    // their own (`-- grebe: session NAME`).
    for (const { name, session } of this.deps.liveSessions ? this.deps.liveSessions() : []) {
      if (name === current) continue;
      const m = await memoryOf(session);
      const db = this.shownDatabase(session, s);
      roots.push(
        node("root", `Session${name === "default" ? "" : ` ${name}`} · ${labelOf(db)}`, vscode.TreeItemCollapsibleState.Collapsed, {
          source: `session:${name}`,
          icon: "database",
          description: m ? `${m.used} of ${m.limit} in use` : undefined,
          tooltip: `Another running session: ${db}`,
          contextValue: "grebe.otherSessionRoot",
        }),
      );
    }
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

  /** A running session's database as the setting names it: the workspace
   *  database is `:workspace:`, not a path into the editor's storage. */
  shownDatabase(sess, s) {
    const d = sess.opts.database;
    return s.database === ":workspace:" && /[\\/]session(-[^\\/]*)?\.duckdb$/.test(d) ? ":workspace:" : d;
  }

  async sessionFor(source) {
    if (source === "session") return this.deps.currentSession();
    if (isSession(source)) {
      const name = source.slice("session:".length);
      const found = (this.deps.liveSessions ? this.deps.liveSessions() : []).find((x) => x.name === name);
      if (!found) throw new Error(`the session ${name} is not running`);
      return found.session;
    }
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
    if (!trust.trusted()) {
      return [message("Restricted Mode: trust this folder to use DuckDB", "workbench.trust.manage", "shield")];
    }
    if (root.source === "session" && !this.deps.liveSession()) {
      const start = message("Start DuckDB session", "grebe.catalog.start", "play");
      start.item.description = "or run a statement";
      start.item.tooltip = "Start the session your runs use, to see what is in it.";
      return [start];
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
      const attached = isSession(root.source) && !d.current && d.db !== "temp";
      // PostgreSQL, SQLite, ...: DuckDB estimates no rows for their tables
      // (it says 0), and lists each table's row type as a type of its own.
      const foreign = d.type && d.type !== "duckdb";
      return node("db", d.db, vscode.TreeItemCollapsibleState.Expanded, {
        source: root.source,
        db: d.db,
        objects: objs.filter((o) => o.db === d.db).map((o) => (foreign ? { ...o, est: null } : o)),
        extras: extras.filter((x) => x.db === d.db && !(foreign && x.kind === "type")),
        icon: d.db === "temp" ? "clock" : "database",
        description: describeDb(d),
        tooltip: d.path ? `${d.db}: ${conninfo.redact(d.path)}` : d.db,
        contextValue: attached ? "grebe.attachedDb" : "grebe.db",
      });
    });
    const session = isSession(root.source) ? await this.sessionGroups(sess) : [];
    // What should be attached and is not: it could not be brought back
    // when the session started (a file that moved, a server that is down).
    if (root.source === "session" && this.deps.remembered) {
      for (const rec of this.deps.remembered()) {
        if (!rec.error || dbs.some((d) => d.db === rec.alias)) continue;
        nodes.push(
          node("lost", rec.alias, vscode.TreeItemCollapsibleState.None, {
            icon: "warning",
            alias: rec.alias,
            description: `not attached · ${conninfo.where(rec.path)}`,
            tooltip: `Could not be attached again when the session started:\n${conninfo.redact(rec.error)}\n\nRestart the session to try again, or forget it.`,
            contextValue: "grebe.lostDb",
          }),
        );
      }
    }
    if (nodes.length === 0) return [message("No tables yet"), ...session];
    // One database: show its contents directly.
    return nodes.length === 1 && nodes[0].kind === "db" ? [...this.dbChildren(nodes[0]), ...session] : [...nodes, ...session];
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

  /** ATTACH a database file, or a PostgreSQL database, to the session, so
   *  its tables can be queried. From the Explorer it is the file clicked;
   *  otherwise ask which. */
  async attach(arg) {
    if (!trust.ok("Attaching a database")) return;
    let uri = fileArg(arg);
    if (!uri) {
      const kind = await vscode.window.showQuickPick(
        [
          { label: "$(file) Database file…", description: ".duckdb, .sqlite", postgres: false },
          { label: "$(server) PostgreSQL…", description: "a connection string", postgres: true },
        ],
        { placeHolder: "Attach to the session" },
      );
      if (!kind) return;
      if (kind.postgres) return this.attachPostgres();
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
    const alias = await freeAlias(sess, path.basename(uri.fsPath).replace(/\.[^.]+$/, ""));
    // A SQLite file needs saying so; DuckDB loads its sqlite extension.
    const opts = [/\.sqlite3?$/i.test(uri.fsPath) ? "TYPE sqlite" : null, mode.readOnly ? "READ_ONLY" : null].filter(Boolean);
    const sql = `ATTACH ${lit(uri.fsPath)} AS ${ident(alias)}${opts.length ? ` (${opts.join(", ")})` : ""}`;
    const r = await sess.run(sql);
    if (r.kind === "error") {
      log.report("catalog", `could not attach ${path.basename(uri.fsPath)}: ${r.type} Error: ${r.message}`);
      return;
    }
    log.info("catalog", sql);
    await this.remember();
    this.refresh();
    vscode.window.setStatusBarMessage(`grebe: attached as ${alias}; query it as ${alias}.<table>`, 5000);
  }

  /** ATTACH a PostgreSQL database by connection string, through DuckDB's
   *  postgres extension, which is loaded first (and installed if it is not
   *  yet) so a failure says which step failed. Asks for a password only if
   *  the server wants one the string does not give. */
  async attachPostgres() {
    if (!trust.ok("Attaching a database")) return;
    const conn = await vscode.window.showInputBox({
      title: "Attach PostgreSQL Database",
      prompt: "libpq connection string or URI. Leave the password out to be asked for it, or to use PGPASSWORD or ~/.pgpass.",
      placeHolder: "dbname=mydb host=localhost port=5432 user=postgres   or   postgresql://postgres@localhost:5432/mydb",
      ignoreFocusOut: true,
      validateInput: (v) => (conninfo.isConnection(v.trim()) ? null : "Expected key=value pairs (dbname=... host=...) or a postgresql:// URI"),
    });
    if (!conn || !conn.trim()) return;
    const sess = await this.deps.currentSession();
    const suggested = await freeAlias(sess, conninfo.aliasFor(conn));
    const typed = await vscode.window.showInputBox({
      title: "Attach PostgreSQL Database",
      prompt: `Name to query it by: ${suggested}.public.<table>`,
      value: suggested,
      ignoreFocusOut: true,
      validateInput: (v) => (v.trim() ? null : "A name is needed"),
    });
    if (!typed || !typed.trim()) return;
    const alias = typed.trim();
    const where = conninfo.where(conn);
    const mode = await vscode.window.showQuickPick(
      [
        { label: "Read-only", description: "queries can read it; nothing can change it", readOnly: true },
        { label: "Read and write", description: "CREATE, INSERT, UPDATE and the rest go to PostgreSQL", readOnly: false },
      ],
      { placeHolder: `Attach ${where} as ${alias}` },
    );
    if (!mode) return;
    if (!(await loadExtension(sess, "postgres"))) return;
    const opts = ["TYPE postgres", mode.readOnly ? "READ_ONLY" : null].filter(Boolean).join(", ");
    const attachSql = (c) => `ATTACH ${lit(c)} AS ${ident(alias)} (${opts})`;
    let r = await sess.run(attachSql(conn));
    if (r.kind === "error" && /password/i.test(r.message) && !conninfo.parse(conn).password) {
      const password = await vscode.window.showInputBox({
        title: `Password for ${where}`,
        prompt: "The server asked for a password. It is used for this attach only and is not stored.",
        password: true,
        ignoreFocusOut: true,
      });
      if (password === undefined) return;
      r = await sess.run(attachSql(conninfo.withPassword(conn, password)));
    }
    if (r.kind === "error") {
      log.report("catalog", `could not attach ${where}: ${r.type} Error: ${r.message}`);
      return;
    }
    log.info("catalog", attachSql(conn));
    await this.remember();
    this.refresh();
    vscode.window.setStatusBarMessage(`grebe: attached ${where} as ${alias}; query it as ${alias}.<schema>.<table>`, 5000);
  }

  /** DETACH an attached database from the session. */
  async detach(n) {
    if (!n || n.kind !== "db") return;
    const sess = await this.sessionFor(n.source);
    const r = await sess.run(`DETACH ${ident(n.db)}`);
    if (r.kind === "error") {
      log.report("catalog", `could not detach ${n.db}: ${r.type} Error: ${r.message}`);
      return;
    }
    log.info("catalog", `DETACH ${ident(n.db)}`);
    await this.remember();
    this.refresh();
  }

  /** Note what the session has attached now, so it comes back with it. */
  async remember() {
    if (this.deps.remember) await this.deps.remember();
  }

  /** Stop trying to bring back an attachment that is gone. */
  forget(n) {
    if (!n || n.kind !== "lost" || !this.deps.forget) return;
    this.deps.forget(n.alias);
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

  async browseFile(arg) {
    if (!trust.ok("Browsing a database file")) return;
    let uri = fileArg(arg);
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
    this.saveFiles();
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
    this.saveFiles();
    if (f && this.fileSession && this.fileSession.alive) await this.fileSession.run(`DETACH ${ident(n.alias)}`);
    this.refresh();
  }

  dispose() {
    this.changed.dispose();
    if (this.fileSession) this.fileSession.dispose();
  }
}

let nextId = 2e9;

const FILES_KEY = "grebe.catalog.files";

/** A root that is a session: the current one, or another by name. */
const isSession = (source) => source === "session" || String(source).startsWith("session:");

/** `wanted` made into an identifier no database in `sess` already uses. */
async function freeAlias(sess, wanted) {
  const taken = new Set((objects(await sess.run("SELECT database_name AS db FROM duckdb_databases()")) || []).map((d) => d.db));
  let alias = String(wanted).replace(/[^A-Za-z0-9_]/g, "_") || "db";
  if (/^[0-9]/.test(alias)) alias = `db_${alias}`;
  while (taken.has(alias)) alias += "_";
  return alias;
}

/** LOAD a DuckDB extension, INSTALLing it first if it is not installed.
 *  Reports what failed and returns false if it could not. */
async function loadExtension(sess, name) {
  if ((await sess.run(`LOAD ${name}`)).kind !== "error") return true;
  log.info("catalog", `INSTALL ${name}`);
  const installed = await sess.run(`INSTALL ${name}`);
  const r = installed.kind === "error" ? installed : await sess.run(`LOAD ${name}`);
  if (r.kind !== "error") return true;
  log.report("catalog", `could not load DuckDB's ${name} extension: ${r.type} Error: ${r.message}`);
  return false;
}

/** What a database is, in a few words: attached or not, its type when it is
 *  not DuckDB, where it lives, and whether it can be written. */
function describeDb(d) {
  if (d.db === "temp") return "TEMP objects";
  const parts = [];
  if (!d.current) parts.push("attached");
  if (d.type && d.type !== "duckdb") parts.push(d.type);
  if (d.path) parts.push(conninfo.where(d.path));
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
      if (!trust.ok("Starting DuckDB")) return;
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
    on("attachPostgres", () => explorer.attachPostgres()),
    on("detach", (n) => explorer.detach(n)),
    on("forget", (n) => explorer.forget(n)),
    on("closeFile", (n) => explorer.closeFile(n)),
    on("useAsDatabase", async (arg) => {
      const uri = fileArg(arg);
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
