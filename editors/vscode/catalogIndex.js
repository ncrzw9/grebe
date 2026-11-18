// What each session's databases hold, kept database by database, for the
// language server (catalogSync.js sends it) and the Catalog view. No
// `vscode` import, so it can be tested with plain Node.
//
// Reading a catalog is not free. Every duckdb_*() function walks every
// attached database, so with a PostgreSQL database of 5,000 tables attached
// one of them takes about 100 ms however it is filtered, and the first one
// after attaching also fetches that database's whole schema from the server
// (measured, 1.5.5 and 2.0.0: about 0.5 s for those 5,000 tables on
// localhost). So the index reads as little as it can:
//
//   - Local databases (the session's own, in-memory and TEMP ones, attached
//     DuckDB files) are read again after a run, each kept as a part of its
//     own so that one changing does not resend the others. While a foreign
//     database is attached, a run of nothing but queries reads nothing: it
//     cannot have changed a table.
//   - A foreign database (PostgreSQL, SQLite, MySQL, ...: any type but
//     duckdb) is indexed once, when first seen, and then reused. It is
//     indexed again when asked (the Catalog), when a statement that changes
//     structure names it or runs while it is the current database, and when
//     the index is older than the age limit. Its system schemas
//     (pg_catalog, information_schema) are left out: about 2,000 columns
//     of every PostgreSQL database that completion is better without. The
//     CAT rules do not check names in those schemas.
//   - The index of a foreign database is kept on disk, one file per
//     connection, so the next window has its names at once.
//
// Nothing here starts while the session is busy with a run, a foreign
// database is read one at a time, and every question the index asks is a
// background statement (duckdb-session.js): a run that starts while one is
// in flight interrupts it, and the index tries again after the run. On
// Windows a statement cannot be interrupted without ending the session, so
// there a run waits for the one index query in flight. The same wait
// happens anywhere if DuckDB does not act on the interrupt at once, which
// it may not in the middle of fetching a schema from a server.
//
// Beside names and types, a part says what a rule about keys or sizes can
// use: whether a column may be NULL, the estimated row count, the primary
// key, unique constraints and indexes. For DuckDB databases that is read
// from duckdb_tables(), duckdb_constraints() and duckdb_indexes(); for
// PostgreSQL from pg_catalog on the server (postgres_query), because
// DuckDB reports no row counts and no index columns for it. That second
// reading is extra: if it fails the columns are still indexed and the log
// says why the rest is missing. Other foreign types get columns only.

"use strict";

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const conninfo = require("./conninfo");

// Most columns one database may have and still be indexed. Measured with
// the release server and a synthetic database of 20-column tables, sent
// table by table as the parts are:
//
//     columns     message   taken in   server memory   lint    completion
//       100,000     4 MB     0.2 s      +  50-70 MB    11 ms     3 ms
//       500,000    22 MB     0.6 s      + 230-330 MB   17 ms     4 ms
//     1,000,000    44 MB     1.3 s      + 450-660 MB   22 ms     3 ms
//
// (Lint: a 200-line file with five misspelt table names, 9 ms with a small
// catalog; what grows is finding a suggestion for each. Completion: the
// slowest of a table name, a qualified one, a column. Memory: after one
// part, and after the same part a second time.) Looking a name up does not
// grow with the catalog, so the limit is about the one-off cost: the
// server answers nothing while it takes a part in, and the extension host
// holds the same tables once more. Past half a second and a third of a
// gigabyte that stops going unnoticed. The same million columns as one
// row each, the form the snapshot has, are 115 MB, 3 s and over 1 GB.
const MAX_COLUMNS = 500000;

const CACHE_DIR = "catalog-index";
const CACHE_VERSION = 1;
// The cache holds no more than this, oldest out first.
const MAX_CACHE_FILES = 24;
const MAX_CACHE_BYTES = 256 * 1024 * 1024;

// Schemas of a foreign database that are not indexed.
const SYSTEM_SCHEMAS = ["pg_catalog", "information_schema"];

const lit = (s) => `'${String(s).replace(/'/g, "''")}'`;
const inList = (names) => names.map(lit).join(", ");
const objects = (r) => r.rows.map((row) => Object.fromEntries(r.columns.map((c, i) => [c, row[i]])));

