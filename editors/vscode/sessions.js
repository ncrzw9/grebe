// The DuckDB sessions of one window, and the connections each attaches. No
// `vscode` import, so it can be tested with plain Node.
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
//   - The connections a session attaches are the entries of the
//     `grebe.duckdb.connections` setting for it (connections.js). Starting
//     a session never waits for one: the local files are attached right
//     after it starts, in the background and without asking anything, and
//     a server is attached when it is first needed (a statement refers to
//     it, its Catalog node is opened, Attach Again). Only then is a
//     password asked for. Each attach has a time limit, so a server that
//     does not answer holds up nothing for longer than that.
//   - An ATTACH typed in SQL is not saved: it lasts as long as the session.

"use strict";

const fs = require("fs");
const path = require("path");
const { Session } = require("./duckdb-session");
const conninfo = require("./conninfo");
const connections = require("./connections");

const DEFAULT = "default";
const MEMORY = ":memory:";
const WORKSPACE = ":workspace:";

// The longest one attach may take before it is cancelled and reported.
const ATTACH_TIMEOUT_MS = 20000;
// libpq gives up on a server that does not answer after this long (it can
// try twice, with and without SSL), well inside ATTACH_TIMEOUT_MS: an
// interrupt does not stop a connection attempt, so without it the session
// would have to be ended.
const CONNECT_TIMEOUT_S = 8;

/** Where a file really is: DuckDB reports a database by its resolved path
 *  (on macOS the temporary folder is behind a link), and Windows does not
 *  tell upper case from lower. */
function real(file) {
  let p = path.resolve(file);
  try {
    p = fs.realpathSync.native(p);
  } catch {
    // Not there (yet): the path as written.
  }
  return process.platform === "win32" ? p.toLowerCase() : p;
}

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

// The databases a session has attached, its own and the in-memory ones
// left out by the caller where that matters.
const ATTACHED = `SELECT database_name AS alias, path, type, readonly FROM duckdb_databases() WHERE NOT internal`;

/** The ATTACH for `rec`. A PostgreSQL connection is given a connect
 *  timeout (see CONNECT_TIMEOUT_S). */
function attachSql(rec, password) {
  let target = rec.path;
  if (rec.type === "postgres" && conninfo.isConnection(target)) target = conninfo.withConnectTimeout(target, CONNECT_TIMEOUT_S);
  if (password !== undefined) target = conninfo.withPassword(target, password);
  const opts = [rec.type && rec.type !== "duckdb" ? `TYPE ${rec.type}` : null, rec.readOnly ? "READ_ONLY" : null].filter(Boolean);
  return `ATTACH ${lit(target)} AS ${ident(rec.alias)}${opts.length ? ` (${opts.join(", ")})` : ""}`;
}

const yes = (v) => v === true || v === "true";

class Sessions {
  /**
   * @param {{
   *   storageDir?: string|null,
   *   reattach?: () => boolean,
   *   connections?: () => object[],
   *   askPassword?: (entry: object) => Promise<string|undefined>,
   *   onStderr?: (text: string) => void,
   *   onExit?: (name: string, reason: string, session: Session) => void,
   *   onChange?: (name: string) => void,
   *   log?: (line: string) => void,
   * }} deps
   * `connections` gives the setting's entries (connections.merge), of every
   * session. `onChange` is told when a connection of session `name` starts
   * or stops being attached, or failed to be.
   */
  constructor(deps = {}) {
    this.deps = deps;
    this.sessions = new Map(); // name -> Session
    this.opening = new Map(); // name -> Promise<Session>
    this.pending = new Map(); // session \0 alias -> Promise<error|null>: attaches under way
    this.errors = new Map(); // session \0 alias -> why its last attach failed
    this.applied = this.all(); // the entries reconcile() last saw
    this.reconciling = Promise.resolve();
  }

  /** Every entry of the setting. */
  all() {
    return this.deps.connections ? this.deps.connections() : [];
  }

  /** The entries of session `name`. */
  entries(name = DEFAULT) {
    return this.all().filter((e) => connections.sessionOf(e) === name);
  }

