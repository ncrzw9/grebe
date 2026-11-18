// The DuckDB sessions of one window, and what each should hold when it
// starts. No `vscode` import, so it can be tested with plain Node.
//
//   - One session by default, shared by every file. A file may ask for a
//     session of its own with a comment in its leading comment block,
//     `-- grebe: session NAME` (optionally followed by a database): a
//     separate duckdb process, so a "prod" file and a "scratch" file cannot
//     see or disturb each other's tables.
//   - A session is kept while its CLI and database stay the same. The file
//     being run only decides the working directory (`.cd`), so relative
//     paths in SQL resolve from that file's folder without a restart.
//   - `:workspace:` as the database is a file in the editor's own storage for
//     this workspace: tables survive a window reload, a crash and a restart,
//     with nothing added to the project folder.
//   - What was attached to a session (from the Catalog or by an ATTACH in
//     SQL) is remembered, without passwords, and attached again when the
//     session next starts. Detaching does not lose it: the connection stays
//     listed as detached, is left alone when the session starts, and is
//     attached again when asked. Only forgetting removes it.
//   - Connections can also be declared in settings
//     (`grebe.duckdb.connections`); those are part of the default session's
//     list for as long as the setting names them.

"use strict";

const fs = require("fs");
const path = require("path");
const { Session } = require("./duckdb-session");
const conninfo = require("./conninfo");

const DEFAULT = "default";
const MEMORY = ":memory:";
const WORKSPACE = ":workspace:";

const lit = (s) => `'${String(s).replace(/'/g, "''")}'`;
const ident = (s) => `"${String(s).replace(/"/g, '""')}"`;

// `-- grebe: session NAME [DATABASE]`
const DIRECTIVE = /^\s*--\s*grebe:\s*session\s+([A-Za-z0-9_][A-Za-z0-9_.-]*)(?:\s+(\S.*?))?\s*$/i;

/**
 * The session a file asks for: `{ name, database }` (`database` null when
 * the comment names none), or null for the shared one. Only the leading
 * comment block is read, so a statement further down that happens to
 * contain the words changes nothing.
 */
function directive(text) {
  let seen = 0;
  for (const line of String(text).split("\n")) {
    if (++seen > 50) break;
    const t = line.trim();
    if (t === "") continue;
    if (!t.startsWith("--")) break;
    const m = DIRECTIVE.exec(t);
    if (m) return { name: m[1], database: m[2] || null };
  }
  return null;
}

/** The database setting a session called `name` uses. A named session never
 *  shares a database file with the default one (two processes cannot both
 *  write it): unless its comment names a database, it is in memory, or its
 *  own workspace file when that is what the setting asks for. */
function databaseFor(name, setting, own) {
  if (name === DEFAULT) return setting;
  if (own) return own;
  return setting === WORKSPACE ? WORKSPACE : MEMORY;
}

/** The setting as what the CLI opens. */
function resolveDatabase(database, { cwd, name = DEFAULT, storageDir = null } = {}) {
  if (database === MEMORY) return MEMORY;
  if (database === WORKSPACE) {
    if (!storageDir) return MEMORY; // no folder open: nowhere to keep it
    const file = name === DEFAULT ? "session.duckdb" : `session-${name.replace(/[^A-Za-z0-9_.-]/g, "_")}.duckdb`;
    return path.join(storageDir, file);
  }
  return path.isAbsolute(database) || !cwd ? database : path.join(cwd, database);
}

// Every database of a session that lives somewhere. An in-memory one has
// no path and nothing to come back to; the session's own file is told
// apart by its path, not by being the current database, which `USE` moves.
const ATTACHED = `SELECT database_name AS alias, path, type, readonly FROM duckdb_databases() WHERE NOT internal AND path IS NOT NULL`;

/** The ATTACH that brings `rec` back. */
function attachSql(rec, password) {
  const target = password === undefined ? rec.path : conninfo.withPassword(rec.path, password);
  const opts = [rec.type && rec.type !== "duckdb" ? `TYPE ${rec.type}` : null, rec.readOnly ? "READ_ONLY" : null].filter(Boolean);
  return `ATTACH ${lit(target)} AS ${ident(rec.alias)}${opts.length ? ` (${opts.join(", ")})` : ""}`;
}