const DATABASES =
  `SELECT database_name AS db, path, type, database_name = current_database() AS current ` +
  `FROM duckdb_databases() WHERE NOT internal OR database_name = 'temp'`;

/** One row per table of `dbs`: its columns, types and nullability as lists
 *  in column order. */
const COLUMNS = (dbs, foreign) =>
  `SELECT database_name AS db, schema_name AS "schema", table_name AS "table", ` +
  `list(column_name ORDER BY column_index) AS "columns", list(data_type ORDER BY column_index) AS "types", ` +
  `list(is_nullable ORDER BY column_index) AS "nullable" ` +
  `FROM duckdb_columns() WHERE database_name IN (${inList(dbs)})` +
  (foreign ? ` AND schema_name NOT IN (${inList(SYSTEM_SCHEMAS)})` : ` AND NOT internal`) +
  ` GROUP BY ALL ORDER BY ALL`;

const COLUMN_COUNT = (db) =>
  `SELECT count(*) AS n FROM duckdb_columns() WHERE database_name = ${lit(db)} AND schema_name NOT IN (${inList(SYSTEM_SCHEMAS)})`;

/** What DuckDB knows of each table and view of its own databases `dbs`
 *  (the same query as `grebe catalog --with-metadata` runs, narrowed). */
const TABLES = (dbs) => {
  const here = `database_name IN (${inList(dbs)})`;
  return (
    `WITH keys AS (SELECT database_name AS d, schema_name AS s, table_name AS t, ` +
    `first(constraint_column_names) FILTER (constraint_type = 'PRIMARY KEY') AS pk, ` +
    `list(constraint_column_names) FILTER (constraint_type = 'UNIQUE') AS uq ` +
    `FROM duckdb_constraints() WHERE constraint_type IN ('PRIMARY KEY', 'UNIQUE') AND ${here} GROUP BY ALL), ` +
    `idx AS (SELECT database_name AS d, schema_name AS s, table_name AS t, ` +
    `list(string_split(expressions[2:-2], ', ')) FILTER (NOT is_unique) AS ix, ` +
    `list(string_split(expressions[2:-2], ', ')) FILTER (is_unique AND NOT is_primary) AS ux ` +
    `FROM duckdb_indexes() WHERE ${here} GROUP BY ALL) ` +
    `SELECT t.database_name AS db, t.schema_name AS "schema", t.table_name AS "table", 'table' AS kind, ` +
    `t.estimated_size AS "rows", k.pk AS primary_key, coalesce(k.uq, []) || coalesce(i.ux, []) AS "unique", i.ix AS indexes ` +
    `FROM duckdb_tables() t ` +
    `LEFT JOIN keys k ON k.d = t.database_name AND k.s = t.schema_name AND k.t = t.table_name ` +
    `LEFT JOIN idx i ON i.d = t.database_name AND i.s = t.schema_name AND i.t = t.table_name ` +
    `WHERE NOT t.internal AND t.${here} ` +
    `UNION ALL SELECT database_name, schema_name, view_name, 'view', NULL, NULL, NULL, NULL ` +
    `FROM duckdb_views() WHERE NOT internal AND ${here}`
  );
};

const FUNCTIONS =
  `SELECT DISTINCT database_name AS db, function_name AS "name", function_type AS "kind" FROM duckdb_functions() ` +
  `WHERE NOT internal AND function_type IN ('macro', 'table_macro') ORDER BY ALL`;

// PostgreSQL, asked directly. Not the system schemas, the TOAST tables or
// other sessions' temporary ones.
const PG_USER_SCHEMAS = `n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%'`;

/** Every table and view: what it is, and the planner's row estimate
 *  (reltuples; -1 until the table has been analysed, which is "unknown"). */
const PG_RELATIONS = (db) =>
  `SELECT * FROM postgres_query(${lit(db)}, $grebe$ ` +
  `SELECT n.nspname::text AS "schema", c.relname::text AS "table", c.relkind::text AS kind, ` +
  `CASE WHEN c.reltuples < 0 THEN NULL ELSE c.reltuples::bigint END AS "rows" ` +
  `FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace ` +
  `WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND ${PG_USER_SCHEMAS} $grebe$)`;

/** One row per key column of every usable index, in index order: a plain
 *  column by its name, an expression as PostgreSQL writes it. INCLUDE
 *  columns are not key columns; a partial index says nothing about the
 *  whole table and is left out. */
