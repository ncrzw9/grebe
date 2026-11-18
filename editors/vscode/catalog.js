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
//
// With the catalog index on (catalogIndex.js; `grebe.duckdb.catalogIndex`)
// a session's databases are drawn from the index, and the session is asked
// nothing for them: the index already reads the local ones after a run
// that can have changed them, and a foreign one (PostgreSQL, SQLite, ...)
// once. Reading them here as well cost a walk of every attached database
// after every run. What is still asked of the session, once per refresh
// and never while a run has it: its variables, secrets and extensions
// (three small questions that touch no database), its memory figure, and
// the contents of a database the index holds no part of, when that one is
// opened. A foreign database starts closed, and nothing is built for a
// database until it is opened. A list of more than PAGE tables is drawn
// as folders of PAGE, named by the first and last table in each, so that
// no level of the tree is thousands of lines long and every table is
// still two clicks away. With the index off, the tree asks the session
// for everything after every run, as it always did.

"use strict";

const path = require("path");
const vscode = require("vscode");
const { Session } = require("./duckdb-session");
const { columnar } = require("./lenient-json");
const results = require("./results");
const log = require("./log");
const trust = require("./trust");
const conninfo = require("./conninfo");
const { EXTRAS } = require("./catalogIndex");

// The most tables and views one list shows before it is split into folders.
const PAGE = 500;
// Redraws asked for within this long of one another are drawn together.
const REDRAW_MS = 50;

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
    // Each running session's last memory figures, { used, limit }, and the
    // sessions that have been asked for newer ones.
    this.mem = new WeakMap();
    this.memAsking = new WeakSet();
    // Counts refresh(): what the tree read itself for an earlier one is
    // read again. Per session, what it was last told: `held` its
    // variables, secrets and extensions, `lived` the contents of the
    // databases the index has no part of.
    this.epoch = 0;
    this.held = new WeakMap();
    this.lived = new WeakMap();
    this.drawing = null;
    this.drawAgain = false;
  }

  saveFiles() {
    if (this.state) this.state.update(FILES_KEY, [...this.files].map(([alias, f]) => ({ alias, file: f.file })));
  }

  /** Something may have changed (a run ended, a database was attached, a
   *  setting changed): read again what the tree reads itself, and draw. */
  refresh() {
    this.epoch++;
    this.redraw();
  }

  /** Draw the tree from what is known now (the index read something, a
   *  memory figure arrived). The first of several is drawn at once, the
   *  rest together a moment later. */
  redraw() {
    if (this.drawing) {
      this.drawAgain = true;
      return;
    }
    this.changed.fire();
    this.drawing = setTimeout(() => {
      this.drawing = null;
      if (!this.drawAgain) return;
      this.drawAgain = false;
      this.redraw();
    }, REDRAW_MS);
    if (this.drawing.unref) this.drawing.unref();
  }

  getTreeItem(node) {
    return node.item;
  }

  async getChildren(node) {
    try {
      if (!node) return this.roots();
      if (node.kind === "root") return this.sourceChildren(node);
      if (node.kind === "db") return node.indexed ? this.indexedDbChildren(node) : this.dbChildren(node);
      if (node.kind === "schema") return this.schemaChildren(node.source, node.objects, node.extras, node.paged);
      if (node.kind === "range") return node.objects.map((o) => this.objectNode(node.source, o));
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
    // How much memory a session holds, when it is running. Not awaited:
    // the question waits behind whatever the session is running, and the
    // tree must not stay empty until a long query ends. The last figure is
    // drawn now; a different one redraws the tree when it arrives.
    const live = this.deps.liveSession();
    const mem = this.memoryOf(live);
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
      const m = this.memoryOf(session);
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
    const d = sess.opts ? sess.opts.database : s.database;
    return s.database === ":workspace:" && /[\\/]session(-[^\\/]*)?\.duckdb$/.test(d) ? ":workspace:" : d;
  }

  /** The last memory figures heard from `sess` (null until it answers), and
   *  a question for newer ones. */
  memoryOf(sess) {
    if (!sess) return null;
    this.askMemory(sess);
    return this.mem.get(sess) || null;
  }

  /** Ask `sess` for its memory figures, one question at a time; redraw if
   *  they changed. An unchanged answer redraws nothing, so a redraw never
   *  asks again forever. Not asked during a run, which redraws as it ends. */
  askMemory(sess) {
    if (this.memAsking.has(sess) || this.running()) return;
    this.memAsking.add(sess);
    // A background statement: a run that starts does not wait for it.
    sess
      .run(MEMORY, { background: true })
      .then((r) => {
        const m = (objects(r) || [])[0];
        if (!m || !sess.alive) return;
        const was = this.mem.get(sess);
        this.mem.set(sess, m);
        if (!was || was.used !== m.used || was.limit !== m.limit) this.redraw();
      })
      .catch(() => {})
      .finally(() => {
        this.memAsking.delete(sess);
      });
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

  /**
   * The session's connections that are not attached now: detached (and
   * waiting to be attached again), or not brought back when the session
   * started (a file that moved, a server that is down). Before the session
   * starts (`idle`), also the ones it will attach when it does.
   */
  waiting(attached, idle = false) {
    if (!this.deps.remembered) return [];
    const out = [];
    for (const rec of this.deps.remembered()) {
      if (attached(rec.alias)) continue;
      const where = conninfo.where(rec.path);
      const from = rec.declared ? "\n\nDeclared in the grebe.duckdb.connections setting." : "";
      const pending = idle && !rec.detached && !rec.error;
      out.push(
        node("lost", rec.alias, vscode.TreeItemCollapsibleState.None, {
          icon: rec.error ? "warning" : pending ? "plug" : "debug-disconnect",
          alias: rec.alias,
          declared: !!rec.declared,
          description: `${rec.error ? "not attached" : pending ? "attached when the session starts" : "detached"} · ${where}`,
          tooltip: rec.error
            ? `Could not be attached:\n${conninfo.redact(rec.error)}\n\nAttach it again to retry, or forget it.${from}`
            : pending
              ? `${rec.alias}: ${conninfo.redact(rec.path)}${from}`
              : `${rec.alias}: ${conninfo.redact(rec.path)}\n\nDetached: nothing here holds it open. Attach it again when you want it back; it stays listed until you forget it.${from}`,
          contextValue: rec.error ? "grebe.lostDb" : pending ? "grebe.pendingDb" : "grebe.detachedDb",
        }),
      );
    }
    return out;
  }

  async sourceChildren(root) {
    if (!trust.trusted()) {
      return [message("Restricted Mode: trust this folder to use DuckDB", "workbench.trust.manage", "shield")];
    }
    if (root.source === "session" && !this.deps.liveSession()) {
      const start = message("Start DuckDB session", "grebe.catalog.start", "play");
      start.item.description = "or run a statement";
      start.item.tooltip = "Start the session your runs use, to see what is in it.";
      // Its connections are listed before it starts: the ones that will be
      // attached with it, and the detached ones.
      return [start, ...this.waiting(() => false, true)];
    }
    if (root.source === "file" && this.files.get(root.alias).error) {
      return [message(this.files.get(root.alias).error)];
    }
    const sess = await this.sessionFor(root.source);
    const view = this.indexView(root.source, sess);
    if (view) return this.indexedChildren(root, sess, view);
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
      // What the editor knows of it (the catalog index): said for a
      // foreign database, which is read once, and for one left out.
      const indexed = isSession(root.source) && this.deps.indexStatus ? this.deps.indexStatus(this.sessionNameOf(root.source), d.db) : null;
      const index = describeIndex(indexed);
      return node("db", d.db, vscode.TreeItemCollapsibleState.Expanded, {
        source: root.source,
        db: d.db,
        objects: objs.filter((o) => o.db === d.db).map((o) => (foreign ? { ...o, est: null } : o)),
        extras: extras.filter((x) => x.db === d.db && !(foreign && x.kind === "type")),
        icon: d.db === "temp" ? "clock" : indexed && indexed.over ? "warning" : "database",
        description: [describeDb(d), index && index.short].filter(Boolean).join(" · "),
        tooltip: (d.path ? `${d.db}: ${conninfo.redact(d.path)}` : d.db) + (index ? `\n\n${index.long}` : ""),
        contextValue: attached ? "grebe.attachedDb" : "grebe.db",
      });
    });
    const session = isSession(root.source) ? await this.sessionGroups(sess) : [];
    if (root.source === "session") nodes.push(...this.waiting((alias) => dbs.some((d) => d.db === alias)));
    if (nodes.length === 0) return [message("No tables yet"), ...session];
    // One database: show its contents directly.
    return nodes.length === 1 && nodes[0].kind === "db" ? [...this.dbChildren(nodes[0]), ...session] : [...nodes, ...session];
  }

  // --- from the catalog index ------------------------------------------------

  /** A run is in progress: the session is the run's. */
  running() {
    return Boolean(this.deps.isRunning && this.deps.isRunning());
  }

  /** What the catalog index holds of the session `sess` that a root
   *  shows. Null when the tree asks the session itself: a browsed file,
   *  or the index is off. */
  indexView(source, sess) {
    if (!isSession(source) || !this.deps.indexView) return null;
    return this.deps.indexView(this.sessionNameOf(source), sess) || null;
  }

  /** One of the tree's own questions to a session whose databases come
   *  from the index. Not asked during a run, and dropped when one starts
   *  (a background statement, like the index's own). Null when it was not
   *  answered; the refresh as the run ends asks again. */
  async quietly(sess, sql) {
    if (!sess.alive || this.running()) return null;
    try {
      const r = await sess.run(sql, { background: true });
      return r.kind === "cancelled" ? null : r;
    } catch {
      return null;
    }
  }

  /** A session's databases as the index knows them. Nothing is asked of
   *  the session for them, and nothing is built of what is in one until
   *  it is opened. */
  async indexedChildren(root, sess, view) {
    // The index reads a session as soon as it is free; it says when.
    if (!view.read) return [message("Reading the catalog…", undefined, "sync")];
    const holds = (d) => Boolean(d.tables && d.tables.length) || d.extras.length > 0;
    const dbs = view.dbs.filter((d) => d.db !== "temp" || holds(d));
    // The session's own database first, then attached ones by name, temp last.
    const rank = (d) => (d.current ? 0 : d.db === "temp" ? 2 : 1);
    dbs.sort((a, b) => rank(a) - rank(b) || a.db.localeCompare(b.db));
    const nodes = dbs.map((d) => {
      const index = describeIndex(d.status);
      // A foreign database starts closed: it is the one that can hold
      // thousands of tables, and the one opened least.
      return node("db", d.db, d.foreign ? vscode.TreeItemCollapsibleState.Collapsed : vscode.TreeItemCollapsibleState.Expanded, {
        source: root.source,
        db: d.db,
        indexed: d,
        icon: d.db === "temp" ? "clock" : d.status && d.status.over ? "warning" : "database",
        description: [describeDb(d), index && index.short].filter(Boolean).join(" · "),
        tooltip: (d.path ? `${d.db}: ${conninfo.redact(d.path)}` : d.db) + (index ? `\n\n${index.long}` : ""),
        contextValue: !d.current && d.db !== "temp" ? "grebe.attachedDb" : "grebe.db",
      });
    });
    const session = await this.heldBy(sess);
    if (root.source === "session") nodes.push(...this.waiting((alias) => dbs.some((d) => d.db === alias)));
    if (nodes.length === 0) return [message("No tables yet"), ...session];
    // One database: show its contents directly.
    return nodes.length === 1 && nodes[0].kind === "db" ? [...(await this.indexedDbChildren(nodes[0])), ...session] : [...nodes, ...session];
  }

  /** What is in a database drawn from the index. */
  async indexedDbChildren(n) {
    const d = n.indexed;
    const s = d.status || {};
    let objs = [];
    let extras = d.extras;
    if (d.tables) {
      objs = d.tables.map((t) => ({
        db: d.db,
        sch: t.schema,
        name: t.table,
        kind: t.kind === "view" ? "view" : "table",
        // A local table's estimate is DuckDB's, a PostgreSQL table's the
        // server's own; other foreign tables and views have none.
        est: t.kind !== "view" && t.rows !== undefined ? t.rows : null,
        cols: t.columns ? t.columns.length : 0,
        tmp: d.db === "temp",
        table: t,
      }));
      // Tables before views, by name, schema by schema: the order the
      // session gives them in.
      objs.sort((a, b) => before(a.sch, b.sch) || before(a.kind, b.kind) || before(a.name, b.name));
    } else if (s.over || (d.foreign && s.error)) {
      // The index holds no part of it (too large for the editor, or it
      // could not be read): the session is asked, now that it is opened.
      const read = await this.liveContents(n.source, d);
      if (!read) return [message("Shown when the running statement ends")];
      objs = read.objects;
      extras = read.extras;
    } else if (d.foreign && s.indexedAt === null) {
      // Never indexed: the index reads it when the session is free, and
      // the tree is drawn again then. It is not read here as well.
      const wait = message("indexing…", undefined, "sync~spin");
      wait.item.tooltip = "Its tables and columns are being read. They are listed here as soon as they are.";
      return [wait];
    }
    return this.dbChildren({ source: n.source, objects: objs, extras, paged: true });
  }

  /** The tables, views and the rest of a database the index holds no part
   *  of, asked of the session once per refresh. Null when the session
   *  could not be asked and nothing was read before. */
  async liveContents(source, d) {
    const sess = await this.sessionFor(source);
    if (!this.lived.has(sess)) this.lived.set(sess, new Map());
    const kept = this.lived.get(sess);
    const had = kept.get(d.db);
    if (had && had.epoch === this.epoch) return had;
    const epoch = this.epoch;
    const objs = await this.quietly(sess, OBJECTS([d.db]));
    const extras = objs && (await this.quietly(sess, EXTRAS([d.db])));
    if (!objs || !extras) return had || null;
    const read = {
      epoch,
      objects: (objects(objs) || []).map((o) => (d.foreign ? { ...o, est: null } : o)),
      extras: (objects(extras) || []).filter((x) => !(d.foreign && x.kind === "type")),
    };
    kept.set(d.db, read);
    return read;
  }

  /** A session's variables, secrets and extensions, asked once per
   *  refresh however often the tree is drawn. While they cannot be asked
   *  for, the last ones known. */
  heldBy(sess) {
    const had = this.held.get(sess);
    if (had && had.epoch === this.epoch) return had.nodes;
    const now = { epoch: this.epoch, last: had ? had.last : [], nodes: null };
    now.nodes = this.sessionGroups(sess, (sql) => this.quietly(sess, sql)).then((groups) => {
      if (groups) now.last = groups;
      else now.epoch = -1; // not answered: the next drawing asks again
      return now.last;
    });
    this.held.set(sess, now);
    return now.nodes;
  }

  // ---------------------------------------------------------------------------

  /** Variables, secrets and extensions: what the session holds outside any
   *  database. `ask` runs one question; when it gives no answer (null),
   *  neither does this. */
  async sessionGroups(sess, ask = (sql) => sess.run(sql)) {
    const read = async (sql) => {
      const r = await ask(sql);
      return r ? objects(r) || [] : null;
    };
    const vars = await read(VARIABLES);
    const secrets = vars && (await read(SECRETS));
    const exts = secrets && (await read(EXTENSIONS));
    if (!exts) return null;
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
      const object = { db: o.db, schema: o.sch, name: o.name, kind: o.kind, est: o.est, cols: o.cols, tmp: o.tmp };
      // From the index: what it holds of the table, for its columns.
      if (o.table) object.table = o.table;
      entry(o.sch).objects.push(object);
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
        paged: dbNode.paged,
        icon: "symbol-namespace",
        description: `${objs.length + extras.length}`,
      }),
    );
    // A lone `main` schema: show what is in it directly.
    if (schemas.length === 1 && schemas[0].item.label === "main") {
      return this.schemaChildren(dbNode.source, schemas[0].objects, schemas[0].extras, dbNode.paged);
    }
    return schemas;
  }

  /** A schema's tables and views, then a folder each for its macros,
   *  sequences and types. `paged`: more tables and views than PAGE are
   *  put in folders, and built when a folder is opened. */
  schemaChildren(source, objs, extras = [], paged = false) {
    const out = paged && objs.length > PAGE ? ranges(source, objs) : objs.map((o) => this.objectNode(source, o));
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
    // The session it was detached from: not always the one in front.
    await this.remember(n.source === "session" ? undefined : n.source.slice("session:".length));
    this.refresh();
  }

  /** The name of the session a root shows. */
  sessionNameOf(source) {
    if (source === "session") return this.deps.sessionName ? this.deps.sessionName() : "default";
    return String(source).slice("session:".length);
  }

  /** Read a database again for the editor (completion, the CAT checks):
   *  the one clicked, or every foreign database of the session in front. */
  reindex(n) {
    const one = n && n.kind === "db" && isSession(n.source);
    if (this.deps.reindex) this.deps.reindex(one ? this.sessionNameOf(n.source) : this.sessionNameOf("session"), one ? n.db : null);
    this.refresh();
  }

  /** Note what the session has attached now, so it comes back with it. */
  async remember(name) {
    if (this.deps.remember) await this.deps.remember(name);
  }

  /** Attach a listed connection that is detached, or was not brought back. */
  async reattach(n) {
    if (!n || n.kind !== "lost" || !this.deps.attachAgain) return;
    if (!trust.ok("Attaching a database")) return;
    await this.sessionFor("session");
    const error = await this.deps.attachAgain(n.alias);
    if (error) log.report("catalog", `could not attach ${n.alias}: ${error}`);
    else vscode.window.setStatusBarMessage(`grebe: attached ${n.alias} again`, 5000);
    this.refresh();
  }

  /** Take a connection that is not attached off the list. */
  async forget(n) {
    if (!n || n.kind !== "lost" || !this.deps.forget) return;
    if (n.declared) {
      const pick = await vscode.window.showInformationMessage(
        `grebe: ${n.alias} is declared in the grebe.duckdb.connections setting. Remove it there to forget it.`,
        "Open Settings",
      );
      if (pick === "Open Settings") vscode.commands.executeCommand("workbench.action.openSettings", "grebe.duckdb.connections");
      return;
    }
    this.deps.forget(n.alias);
    this.refresh();
  }

  objectNode(source, { table, ...o }) {
    const est = o.est === null || o.est === undefined ? null : Number(o.est);
    const rows = est === null ? "" : `~${est.toLocaleString()} row${est === 1 ? "" : "s"} · `;
    const n = node("object", o.name, vscode.TreeItemCollapsibleState.Collapsed, {
      source,
      object: o,
      table,
      icon: o.kind === "view" ? "eye" : "table",
      description: `${rows}${o.cols} col${o.cols === 1 ? "" : "s"}${o.tmp ? " · temp" : ""}`,
      tooltip: `${o.kind} ${qname(o)}`,
      contextValue: o.kind === "view" ? "grebe.view" : "grebe.table",
    });
    n.item.command = { command: "grebe.catalog.preview", title: "Preview", arguments: [n] };
    return n;
  }

  async columnNodes(n) {
    const column = (name, type, nullable) =>
      node("column", name, vscode.TreeItemCollapsibleState.None, {
        icon: "symbol-field",
        description: type + (nullable ? "" : " NOT NULL"),
        contextValue: "grebe.column",
        object: n.object,
      });
    // From the index: it holds the columns too.
    if (n.table) {
      const t = n.table;
      return (t.columns || []).map((name, i) => column(name, (t.types || [])[i] || "", !(t.nullable && t.nullable[i] === false)));
    }
    const sess = await this.sessionFor(n.source);
    const cols = objects(await sess.run(COLUMNS(n.object))) || [];
    return cols.map((c) => column(c.name, c.type, c.nullable));
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
    clearTimeout(this.drawing);
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

/** What the catalog index says of a database, as a few words for the
 *  tree and a sentence for the tooltip; null when there is nothing to say
 *  (a local database that is read after every run). */
function describeIndex(s) {
  if (!s) return null;
  const n = (v) => Number(v).toLocaleString("en-US");
  if (s.over) {
    return {
      short: "not in the editor: too large",
      long: `${n(s.over)} columns, over the limit of ${n(s.limit)} for one database: the editor does not complete its names or check them. The other databases are not affected.`,
    };
  }
  if (!s.foreign) return null;
  if (s.indexing) return { short: "indexing…", long: "Reading its tables and columns for the editor." };
  if (s.error) return { short: "not indexed", long: `Its tables could not be read for the editor:\n${s.error}\n\nRefresh Connection Index tries again.` };
  if (s.indexedAt === null) return { short: "not indexed yet", long: "Its tables are read for the editor when the session is free." };
  const min = Math.max(0, Math.round((Date.now() - s.indexedAt) / 60000));
  const ago = min < 1 ? "just now" : min < 60 ? `${min} min ago` : min < 2880 ? `${Math.round(min / 60)} h ago` : `${Math.round(min / 1440)} d ago`;
  return {
    short: `indexed ${ago}`,
    long:
      `The editor completes and checks names from an index of this database: ${n(s.tables)} tables, ${n(s.columns)} columns, read ${ago}` +
      `${s.stale ? " and about to be read again" : ""}. It is read again when a statement changes the database, when it is older than ` +
      `grebe.duckdb.catalogIndexMaxAge, and when you choose Refresh Connection Index.`,
  };
}

const before = (a, b) => (a < b ? -1 : a > b ? 1 : 0);

/** `objs` (tables, then views, each by name) in folders of no more than
 *  PAGE of one kind, each named by the first and the last name in it. */
function ranges(source, objs) {
  const out = [];
  for (let i = 0; i < objs.length; ) {
    let j = i + 1;
    while (j < objs.length && j - i < PAGE && objs[j].kind === objs[i].kind) j++;
    const some = objs.slice(i, j);
    const first = some[0].name;
    const last = some[some.length - 1].name;
    out.push(
      node("range", some.length === 1 ? first : `${first} … ${last}`, vscode.TreeItemCollapsibleState.Collapsed, {
        source,
        objects: some,
        icon: "folder",
        description: `${some.length.toLocaleString("en-US")} ${some[0].kind}${some.length === 1 ? "" : "s"}`,
        tooltip: `${some[0].kind === "view" ? "Views" : "Tables"} ${first} to ${last}, by name`,
      }),
    );
    i = j;
  }
  return out;
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
    // The button reads everything again, the foreign databases' index
    // included.
    on("refresh", () => explorer.reindex(null)),
    on("reindex", (n) => explorer.reindex(n)),
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
    on("reattach", (n) => explorer.reattach(n)),
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
