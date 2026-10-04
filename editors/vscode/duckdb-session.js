// A long-lived `duckdb` CLI process that runs one statement at a time. No
// `vscode` import, so it can be tested with plain Node.
//
// Why this shape, from measuring the CLI over a pipe:
//
//   - One process per session, not one per statement: an in-memory
//     database, TEMP tables and SET options only live as long as the process.
//   - Statements go in one at a time, each followed by `.print <marker>`:
//     `-json` output carries no statement tag and DDL prints nothing, so
//     without a marker there is no telling where one statement's output ends.
//     A round-trip after start-up measured under 1 ms.
//   - The session sets its own output mode before the first marker and
//     discards everything before it, because a user's ~/.duckdbrc may switch
//     modes, turn on .echo/.timer, or print.
//   - Errors are requested as JSON (`errors_as_json`) but not relied on: a
//     2.0 parser error is prose even with it on.

"use strict";

const { spawn, execFile } = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const { parse, readTable } = require("./lenient-json");
const watchdog = require("./watchdog");

const SETUP = [
  ".echo off",
  ".timer off",
  ".changes off",
  ".bail off",
  ".mode json",
  "SET errors_as_json = true;",
];

// How long start-up may take before the session gives up. A program that
// is not DuckDB may never print the marker (`yes`, a script that sleeps):
// without a limit `start()` waited forever. Generous, because a ~/.duckdbrc
// may INSTALL an extension over the network.
const START_TIMEOUT_MS = 20000;

// Cancelling a statement. Measured on the 1.5.5 CLI over a pipe:
//
//   - SIGINT to a CLI reading a pipe ends the process, and the session with
//     it. Run with `-interactive`, SIGINT stops the query instead (3-9 ms on
//     a 30M-row join, an 830 MB CSV scan, a COPY to Parquet), prints nothing
//     for it, keeps the process and its TEMP tables, and the marker after it
//     still arrives. SIGINT while idle does nothing.
//   - `-interactive` also turns on the shell's history file, which would put
//     every statement run here into ~/.duckdb_history; DUCKDB_HISTORY points
//     it at /dev/null.
//   - Errors stay plain JSON and no progress bar is drawn: neither is a
//     terminal.
//
// Windows has no SIGINT to send another process (Node's kill() terminates
// it there), so cancelling on Windows ends the session.
const INTERRUPTIBLE = process.platform !== "win32";

// How long a cancelled statement may take to stop before the process is
// killed instead. A query normally stops within milliseconds; one that
// does not (stuck in a call that does not check for interrupts) should not
// hold the session forever.
const CANCEL_GRACE_MS = 3000;

// Every session not yet ended, so they can all be ended together.
const live = new Set();

/** Terminal colour codes removed: the CLI colours its "Loading resources
 *  from ~/.duckdbrc" line even when writing to a pipe. */