const PG_INDEXES = (db) =>
  `SELECT * FROM postgres_query(${lit(db)}, $grebe$ ` +
  `SELECT n.nspname::text AS "schema", c.relname::text AS "table", ic.relname::text AS "index", ` +
  `i.indisprimary AS is_primary, i.indisunique AS is_unique, k.ord::int AS ord, ` +
  `coalesce(a.attname::text, pg_catalog.pg_get_indexdef(i.indexrelid, k.ord::int, true)) AS col ` +
  `FROM pg_catalog.pg_index i ` +
  `JOIN pg_catalog.pg_class c ON c.oid = i.indrelid ` +
  `JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace ` +
  `JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid ` +
  `CROSS JOIN LATERAL generate_series(1, i.indnkeyatts) AS k(ord) ` +
  `LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[k.ord - 1] AND a.attnum > 0 ` +
  `WHERE i.indisvalid AND i.indpred IS NULL AND ${PG_USER_SCHEMAS} ` +
  `ORDER BY 1, 2, 3, 6 $grebe$)`;

// A statement that can change what a database holds, by its first word; and
// the functions that run one on a server.
const DDL = /^(create|alter|drop|rename|attach|detach|use|comment|import|truncate|vacuum|call|pragma|install|load)\b/i;
const REMOTE = /\b(postgres_execute|mysql_execute|pg_clear_cache|postgres_clear_cache)\b/i;

/** `sql` without its leading comments and blank space. */
function head(sql) {
  let s = String(sql);
  for (;;) {
    s = s.replace(/^\s+/, "");
    if (s.startsWith("--")) s = s.slice(s.indexOf("\n") < 0 ? s.length : s.indexOf("\n") + 1);
    else if (s.startsWith("/*")) s = s.slice(s.indexOf("*/") < 0 ? s.length : s.indexOf("*/") + 2);
    else return s;
  }
}

/** Whether `sql` can change structure (not: whether it does). */
function changesStructure(sql) {
  return DDL.test(head(sql)) || REMOTE.test(sql);
}

/** Whether `sql` names `alias`, as a word or quoted. */
function mentions(sql, alias) {
  const word = String(alias).replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`(^|[^A-Za-z0-9_])${word}([^A-Za-z0-9_]|$)`, "i").test(sql);
}

const digest = (value) => crypto.createHash("sha1").update(JSON.stringify(value)).digest("hex");

/** The file name a connection's index is kept under: a hash of what it is
 *  and where, with the password taken out first. Never the string itself. */
function cacheKey(type, target) {
  return crypto.createHash("sha256").update(`${type || "duckdb"}\n${conninfo.withoutPassword(String(target))}`).digest("hex").slice(0, 32);
}

/** `rows` (objects with schema, table and the rest) as a part's tables,
 *  null and empty facts left out. */
function tablesOf(rows) {
  return rows.map((r) => {
    const t = { schema: r.schema, table: r.table };
    if (r.kind === "view") t.kind = "view";
    if (r.rows !== null && r.rows !== undefined) t.rows = Number(r.rows);
    if (Array.isArray(r.primary_key) && r.primary_key.length) t.primary_key = r.primary_key;
    if (Array.isArray(r.unique) && r.unique.length) t.unique = r.unique;
    if (Array.isArray(r.indexes) && r.indexes.length) t.indexes = r.indexes;
    if (Array.isArray(r.columns)) {
      t.columns = r.columns;
      t.types = r.types || [];
      if (Array.isArray(r.nullable)) t.nullable = r.nullable;
    }
    return t;
  });
}

const countColumns = (tables) => tables.reduce((n, t) => n + (t.columns ? t.columns.length : 0), 0);

class CatalogIndex {
  /**
   * @param {{
   *   storageDir?: string|null,
   *   maxColumns?: number,
   *   maxAgeMs?: () => number,
   *   isForeign?: (db: { db: string, type: string, path: string|null }) => boolean,
   *   isBusy?: () => boolean,
   *   connections?: () => { type: string, path: string }[],
   *   onChange?: (name: string) => void,
   *   log?: (line: string) => void,
   *   now?: () => number,
   * }} deps
   * `isForeign` decides which databases are indexed once rather than after
   * every run (any type but duckdb, unless told otherwise). `isBusy` is
   * true while a run is in progress. `connections` lists the connections
   * still remembered: the cache of any other is deleted.
   */
  constructor(deps = {}) {
    this.deps = deps;
    this.maxColumns = deps.maxColumns || MAX_COLUMNS;
    this.dir = deps.storageDir ? path.join(deps.storageDir, CACHE_DIR) : null;
    this.sessions = new Map(); // session name -> state (see state())
    this.working = new Map(); // session name -> the indexing pass under way
  }