class Sessions {
  /**
   * @param {{
   *   storageDir?: string|null,
   *   store?: { get(name: string): object[], set(name: string, recs: object[]): void },
   *   reattach?: () => boolean,
   *   declared?: () => object[],
   *   askPassword?: (rec: object) => Promise<string|undefined>,
   *   onStderr?: (text: string) => void,
   *   onExit?: (name: string, reason: string, session: Session) => void,
   *   onReattach?: (name: string, rec: object, error: string|null) => void,
   *   log?: (line: string) => void,
   * }} deps
   */
  constructor(deps = {}) {
    this.deps = deps;
    this.sessions = new Map(); // name -> Session
    this.opening = new Map(); // name -> Promise<Session>
    const mem = new Map();
    this.store = deps.store || { get: (n) => mem.get(n) || [], set: (n, r) => mem.set(n, r) };
  }

  /** The running session called `name`, or null. Never starts one. */
  live(name = DEFAULT) {
    const s = this.sessions.get(name);
    return s && s.alive ? s : null;
  }

  /** Names of the sessions running now. */
  names() {
    return [...this.sessions.keys()].filter((n) => this.live(n));
  }

  /**
   * The session called `name` for this CLI and database, started if it is
   * not running (or was running something else), with `cwd` as its working
   * directory. Two callers asking at once get the same session, after its
   * start-up and re-attaching are done.
   */
  async open({ name = DEFAULT, cli, database, cwd }) {
    for (;;) {
      const pending = this.opening.get(name);
      if (!pending) break;
      await pending.catch(() => {});
    }
    const have = this.sessions.get(name);
    if (have && have.alive && have.opts.cli === cli && have.opts.database === database) {
      if (cwd) await have.chdir(cwd);
      return have;
    }
    const job = this.start({ name, cli, database, cwd });
    this.opening.set(name, job);
    try {
      return await job;
    } finally {
      this.opening.delete(name);
    }
  }

  /**
   * The connections of `name`: what is stored for it and, for the default
   * session, what the settings declare. A declared connection takes its
   * place and options from the settings and its state (detached, the error
   * of a failed attach) from what is stored.
   */
  recs(name = DEFAULT) {
    const stored = this.store.get(name);
    const declared = name === DEFAULT && this.deps.declared ? this.deps.declared() : [];
    if (!declared.length) return stored;
    const out = declared.map((d) => {
      const had = stored.find((r) => r.alias === d.alias);
      const rec = { alias: d.alias, path: d.path, type: d.type || "duckdb", readOnly: !!d.readOnly };
      if (had && had.detached) rec.detached = true;
      if (had && had.error && had.path === d.path) rec.error = had.error;
      rec.declared = true;
      return rec;
    });
    return [...out, ...stored.filter((r) => !declared.some((d) => d.alias === r.alias))];
  }

  save(name, recs) {
    this.store.set(name, recs.map(({ declared, ...rec }) => rec));
  }

  async start({ name, cli, database, cwd }) {
    const old = this.sessions.get(name);
    if (old) old.dispose();
    if (database !== MEMORY && this.deps.storageDir && path.dirname(database) === this.deps.storageDir) {
      fs.mkdirSync(this.deps.storageDir, { recursive: true });
    }
    const sess = new Session({
      cli,
      database,
      cwd: cwd || process.cwd(),
      onStderr: this.deps.onStderr,
      onExit: (reason) => {
        if (this.sessions.get(name) === sess) this.sessions.delete(name);
        if (this.deps.onExit) this.deps.onExit(name, reason, sess);
      },
    });
    sess.name = name;
    this.sessions.set(name, sess);
    this.say(`starting ${cli} ${database}${name === DEFAULT ? "" : ` (session ${name})`} (cwd ${sess.opts.cwd})`);
    try {
      await sess.start();
    } catch (e) {
      if (this.sessions.get(name) === sess) this.sessions.delete(name);
      throw e;
    }
    await this.reattach(name, sess);
    return sess;
  }

  /** Attach again what this session had attached when it was last seen. */
  async reattach(name, sess) {
    if (this.deps.reattach && !this.deps.reattach()) return;
    const recs = this.recs(name);
    if (!recs.length) return;
    const own = sess.opts.database === MEMORY ? null : path.resolve(sess.opts.database);
    const out = [];
    for (const rec of recs) {
      // Detached on purpose: it waits to be asked for.
      if (rec.detached) {
        out.push(rec);
        continue;
      }
      // The session's own file, remembered while another database was in
      // USE: it is open already.
      if (own && !conninfo.isConnection(rec.path) && path.resolve(rec.path) === own) continue;
      const error = await this.bring(sess, rec);
      out.push(error ? { ...rec, error } : { alias: rec.alias, path: rec.path, type: rec.type, readOnly: rec.readOnly });
      this.say(error ? `could not re-attach ${rec.alias}: ${error}` : `re-attached ${rec.alias} (${conninfo.where(rec.path)})`);
      if (this.deps.onReattach) this.deps.onReattach(name, rec, error);
    }
    this.save(name, out);
  }