function plain(text) {
  return text.replace(/\x1b\[[0-9;]*m/g, "");
}

/** What the CLI said on its way out, fit to show: colour codes and its
 *  "-- Loading resources from ~/.duckdbrc" banner removed ("Encountered
 *  errors while executing init file" already names the file). */
function farewell(text) {
  return plain(text).replace(/^-- Loading resources from .*\n?/m, "").trim();
}

/** A failure to launch `cli`, as something a person can act on. */
function spawnMessage(cli, e) {
  if (e.code === "ENOENT") return `No duckdb CLI at "${cli}" (not found).`;
  if (e.code === "EACCES") return `"${cli}" is not executable.`;
  return `Could not launch "${cli}": ${e.message}`;
}

/**
 * `duckdb --version`, checked. Resolves `{ text, error }`: `text` is e.g.
 * "v1.5.5 (Variegata) d8cdaa33fd"; `error` is null, or why `cli` cannot be
 * used -- not found, not executable, no answer, or not a DuckDB CLI.
 */
function checkCli(cli) {
  return new Promise((resolve) => {
    execFile(cli, ["--version"], { timeout: 10000 }, (err, stdout, stderr) => {
      const text = String(stdout || "").trim();
      if (err && err.code === "ENOENT") return resolve({ text: null, error: spawnMessage(cli, err), missing: true });
      if (err && err.code === "EACCES") return resolve({ text: null, error: spawnMessage(cli, err) });
      if (err && err.killed) return resolve({ text: null, error: `"${cli} --version" did not answer within 10 s.` });
      if (err) {
        // `--version` reads ~/.duckdbrc too, so a broken rc file shows here.
        const said = farewell(String(stderr || "")) || err.message;
        return resolve({ text: null, error: `"${cli} --version" failed: ${said}` });
      }
      if (!parseVersion(text)) {
        return resolve({ text: null, error: `"${cli}" does not look like the DuckDB CLI ("--version" printed "${text.split("\n")[0].slice(0, 80)}").` });
      }
      resolve({ text, error: null });
    });
  });
}

/** `duckdb --version`, e.g. "v1.5.5 (Variegata) d8cdaa33fd", or null. */
async function cliVersion(cli) {
  return (await checkCli(cli)).text;
}

/** `v1.5.5 ...` -> [1, 5, 5]; anything unrecognised -> null. */
function parseVersion(text) {
  const m = /v?(\d+)\.(\d+)\.(\d+)/.exec(text || "");
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

const ERROR_HEAD = /^(?:[A-Z][A-Za-z ]*? Error: |\{"exception_type")/;

/**
 * Classify what one statement printed.
 *
 *   { kind: "ok" }                                   DDL/DML: printed nothing
 *   { kind: "rows", columns, rows }                  a result set
 *   { kind: "error", message, type, subtype, position }
 *   { kind: "text", text }                           anything else (EXPLAIN)
 */
function classify(output) {
  const text = output.replace(/\s+$/, "");
  if (text.trim() === "") return { kind: "ok" };
  if (ERROR_HEAD.test(text)) return errorOf(text);
  if (text.startsWith("[")) {
    try {
      return { kind: "rows", ...readTable(text) };
    } catch {
      // Not a result array after all; show it as it came.
    }
  }
  return { kind: "text", text };
}

function errorOf(text) {
  // `Parser Error: {...}` on 1.5.5, bare `{...}` for other types.
  const brace = text.indexOf("{");
  const prefix = brace > 0 ? text.slice(0, brace).replace(/: $/, "") : "";
  if (brace >= 0) {
    try {
      const obj = Object.fromEntries(parse(text.slice(brace)).pairs);
      const position = Number.parseInt(obj.position, 10);
      return {
        kind: "error",
        type: obj.exception_type || prefix.replace(/ Error$/, "") || "Error",
        subtype: obj.error_subtype || null,
        message: obj.exception_message || text,
        position: Number.isNaN(position) ? null : position,
      };
    } catch {
      // Prose that happens to contain a brace; fall through.
    }
  }
  const head = /^([A-Z][A-Za-z ]*?) Error: /.exec(text);
  return {
    kind: "error",
    type: head ? head[1] : "Error",
    subtype: null,
    message: head ? text.slice(head[0].length) : text,
    position: null,
  };
}

/** A statement as a query body: trailing whitespace and one final `;`
 *  removed. Callers wrap it on lines of its own, so a trailing `-- comment`
 *  cannot swallow the closing parenthesis. */
function queryBody(sql) {
  return sql.replace(/\s+$/, "").replace(/;$/, "");
}

class Session {
  /**
   * @param {{ cli: string, database: string, cwd: string,
   *           onStderr?: (text: string) => void,
   *           onExit?: (reason: string) => void,
   *           startTimeoutMs?: number, cancelGraceMs?: number }} opts
   * `onStderr` hears, as it arrives, what the CLI writes to stderr during
   * start-up or between statements (a statement's own stderr is its result).
   * `onExit` hears the process ending on its own (a crash, a kill, an rc
   * file that failed) -- not `dispose()`.
   */
  constructor(opts) {
    this.opts = opts;
    this.proc = null;
    // stdout as raw chunks since the last marker. Strings are not built
    // until a statement's output is complete: appending to one growing
    // string and searching it made V8 re-flatten the whole thing on every
    // chunk -- measured ~500 ms for a 100,000-row result that arrives in
    // ~190 ms as bytes.
    this.chunks = [];
    this.err = "";
    this.waiting = null; // { marker, resolve, reject }
    this.queue = Promise.resolve();
    this.closedWith = null;
    this.starting = false;
  }

  get key() {
    return JSON.stringify([this.opts.cli, this.opts.database, this.opts.cwd]);
  }

  get alive() {
    return this.proc !== null && this.closedWith === null;
  }

  /** Start the process and wait until its setup has been applied. */
  async start() {
    const args = INTERRUPTIBLE ? ["-interactive", this.opts.database] : [this.opts.database];
    const proc = spawn(this.opts.cli, args, {
      cwd: this.opts.cwd,
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
      env: INTERRUPTIBLE ? { ...process.env, DUCKDB_HISTORY: "/dev/null" } : process.env,
    });
    this.proc = proc;
    live.add(this);
    watchdog.watch(proc.pid);
    proc.stderr.setEncoding("utf8");
    proc.stdout.on("data", (d) => {
      this.chunks.push(d);
      // Start-up output is thrown away, so keep only what the marker search
      // needs: a program that is not DuckDB may print without end.
      if (this.starting && this.chunks.length > 2) this.chunks.splice(0, this.chunks.length - 2);
      this.pump();
    });
    proc.stderr.on("data", (d) => {
      this.err += d;
      // While a statement runs, its stderr is its result (the caller has
      // it); anything else -- start-up, or between statements -- is news.
      if (this.opts.onStderr && (this.starting || !this.waiting)) this.opts.onStderr(plain(d));
    });
    proc.on("error", (e) => this.close(spawnMessage(this.opts.cli, e)));
    proc.on("exit", (code, signal) => {
      live.delete(this);
      watchdog.unwatch(proc.pid);
      if (this.closedWith !== null) return; // dispose() or a start-up timeout
      const said = farewell(this.err + Buffer.concat(this.chunks).toString("utf8"));
      const reason = said || `duckdb exited (${signal || `code ${code}`})`;
      const wasStarting = this.starting;
      this.close(reason);
      // A failed start rejects start(); only a session that was up is news.
      if (!wasStarting && this.opts.onExit) this.opts.onExit(reason);
    });
    proc.stdin.on("error", () => {}); // EPIPE after exit; `exit` reports it
    const ms = this.opts.startTimeoutMs ?? START_TIMEOUT_MS;
    const timer = setTimeout(() => {
      this.close(`"${this.opts.cli}" did not answer within ${ms / 1000} s. Is it the DuckDB CLI?`);
      if (proc.exitCode === null) proc.kill();
    }, ms);
    this.starting = true;
    try {
      // Everything before the first marker is the rc file's business.
      await this.send(SETUP.join("\n"));
    } finally {
      this.starting = false;
      clearTimeout(timer);
    }
  }

  /**
   * Run one statement. Resolves with `classify`'s shape plus `ms`, or
   * `{ kind: "cancelled", reason }` when cancel() stopped it ("cancel") or
   * `timeoutMs` ran out ("timeout"). Rejects when the session ends under it;
   * the error has `ended: true`, and `cancelled` when ending it was how a
   * cancel had to be done.
   */
  run(sql, { timeoutMs = 0 } = {}) {
    const job = this.queue.then(async () => {
      const started = process.hrtime.bigint();
      const trimmed = sql.replace(/\s+$/, "");
      const terminated = trimmed.endsWith(";") ? trimmed : `${trimmed}\n;`;
      const sent = this.send(terminated);
      const timer = timeoutMs > 0 ? setTimeout(() => this.cancel("timeout"), timeoutMs) : null;
      let reply;
      try {
        reply = await sent;
      } finally {
        clearTimeout(timer);
      }
      const { out, err, cancelled } = reply;
      let result = classify(err.trim() ? `${err}\n${out}` : out);
      // A cancelled query prints nothing. If it printed rows or an error
      // anyway, it finished before the interrupt reached it: that result is
      // real and is kept.
      if (cancelled && (result.kind === "ok" || (result.kind === "error" && /interrupt/i.test(result.type)))) {
        result = { kind: "cancelled", reason: cancelled };
      }
      result.ms = Number(process.hrtime.bigint() - started) / 1e6;
      return result;
    });
    this.queue = job.catch(() => {});
    return job;
  }

  /** A statement is running (not start-up, not idle). */
  get busy() {
    return this.waiting !== null && !this.starting;
  }

  /** The text being run now, or null. */
  get running() {
    return this.busy ? this.waiting.text : null;
  }

  /**
   * Stop the statement in flight. Resolves true if there was one. Where the
   * CLI can be interrupted the session survives; if the statement has not
   * stopped within the grace period, or on Windows, the process is ended
   * instead and run() rejects with `cancelled` and `ended` set.
   */
  cancel(reason = "cancel") {
    const w = this.waiting;
    if (!w || this.starting || this.closedWith !== null) return false;
    if (w.cancelled) return true;
    w.cancelled = reason;
    if (!INTERRUPTIBLE) {
      this.end(`cancelled; on Windows that ends the DuckDB session`);
      return true;
    }
    this.proc.kill("SIGINT");
    const grace = this.opts.cancelGraceMs ?? CANCEL_GRACE_MS;
    const timer = setTimeout(() => {
      if (this.waiting === w) this.end(`cancelled; DuckDB did not stop within ${grace / 1000} s, so the session was ended`);
    }, grace);
    if (timer.unref) timer.unref();
    return true;
  }

  /** End the process now, rejecting the statement in flight as cancelled. */
  end(reason) {
    this.close(reason, { cancelled: true });
    if (this.proc && this.proc.exitCode === null) this.proc.kill("SIGKILL");
  }

  /**
   * Column names and types of a query without running it: `DESCRIBE` only
   * plans. `DESCRIBE`, `SUMMARIZE` and `SHOW` cannot be described directly,
   * so on a parser error the query is wrapped as `FROM (...)`, which can.
   * Resolves `[{ name, type }]`, or null when neither form works.
   */
  async describe(sql) {
    const body = queryBody(sql);
    for (const text of [`DESCRIBE\n${body}\n;`, `DESCRIBE FROM (\n${body}\n);`]) {
      const r = await this.run(text);
      if (r.kind === "rows") {
        const name = r.columns.indexOf("column_name");
        const type = r.columns.indexOf("column_type");
        return r.rows.map((row) => ({ name: row[name], type: row[type] }));
      }
      if (!(r.kind === "error" && r.type === "Parser")) return null;
    }
    return null;
  }

  /**
   * Write a query's full result to `file` with DuckDB's own `COPY`, so the
   * file has every row and exact types (not just the rows on screen).
   * `format`: csv | tsv | parquet | json (newline-delimited). The query runs
   * again, so the caller only offers this for statements that change
   * nothing. Resolves `classify`'s shape.
   *
   * A cancelled export leaves nothing behind. Measured on 1.5.5: writing to
   * a new path, COPY creates the file at once, so a cancel left a truncated
   * file there; writing over an existing one, it writes `tmp_<name>` and
   * renames at the end, so a cancel kept the old file and left an empty
   * `tmp_<name>`. Whatever this export created is removed; a file that was
   * already there is not touched.
   */
  async exportTo(sql, file, format) {
    const scratch = path.join(path.dirname(file), `tmp_${path.basename(file)}`);
    const existed = { file: fs.existsSync(file), scratch: fs.existsSync(scratch) };
    const tidy = () => {
      for (const [p, was] of [[file, existed.file], [scratch, existed.scratch]]) {
        if (!was) fs.rmSync(p, { force: true });
      }
    };
    let r;
    try {
      r = await this.copy(sql, file, format);
    } catch (e) {
      if (e && e.cancelled) tidy();
      throw e;
    }
    if (r.kind === "cancelled") tidy();
    return r;
  }

  async copy(sql, file, format) {
    const opts = {
      csv: "FORMAT csv, HEADER true",
      tsv: "FORMAT csv, HEADER true, DELIMITER '\\t'",
      parquet: "FORMAT parquet",
      json: "FORMAT json",
    }[format];
    if (!opts) throw new Error(`unknown export format ${format}`);
    const target = `'${file.replace(/'/g, "''")}'`;
    const body = queryBody(sql);
    let r = await this.run(`COPY (\n${body}\n) TO ${target} (${opts});`);
    if (r.kind === "error" && r.type === "Parser") {
      r = await this.run(`COPY (FROM (\n${body}\n)) TO ${target} (${opts});`);
    }
    return r;
  }

  /** Write `text`, then a marker; resolve with what was printed before it. */
  send(text) {
    if (this.closedWith !== null) return Promise.reject(new Error(this.closedWith));
    const marker = `@@grebe-${crypto.randomBytes(8).toString("hex")}@@`;
    return new Promise((resolve, reject) => {
      this.waiting = { marker, resolve, reject, text };
      this.proc.stdin.write(`${text}\n.print ${marker}\n`);
      this.pump();
    });
  }

  pump() {
    const w = this.waiting;
    if (!w || this.chunks.length === 0) return;
    // The marker is ASCII and at most one chunk boundary can split it, so
    // only the newest chunk plus the tail of the one before need searching.
    const needle = Buffer.from(`${w.marker}\n`);
    const last = this.chunks[this.chunks.length - 1];
    const prev = this.chunks.length > 1 ? this.chunks[this.chunks.length - 2] : null;
    const tailLen = prev ? Math.min(prev.length, needle.length - 1) : 0;
    const window = tailLen ? Buffer.concat([prev.subarray(prev.length - tailLen), last]) : last;
    const hit = window.indexOf(needle);
    if (hit < 0) return;
    const all = Buffer.concat(this.chunks);
    const at = all.length - window.length + hit;
    const out = all.subarray(0, at).toString("utf8");
    const rest = all.subarray(at + needle.length);
    this.chunks = rest.length ? [rest] : [];
    this.waiting = null;
    const cancelled = w.cancelled || null;
    // stderr and stdout are separate pipes. Anything the CLI wrote to
    // stderr for this statement was written before the marker reached
    // stdout, so it is readable by the time the event loop turns once more.
    setImmediate(() => {
      const err = this.err;
      this.err = "";
      w.resolve({ out, err, cancelled });
    });
  }

  close(reason, { cancelled = false } = {}) {
    if (this.closedWith !== null) return;
    this.closedWith = reason;
    if (this.waiting) {
      const e = new Error(reason);
      e.ended = true;
      e.cancelled = cancelled || Boolean(this.waiting.cancelled);
      this.waiting.reject(e);
      this.waiting = null;
    }
  }

  /**
   * End the process. Any statement in flight rejects. SIGTERM stops even a
   * busy CLI within ~100 ms (measured); SIGKILL follows if it has not.
   */
  dispose() {
    this.close("session closed");
    const proc = this.proc;
    if (!proc || proc.exitCode !== null) return;
    proc.kill();
    const timer = setTimeout(() => {
      if (proc.exitCode === null) proc.kill("SIGKILL");
    }, 2000);
    if (timer.unref) timer.unref();
  }

  /** Cancel whatever any session is running. */
  static cancelAll(reason = "cancel") {
    let any = false;
    for (const s of live) if (s.cancel(reason)) any = true;
    return any;
  }

  /** End every session still running, as the extension shuts down. */
  static disposeAll() {
    for (const s of [...live]) s.dispose();
  }
}

module.exports = { Session, INTERRUPTIBLE, classify, checkCli, cliVersion, parseVersion, plain, queryBody };