  now() {
    return this.deps.now ? this.deps.now() : Date.now();
  }

  say(line) {
    if (this.deps.log) this.deps.log(line);
  }

  isForeign(d) {
    return this.deps.isForeign ? this.deps.isForeign(d) : Boolean(d.type) && d.type !== "duckdb";
  }

  state(name, sess) {
    let st = this.sessions.get(name);
    if (!st || st.session !== sess) {
      // dbs: database name -> { db, type, path, key, foreign, current, part,
      // digest, columns, indexedAt, stale, over, error, indexing }
      st = { session: sess, dbs: new Map(), read: false, dirty: true };
      this.sessions.set(name, st);
    }
    return st;
  }

  /** Forget the sessions that are not among `live` (`[{ name, session }]`). */
  keep(live) {
    for (const [name, st] of [...this.sessions]) {
      if (!live.some((x) => x.name === name && x.session === st.session)) this.sessions.delete(name);
    }
  }

  /** One of the index's own questions to `sess`. Null when the session
   *  could not answer: it ended, or the question was interrupted for a
   *  run. */
  async ask(sess, sql) {
    if (!sess.alive) return null;
    try {
      const r = await sess.run(sql, { background: true });
      return r.kind === "cancelled" ? null : r;
    } catch {
      return null;
    }
  }

  /**
   * A run of `statements` (`[{ text, kind }]`, `kind` as the language
   * server names it) ended in the session called `name`. Nothing is read
   * here; this only notes what the run can have changed.
   */
  noteRun(name, statements) {
    const st = this.sessions.get(name);
    if (!st) return;
    const changing = (statements || []).filter((s) => s.kind !== "select");
    if (changing.length) st.dirty = true;
    for (const e of st.dbs.values()) {
      if (!e.foreign) continue;
      const hits = changing.filter((s) => changesStructure(s.text) && (e.current || mentions(s.text, e.db)));
      if (!hits.length) continue;
      e.stale = "a statement changed it";
      // DuckDB follows its own DDL, not what a statement had the server
      // do: then its copy of the schema has to be thrown away first.
      if (hits.some((s) => REMOTE.test(s.text))) e.fresh = true;
    }
  }

  /** Something outside a run changed what `name` has attached (the
   *  Catalog attached or detached a database). */
  touch(name) {
    const st = this.sessions.get(name);
    if (st) st.dirty = true;
  }

  /** Index `db` of session `name` again (every foreign database of it
   *  when `db` is null), and read its local ones again. */
  reindex(name, db = null) {
    const st = this.sessions.get(name);
    if (!st) return;
    st.dirty = true;
    for (const e of st.dbs.values()) {
      if (e.foreign && (db === null || e.db === db)) {
        e.stale = "asked";
        e.fresh = true; // ask the server, not DuckDB's copy of its schema
      }
    }
  }