  /** ATTACH `rec` to `sess`, asking for a password when the server wants
   *  one. The error as text, or null. */
  async bring(sess, rec) {
    let r;
    try {
      r = await sess.run(attachSql(rec));
      if (r.kind === "error" && /password/i.test(r.message) && this.deps.askPassword && conninfo.isConnection(rec.path)) {
        const password = await this.deps.askPassword(rec);
        if (password !== undefined) r = await sess.run(attachSql(rec, password));
      }
    } catch (e) {
      r = { kind: "error", type: "Session", message: String(e.message ?? e) };
    }
    // DuckDB repeats the connection string in its error, password and
    // all: what is kept (and logged, and shown) has it taken out.
    return r.kind === "error" ? conninfo.redact(`${r.type} Error: ${r.message}`) : null;
  }

  /**
   * Attach a listed connection that is not attached now: one that was
   * detached, or one that could not be brought back. The error as text, or
   * null when it is attached.
   */
  async attach(name, alias) {
    const sess = this.live(name);
    if (!sess) return "the session is not running";
    const rec = this.recs(name).find((r) => r.alias === alias);
    if (!rec) return `no connection called ${alias}`;
    const error = await this.bring(sess, rec);
    this.say(error ? `could not attach ${alias}: ${error}` : `attached ${alias} (${conninfo.where(rec.path)})`);
    if (error) {
      this.save(name, this.recs(name).map((r) => (r.alias === alias ? { ...r, error } : r)));
      return error;
    }
    await this.remember(name);
    return null;
  }

  /**
   * Note what the session has attached now, so it can be attached again.
   * Called after a run and after the Catalog attaches or detaches. A
   * connection that is no longer attached stays listed, as detached (or
   * with the reason it could not be brought back), until it is attached
   * again or forgotten.
   */
  async remember(name = DEFAULT) {
    const sess = this.live(name);
    if (!sess || sess.busy) return;
    let r;
    try {
      r = await sess.run(ATTACHED);
    } catch {
      return;
    }
    if (r.kind !== "rows") return;
    const at = (c) => r.columns.indexOf(c);
    const own = sess.opts.database === MEMORY ? null : path.resolve(sess.opts.database);
    const now = r.rows
      .map((row) => ({
        alias: row[at("alias")],
        path: conninfo.withoutPassword(String(row[at("path")])),
        type: row[at("type")] || "duckdb",
        readOnly: row[at("readonly")] === true || row[at("readonly")] === "true",
      }))
      .filter((rec) => conninfo.isConnection(rec.path) || !own || path.resolve(rec.path) !== own);
    const before = this.recs(name);
    const waiting = before
      .filter((p) => !now.some((n) => n.alias === p.alias))
      .map(({ declared, ...p }) => (p.error || p.detached ? p : { ...p, detached: true }));
    const next = [...now, ...waiting];
    const plain = (recs) => JSON.stringify(recs.map(({ declared, ...rec }) => rec));
    if (plain(next) !== JSON.stringify(this.store.get(name))) this.save(name, next);
  }

  /** The connections of `name`:
   *  `[{ alias, path, type, readOnly, detached?, error?, declared? }]`. */
  remembered(name = DEFAULT) {
    return this.recs(name);
  }

  /** Take `alias` off the list. (One the settings declare stays while they
   *  do.) */
  forget(name, alias) {
    this.store.set(name, this.store.get(name).filter((r) => r.alias !== alias));
  }

  /** End the session called `name` (every session when null). The next
   *  open() starts a new one. */
  end(name = null) {
    for (const [n, s] of [...this.sessions]) {
      if (name !== null && n !== name) continue;
      this.sessions.delete(n);
      s.dispose();
    }
  }

  say(line) {
    if (this.deps.log) this.deps.log(line);
  }
}

module.exports = { Sessions, directive, databaseFor, resolveDatabase, attachSql, DEFAULT, MEMORY, WORKSPACE };