  /** The entry of session `name` called `alias`, in any case. */
  entry(name, alias) {
    const k = connections.keyOf({ session: name, alias });
    return this.entries(name).find((e) => connections.keyOf(e) === k) || null;
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
   * directory. Two callers asking at once get the same session. Resolves
   * as soon as the duckdb process is up: connections are attached after.
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
    for (const k of [...this.errors.keys()]) if (k.startsWith(`${name}\u0000`)) this.errors.delete(k);
    sess.attaching = this.attachLocal(name, sess);
    return sess;
  }

  /** Attach the local files of `name` that are not marked detached, in the
   *  background, asking nothing. Servers wait until they are needed. With
   *  grebe.duckdb.reattach off, nothing. */
  attachLocal(name, sess) {
    if (this.deps.reattach && !this.deps.reattach()) return Promise.resolve();
    const own = sess.opts.database === MEMORY ? null : real(sess.opts.database);
    const jobs = [];
    for (const e of this.entries(name)) {
      if (e.detached || connections.isServer(e)) continue;
      // The session's own file is open already.
      if (own && real(e.path) === own) continue;
      jobs.push(this.ensure(name, e.alias, { prompt: false, check: false }));
    }
    return Promise.all(jobs).then(() => {});
  }

  /** What the Catalog shows of `alias`: whether an attach is under way,
   *  and why the last one failed. */
  state(name, alias) {
    const k = connections.keyOf({ session: name, alias });
    return { attaching: this.pending.has(k), error: this.errors.get(k) || null };
  }

  /**
   * Attach the entry `alias` of session `name` unless it is attached. A
   * second call while one is under way joins it. `prompt`: a server that
   * asks for a password may ask the user (their own action asked for the
   * connection). `check`: look first whether it is attached. Resolves to
   * the error as text, or null.
   */
  ensure(name, alias, { prompt = true, check = true } = {}) {
    const sess = this.live(name);
    if (!sess) return Promise.resolve("the session is not running");
    const e = this.entry(name, alias);
    if (!e) return Promise.resolve(`no connection called ${alias}`);
    const k = connections.keyOf(e);
    if (this.pending.has(k)) return this.pending.get(k);
    const job = (async () => {
      if (check && (await this.attachedNow(sess)).has(e.alias.toLowerCase())) return null;
      this.changed(name);
      const error = await this.bring(sess, e, prompt);
      this.say(error ? `could not attach ${e.alias}: ${error}` : `attached ${e.alias} (${conninfo.where(e.path)})`);
      return error;
    })()
      .catch((err) => String(err.message ?? err))
      .then((error) => {
        if (error) this.errors.set(k, error);
        else this.errors.delete(k);
        return error;
      })
      .finally(() => {
        this.pending.delete(k);
        this.changed(name);
      });
    this.pending.set(k, job);
    return job;
  }

  /** ATTACH `e` to `sess`, within ATTACH_TIMEOUT_MS. With `prompt`, a
   *  server that wants a password is given one asked of the user; the
   *  session is free while they type. The error as text, or null. */
  async bring(sess, e, prompt) {
    const attempt = async (password) => {
      try {
        return await sess.run(attachSql(e, password), { timeoutMs: ATTACH_TIMEOUT_MS });
      } catch (err) {
        return { kind: "error", type: "Session", message: String(err.message ?? err) };
      }
    };
    let r = await attempt();
    if (r.kind === "error" && /password/i.test(r.message) && prompt && this.deps.askPassword && conninfo.isConnection(e.path)) {
      const password = await this.deps.askPassword(e);
      if (password === undefined) return "no password was given";
      r = await attempt(password);
    }
    if (r.kind === "cancelled") return `no answer within ${ATTACH_TIMEOUT_MS / 1000} s; the attach was cancelled`;
    // DuckDB repeats the connection string in its error, password and
    // all: what is kept (and logged, and shown) has it taken out.
    return r.kind === "error" ? conninfo.redact(`${r.type} Error: ${r.message}`) : null;
  }