  /**
   * Bring what is known of session `name` up to date with what is cheap to
   * read: which databases there are, and the local ones' contents. Foreign
   * databases are only noticed here (and given their cached index);
   * indexPending() reads them. Returns false when the session could not be
   * asked now (busy, ended, interrupted): what was known stays.
   */
  async refresh(name, sess) {
    if (!sess || !sess.alive) return false;
    const st = this.state(name, sess);
    this.expire(st);
    const hasForeign = [...st.dbs.values()].some((e) => e.foreign);
    // Nothing but queries ran since the last reading. With a foreign
    // database attached the reading is not cheap, so it is skipped.
    if (st.read && !st.dirty && hasForeign) return true;
    if (sess.busy || (this.deps.isBusy && this.deps.isBusy())) return false;
    const dbs = await this.ask(sess, DATABASES);
    if (!dbs || dbs.kind !== "rows") return false;
    const seen = objects(dbs).map((d) => ({ ...d, current: d.current === true || d.current === "true" }));
    for (const d of seen) {
      const foreign = this.isForeign(d);
      const key = d.path ? cacheKey(d.type, d.path) : null;
      let e = st.dbs.get(d.db);
      // The same name for something else: start over.
      if (e && (e.foreign !== foreign || e.key !== key || e.type !== d.type)) e = null;
      if (!e) {
        e = { db: d.db, type: d.type || "duckdb", path: d.path, key, foreign, part: null, digest: null, columns: 0, indexedAt: null, stale: null, over: null, error: null, indexing: false };
        st.dbs.set(d.db, e);
        if (foreign) this.restore(e);
      }
      e.current = d.current;
    }
    for (const db of [...st.dbs.keys()]) if (!seen.some((d) => d.db === db)) st.dbs.delete(db);
    this.expire(st);

    const local = [...st.dbs.values()].filter((e) => !e.foreign).map((e) => e.db);
    if (local.length) {
      const cols = await this.ask(sess, COLUMNS(local, false));
      if (!cols || cols.kind !== "rows") return false;
      const meta = await this.ask(sess, TABLES(local));
      const fns = await this.ask(sess, FUNCTIONS);
      if (!meta || !fns) return false;
      const facts = new Map();
      for (const m of meta.kind === "rows" ? objects(meta) : []) facts.set(`${m.db}\u0000${m.schema}\u0000${m.table}`, m);
      const rows = objects(cols);
      for (const db of local) {
        const e = st.dbs.get(db);
        const tables = tablesOf(rows.filter((r) => r.db === db).map((r) => ({ ...(facts.get(`${db}\u0000${r.schema}\u0000${r.table}`) || {}), ...r })));
        const functions = fns.kind === "rows" ? objects(fns).filter((f) => f.db === db).map(({ name: n, kind }) => ({ name: n, kind })) : [];
        this.put(e, tables, functions);
      }
    }
    st.read = true;
    st.dirty = false;
    this.prune();
    return true;
  }

  /** Mark the foreign indexes that are older than the age limit. */
  expire(st) {
    const maxAge = this.deps.maxAgeMs ? this.deps.maxAgeMs() : 0;
    if (!(maxAge > 0)) return;
    for (const e of st.dbs.values()) {
      if (e.foreign && !e.stale && e.indexedAt !== null && this.now() - e.indexedAt > maxAge) {
        e.stale = "older than the age limit";
        e.fresh = true;
      }
    }
  }

  /** `e` has more columns than one database may: it alone is left out. */
  tooLarge(e, columns) {
    if (e.over !== columns) {
      this.say(
        `${e.db} has ${columns.toLocaleString("en-US")} columns, over the limit of ${this.maxColumns.toLocaleString("en-US")} for one database: ` +
          `it is left out of completion and the CAT checks. The other databases are not affected.`,
      );
    }
    e.columns = columns;
    e.error = null;
    e.over = columns;
    e.part = null;
    e.digest = null;
  }

  /** Give `e` the tables just read, or mark it as too large. */
  put(e, tables, functions) {
    const columns = countColumns(tables);
    if (columns > this.maxColumns) return this.tooLarge(e, columns);
    e.columns = columns;
    e.error = null;
    e.over = null;
    // A database with nothing in it has no part: a file that is about to
    // create its tables should not be marked up for using them.
    e.part = tables.length || functions.length ? { tables, functions } : null;
    e.digest = e.part ? digest(e.part) : null;
  }

  /**
   * Index the foreign databases of session `name` that have no index, or
   * a stale one, one at a time. Stops (to be called again later) as soon
   * as a run wants the session. Resolves true when nothing is left to do.
   * A second call while one is under way joins it.
   */
  indexPending(name, sess) {
    const going = this.working.get(name);
    if (going) return going;
    const job = this.indexAll(name, sess).finally(() => this.working.delete(name));
    this.working.set(name, job);
    return job;
  }

  async indexAll(name, sess) {
    for (;;) {
      const st = this.sessions.get(name);
      if (!st || st.session !== sess || !sess.alive) return true;
      const e = [...st.dbs.values()].find((x) => x.foreign && (x.stale || (x.indexedAt === null && !x.over && !x.error)));
      if (!e) return true;
      if (sess.busy || (this.deps.isBusy && this.deps.isBusy())) return false;
      e.indexing = true;
      this.changed(name);
      let done;
      try {
        done = await this.indexOne(sess, e);
      } finally {
        e.indexing = false;
      }
      if (!done) {
        this.changed(name);
        return false;
      }
      e.stale = null;
      e.fresh = false;
      this.changed(name);
    }
  }

