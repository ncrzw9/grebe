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
const { parse, readTable } = require("./lenient-json");

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
   *           startTimeoutMs?: number }} opts
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
    const proc = spawn(this.opts.cli, [this.opts.database], {
      cwd: this.opts.cwd,
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    this.proc = proc;
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

  /** Run one statement. Resolves with `classify`'s shape plus `ms`. */
  run(sql) {
    const job = this.queue.then(async () => {
      const started = process.hrtime.bigint();
      const trimmed = sql.replace(/\s+$/, "");
      const terminated = trimmed.endsWith(";") ? trimmed : `${trimmed}\n;`;
      const { out, err } = await this.send(terminated);
      const result = classify(err.trim() ? `${err}\n${out}` : out);
      result.ms = Number(process.hrtime.bigint() - started) / 1e6;
      return result;
    });
    this.queue = job.catch(() => {});
    return job;
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
   */
  async exportTo(sql, file, format) {
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
      this.waiting = { marker, resolve, reject };
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
    // stderr and stdout are separate pipes. Anything the CLI wrote to
    // stderr for this statement was written before the marker reached
    // stdout, so it is readable by the time the event loop turns once more.
    setImmediate(() => {
      const err = this.err;
      this.err = "";
      w.resolve({ out, err });
    });
  }

  close(reason) {
    if (this.closedWith !== null) return;
    this.closedWith = reason;
    if (this.waiting) {
      this.waiting.reject(new Error(reason));
      this.waiting = null;
    }
  }

  /** Kill the process. Any statement in flight rejects. */
  dispose() {
    this.close("session closed");
    if (this.proc && this.proc.exitCode === null) this.proc.kill();
  }
}

module.exports = { Session, classify, checkCli, cliVersion, parseVersion, plain, queryBody };