  /** What `sess` has attached now: lower-cased alias -> `{ alias, path,
   *  type, readOnly }`. Empty when it cannot be asked. */
  async attachedNow(sess) {
    const out = new Map();
    let r;
    try {
      r = await sess.run(ATTACHED);
    } catch {
      return out;
    }
    if (r.kind !== "rows") return out;
    const at = (c) => r.columns.indexOf(c);
    for (const row of r.rows) {
      const alias = String(row[at("alias")]);
      const p = row[at("path")];
      out.set(alias.toLowerCase(), {
        alias,
        path: p === null || p === undefined ? null : conninfo.plain(String(p)),
        type: row[at("type")] || "duckdb",
        readOnly: yes(row[at("readonly")]),
      });
    }
    return out;
  }

  /** DETACH `alias` from session `name`, after any attach of it under way.
   *  The error as text, or null (also when it was not attached). */
  async detach(name, alias) {
    const k = connections.keyOf({ session: name, alias });
    if (this.pending.has(k)) await this.pending.get(k);
    this.errors.delete(k);
    const sess = this.live(name);
    if (!sess) return null;
    let r;
    try {
      r = await sess.run(`DETACH DATABASE IF EXISTS ${ident(alias)}`);
    } catch (err) {
      return String(err.message ?? err);
    }
    this.changed(name);
    if (r.kind === "error") return `${r.type} Error: ${r.message}`;
    this.say(`detached ${alias}`);
    return null;
  }

  /**
   * Before `sql` runs in session `name`: attach the connections it refers
   * to that are not attached (and not marked detached). Each within its
   * time limit; a server may ask for a password, the run being the user's
   * own. Resolves to `[{ alias, error }]` for those that could not be.
   */
  async prepare(name, sql) {
    const candidates = this.entries(name).filter((e) => !e.detached);
    if (!candidates.length) return [];
    const wanted = connections.references(sql, candidates.map((e) => e.alias));
    if (!wanted.length) return [];
    const sess = this.live(name);
    if (!sess) return [];
    const have = await this.attachedNow(sess);
    const out = [];
    for (const a of wanted) {
      if (have.has(a)) continue;
      const e = this.entry(name, a);
      const error = await this.ensure(name, e.alias, { check: false });
      if (error) out.push({ alias: e.alias, error });
    }
    return out;
  }

  /**
   * The setting changed: bring every running session in line with it
   * (connections.plan), against what each has attached now. Safe to call
   * for a change this extension made itself: what is attached already is
   * left alone. Calls run one after another.
   */
  reconcile() {
    const job = this.reconciling.then(() => this.reconcileNow());
    this.reconciling = job.catch(() => {});
    return job;
  }

  async reconcileNow() {
    const prev = this.applied;
    const next = this.all();
    this.applied = next;
    const mine = (list, name) => list.filter((e) => connections.sessionOf(e) === name);
    const plainOf = (list) => JSON.stringify(list.map(({ scope, ...e }) => e));
    for (const name of this.names()) {
      const p = mine(prev, name);
      const n = mine(next, name);
      if (plainOf(p) === plainOf(n)) continue;
      // An entry that changed or went: its old failure says nothing now.
      for (const e of p) {
        const was = n.find((x) => connections.keyOf(x) === connections.keyOf(e));
        if (!was || !connections.sameTarget(was, e)) this.errors.delete(connections.keyOf(e));
      }
      const sess = this.live(name);
      if (!sess) continue;
      const actions = connections.plan(p, n, await this.attachedNow(sess));
      for (const a of actions) {
        if (a.do === "detach") await this.detach(name, a.alias);
        else await this.ensure(name, a.entry.alias, { prompt: false, check: false });
      }
      this.changed(name);
    }
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

  changed(name) {
    if (this.deps.onChange) this.deps.onChange(name);
  }

  say(line) {
    if (this.deps.log) this.deps.log(line);
  }
}

module.exports = { Sessions, directive, databaseFor, resolveDatabase, attachSql, ATTACH_TIMEOUT_MS, CONNECT_TIMEOUT_S, DEFAULT, MEMORY, WORKSPACE };