  /** Read one foreign database. False when interrupted: try again later. */
  async indexOne(sess, e) {
    const t0 = this.now();
    const where = e.path ? conninfo.where(e.path) : "in memory";
    const fail = (r) => {
      // A database that cannot be read is not asked again until something
      // changes; what was indexed before stays in use.
      e.error = conninfo.redact(`${r.type} Error: ${r.message}`);
      e.indexedAt = e.indexedAt ?? this.now();
      this.say(`could not index ${e.db} (${e.type}, ${where}): ${e.error}`);
      return true;
    };
    // DuckDB keeps its own copy of a PostgreSQL schema and does not look
    // again by itself. The function's name differs between versions.
    if (e.fresh && e.type === "postgres") {
      const r = await this.ask(sess, "CALL pg_clear_cache()");
      if (!r) return false;
      if (r.kind === "error" && !(await this.ask(sess, "CALL postgres_clear_cache()"))) return false;
    }
    const count = await this.ask(sess, COLUMN_COUNT(e.db));
    if (!count) return false;
    if (count.kind !== "rows") return fail(count);
    const n = Number(count.rows[0][0]);
    if (n > this.maxColumns) {
      this.tooLarge(e, n);
      e.indexedAt = this.now();
      this.drop(e);
      return true;
    }
    const cols = await this.ask(sess, COLUMNS([e.db], true));
    if (!cols) return false;
    if (cols.kind !== "rows") return fail(cols);
    let rows = objects(cols);
    let facts = "";
    if (e.type === "postgres") {
      const merged = await this.postgresFacts(sess, e, rows);
      if (!merged) return false;
      rows = merged.rows;
      facts = merged.note;
    }
    const tables = tablesOf(rows);
    this.put(e, tables, []);
    e.indexedAt = this.now();
    this.save(e);
    this.say(
      `indexed ${e.db} (${e.type}, ${where}): ${tables.length.toLocaleString("en-US")} tables, ${e.columns.toLocaleString("en-US")} columns in ${((this.now() - t0) / 1000).toFixed(1)} s${facts}`,
    );
    return true;
  }

  /** `rows` with what PostgreSQL itself says of each table added. Null
   *  when interrupted. A query that fails costs only what it would have
   *  added. */
  async postgresFacts(sess, e, rows) {
    const rel = await this.ask(sess, PG_RELATIONS(e.db));
    if (!rel) return null;
    const idx = await this.ask(sess, PG_INDEXES(e.db));
    if (!idx) return null;
    const missing = [rel, idx].filter((r) => r.kind !== "rows").map((r) => conninfo.redact(`${r.type} Error: ${r.message}`));
    const of = new Map(); // schema \0 table -> facts
    const at = (r) => {
      const k = `${r.schema}\u0000${r.table}`;
      if (!of.has(k)) of.set(k, {});
      return of.get(k);
    };
    if (rel.kind === "rows") {
      for (const r of objects(rel)) {
        const f = at(r);
        f.kind = r.kind === "v" || r.kind === "m" ? "view" : "table";
        if (r.rows !== null && r.rows !== undefined) f.rows = Number(r.rows);
      }
    }
    if (idx.kind === "rows") {
      const indexes = new Map(); // schema \0 table \0 index -> { row, cols }
      for (const r of objects(idx)) {
        const k = `${r.schema}\u0000${r.table}\u0000${r.index}`;
        if (!indexes.has(k)) indexes.set(k, { row: r, cols: [] });
        indexes.get(k).cols.push(r.col);
      }
      const yes = (v) => v === true || v === "true" || v === "t";
      for (const { row, cols } of indexes.values()) {
        const f = at(row);
        if (yes(row.is_primary)) f.primary_key = cols;
        else if (yes(row.is_unique)) (f.unique ||= []).push(cols);
        else (f.indexes ||= []).push(cols);
      }
    }
    const note = missing.length ? `; keys, indexes or row counts could not be read: ${missing.join("; ")}` : "";
    return { rows: rows.map((r) => ({ ...(of.get(`${r.schema}\u0000${r.table}`) || {}), ...r })), note };
  }

  changed(name) {
    if (this.deps.onChange) this.deps.onChange(name);
  }

  /** The parts of session `name` to send: `[{ database, digest, tables,
   *  functions }]`. */
  parts(name) {
    const st = this.sessions.get(name);
    if (!st) return [];
    return [...st.dbs.values()].filter((e) => e.part).map((e) => ({ database: e.db, digest: e.digest, tables: e.part.tables, functions: e.part.functions }));
  }

  /** What the Catalog shows of `db` in session `name`, or null. */
  status(name, db) {
    const st = this.sessions.get(name);
    const e = st && st.dbs.get(db);
    if (!e) return null;
    return {
      foreign: e.foreign,
      tables: e.part ? e.part.tables.length : 0,
      columns: e.columns,
      indexedAt: e.indexedAt,
      indexing: e.indexing,
      stale: e.stale,
      over: e.over,
      limit: this.maxColumns,
      error: e.error,
    };
  }

  // --- the cache ----------------------------------------------------------

  file(e) {
    return this.dir && e.key ? path.join(this.dir, `${e.key}.json`) : null;
  }

  /** Give `e` the index kept for its connection, if there is one. */
  restore(e) {
    const file = this.file(e);
    if (!file) return;
    let kept;
    try {
      kept = JSON.parse(fs.readFileSync(file, "utf8"));
    } catch {
      return; // none, or not readable: index it
    }
    if (!kept || kept.version !== CACHE_VERSION || kept.type !== e.type || !Array.isArray(kept.tables) || typeof kept.indexedAt !== "number") return;
    this.put(e, kept.tables, []);
    e.indexedAt = kept.indexedAt;
    this.say(`${e.db} (${e.type}): using the index of ${new Date(kept.indexedAt).toISOString()} (${kept.tables.length.toLocaleString("en-US")} tables)`);
  }

  /** Keep `e`'s index for the next window. What is written: the type, a
   *  description of the place without credentials, the time, the tables.
   *  Never the connection string. */
  save(e) {
    const file = this.file(e);
    if (!file || !e.part) return this.drop(e);
    try {
      fs.mkdirSync(this.dir, { recursive: true });
      const kept = { version: CACHE_VERSION, type: e.type, where: conninfo.where(e.path), indexedAt: e.indexedAt, columns: e.columns, tables: e.part.tables };
      fs.writeFileSync(`${file}.tmp`, JSON.stringify(kept));
      fs.renameSync(`${file}.tmp`, file);
    } catch (err) {
      this.say(`could not keep the index of ${e.db}: ${err.message ?? err}`);
    }
    this.prune();
  }

  drop(e) {
    const file = this.file(e);
    if (file) fs.rmSync(file, { force: true });
  }

  /** Delete the kept indexes of connections that are no longer attached
   *  or remembered, then the oldest ones past the cache's limits. */
  prune() {
    if (!this.dir) return;
    let names;
    try {
      names = fs.readdirSync(this.dir).filter((f) => /^[0-9a-f]{32}\.json$/.test(f));
    } catch {
      return;
    }
    const known = new Set();
    for (const st of this.sessions.values()) for (const e of st.dbs.values()) if (e.key) known.add(e.key);
    const remembered = this.deps.connections ? this.deps.connections() : null;
    for (const c of remembered || []) known.add(cacheKey(c.type, c.path));
    const files = [];
    for (const f of names) {
      const full = path.join(this.dir, f);
      if (remembered && !known.has(f.slice(0, 32))) {
        fs.rmSync(full, { force: true });
        continue;
      }
      try {
        const s = fs.statSync(full);
        files.push({ full, size: s.size, at: s.mtimeMs });
      } catch {
        // gone meanwhile
      }
    }
    files.sort((a, b) => b.at - a.at);
    let bytes = 0;
    files.forEach((f, i) => {
      bytes += f.size;
      if (i >= MAX_CACHE_FILES || bytes > MAX_CACHE_BYTES) fs.rmSync(f.full, { force: true });
    });
  }
}

module.exports = { CatalogIndex, MAX_COLUMNS, CACHE_DIR, cacheKey, changesStructure, mentions, DATABASES, COLUMNS, TABLES, PG_RELATIONS, PG_INDEXES };
