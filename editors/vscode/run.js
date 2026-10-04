// Run SQL through the user's `duckdb` CLI. The language server
// decides what the statements are (`grebe/statements`); a long-lived
// Session runs them one at a time; the results panel shows each outcome.

"use strict";

const path = require("path");
const vscode = require("vscode");
const { Session, checkCli, parseVersion } = require("./duckdb-session");
const { columnar } = require("./lenient-json");
const results = require("./results");
const log = require("./log");

let session = null;
let status = null;
let runtimeErrors = null;
let output = null;
let versionFor = { cli: null, text: null, error: null };
// The last CLI problem shown as a notification, so one problem is shown once.
let announced = null;
let nextResultId = 0;
let lenses = null;
// The run (or export) in progress: { sess, label, started, cancelled, timer }.
let active = null;
// Result id -> { sql, uri } for the results on screen that can be exported.
const exportable = new Map();

const EXPORTS = {
  csv: { ext: "csv", label: "CSV" },
  tsv: { ext: "tsv", label: "TSV" },
  parquet: { ext: "parquet", label: "Parquet" },
  json: { ext: "json", label: "JSON (one object per line)" },
};

/** An export button: ask where, then have DuckDB write the full result. */
async function exportResult(id, format) {
  const item = exportable.get(id);
  const fmt = EXPORTS[format];
  if (!item || !fmt) {
    results.exported(id, "This result is from an earlier run; run it again to export.");
    return;
  }
  const base = vscode.workspace.getWorkspaceFolder(item.uri)?.uri ?? vscode.Uri.file(path.dirname(item.uri.fsPath));
  if (active) {
    results.exported(id, "A run is in progress; export when it ends.");
    return;
  }
  const target = await vscode.window.showSaveDialog({
    defaultUri: vscode.Uri.joinPath(base, `result.${fmt.ext}`),
    filters: { [fmt.label]: [fmt.ext] },
    saveLabel: `Export ${fmt.label}`,
  });
  if (!target) return;
  results.exported(id, `Writing ${path.basename(target.fsPath)}…`);
  let r;
  try {
    const sess = await sessionFor(item.uri);
    startActivity(sess, `export to ${path.basename(target.fsPath)}`);
    r = await sess.exportTo(item.sql, target.fsPath, format);
  } catch (e) {
    r = e && e.cancelled ? cancelledResult(e) : { kind: "error", type: "Session", message: String(e.message ?? e) };
    if (e && e.ended) session = null;
  } finally {
    endActivity();
  }
  if (r.kind === "cancelled") {
    results.exported(id, r.ended ? `Export cancelled. ${r.message}` : "Export cancelled.");
    log.info("duckdb", `export to ${target.fsPath} cancelled`);
    return;
  }
  if (r.kind === "error") {
    results.exported(id, `Export failed: ${r.type} Error: ${r.message}`);
    log.error("duckdb", `export to ${target.fsPath} failed: ${r.type} Error: ${r.message}`);
    return;
  }
  log.info("duckdb", `exported ${format} to ${target.fsPath}`);
  results.exported(id, `Saved ${target.fsPath}`);
  const pick = await vscode.window.showInformationMessage(
    `grebe: exported to ${path.basename(target.fsPath)}`,
    "Reveal in File Explorer",
  );
  if (pick) vscode.commands.executeCommand("revealFileInOS", target);
}

function settings() {
  const cfg = vscode.workspace.getConfiguration("grebe.duckdb");
  return {
    cli: cfg.get("path", "") || "duckdb",
    database: cfg.get("database", "") || ":memory:",
    maxRows: Math.max(1, cfg.get("maxRows", 100000)),
    timeoutMs: Math.max(0, cfg.get("queryTimeout", 0)) * 1000,
  };
}

// --- a run in progress -------------------------------------------------------

/** Mark `sess` busy with `label`: a clock on the status bar that cancels
 *  when clicked, and the editor's Run buttons turned into a Stop button. */
function startActivity(sess, label) {
  active = { sess, label, started: Date.now(), cancelled: false, timer: null };
  vscode.commands.executeCommand("setContext", "grebe.duckdb.running", true);
  const tick = () => {
    if (!active) return;
    const s = Math.floor((Date.now() - active.started) / 1000);
    status.text = `$(loading~spin) DuckDB · ${s} s`;
    status.tooltip = `Running ${active.label}\nClick to cancel.`;
    status.command = "grebe.duckdb.cancel";
  };
  tick();
  active.timer = setInterval(tick, 1000);
}

function endActivity() {
  if (!active) return;
  clearInterval(active.timer);
  active = null;
  vscode.commands.executeCommand("setContext", "grebe.duckdb.running", false);
  status.command = "grebe.duckdb.chooseDatabase";
  refreshStatus();
}

/** Stop what is running: the statement in flight, and any left in the run. */
async function cancel() {
  if (!active) {
    vscode.window.setStatusBarMessage("grebe: nothing is running", 2500);
    return false;
  }
  active.cancelled = true;
  log.info("duckdb", `cancelling ${active.label} after ${((Date.now() - active.started) / 1000).toFixed(1)} s`);
  return active.sess.cancel("cancel");
}

/** A statement that was stopped, as a result to show. A cancel that had to
 *  end the session rejects run() instead; that becomes one of these too. */
function cancelledResult(e, ms) {
  return { kind: "cancelled", reason: "cancel", ended: true, ms, message: `${e.message.replace(/^cancelled; /, "DuckDB ").replace(/^DuckDB on Windows/, "On Windows, cancelling")}: TEMP tables and in-memory data are gone.` };
}

/** The directory relative paths in SQL resolve against: the file's
 *  workspace folder, like running `duckdb` from the project root. */
function cwdFor(uri) {
  const folder = vscode.workspace.getWorkspaceFolder(uri);
  if (folder) return folder.uri.fsPath;
  return uri.scheme === "file" ? path.dirname(uri.fsPath) : process.cwd();
}

function resolveDatabase(database, cwd) {
  if (database === ":memory:" || path.isAbsolute(database)) return database;
  return path.join(cwd, database);
}

/**
 * A DuckDB problem, shown now: in the log, and as a notification with the
 * two things that fix most of them. Not awaited by callers -- the
 * notification stays until dismissed.
 */
function reportError(message) {
  log.report("duckdb", message, {
    "Set DuckDB Path": () => vscode.commands.executeCommand("workbench.action.openSettings", "grebe.duckdb.path"),
  });
}

/** The session ended on its own (crashed, killed, out of memory): say so
 *  now, not at the next run. */
function sessionEnded(which, reason) {
  if (session !== which) return;
  session = null;
  output.appendLine(`session ended: ${reason}`);
  Promise.resolve(
    vscode.window.showWarningMessage(
      `grebe: the DuckDB session ended: ${reason.replace(/[.!]?$/, ".")} The next run starts a new session; TEMP tables and in-memory data are gone.`,
      "Show Output",
    ),
  ).then((pick) => pick && output.show(true));
  for (const l of runListeners) l();
}

async function sessionFor(uri) {
  const s = settings();
  const cwd = cwdFor(uri);
  const wanted = new Session({
    cli: s.cli,
    database: resolveDatabase(s.database, cwd),
    cwd,
    // What DuckDB says outside a statement (the rc file, warnings) goes to
    // the log as it arrives.
    onStderr: (text) => output.append(text),
    onExit: (reason) => sessionEnded(wanted, reason),
  });
  if (session && session.alive && session.key === wanted.key) return session;
  if (session) session.dispose();
  session = wanted;
  output.appendLine(`starting ${s.cli} ${wanted.opts.database} (cwd ${cwd})`);
  try {
    await session.start();
  } catch (e) {
    session = null;
    throw e;
  }
  return session;
}

/**
 * Check the CLI (`duckdb --version`, which also reads ~/.duckdbrc) and show
 * the result in the status bar. A problem is also shown as a notification
 * right away, before anything is run -- except a missing `duckdb` on PATH
 * when grebe.duckdb.path was never set: linting and formatting need no
 * DuckDB, so someone who never runs SQL is not nagged.
 */
async function refreshStatus({ recheck = false } = {}) {
  const s = settings();
  if (recheck || versionFor.cli !== s.cli) {
    versionFor = { cli: s.cli, ...(await checkCli(s.cli)) };
  }
  const v = parseVersion(versionFor.text);
  const db = s.database === ":memory:" ? ":memory:" : path.basename(s.database);
  status.text = v ? `$(database) DuckDB ${v.join(".")} · ${db}` : `$(warning) DuckDB`;
  status.tooltip = v
    ? `${versionFor.text}\nDatabase: ${s.database}\nClick to choose a database.`
    : `${versionFor.error} Set grebe.duckdb.path.`;
  status.show();
  if (v) {
    output.appendLine(`duckdb: ${s.cli} ${versionFor.text}`);
    announced = null;
    return true;
  }
  const quiet = versionFor.missing && !explicitPath();
  const key = `${s.cli}\n${versionFor.error}`;
  if (quiet) output.appendLine(`duckdb: ${versionFor.error} Running SQL needs the DuckDB CLI; set grebe.duckdb.path.`);
  else if (announced !== key) {
    announced = key;
    reportError(`${versionFor.error} Running SQL needs a working DuckDB CLI.`);
  }
  return false;
}

/** Whether the user set grebe.duckdb.path themselves. */
function explicitPath() {
  return Boolean(vscode.workspace.getConfiguration("grebe.duckdb").get("path", ""));
}

/** After grebe.duckdb.path or .database changes: check the CLI and open the
 *  new session now, so a bad path, a broken ~/.duckdbrc or a database this
 *  CLI cannot open is reported when it is chosen, not at the next run. */
async function applySettings() {
  restart();
  if (!(await refreshStatus({ recheck: true }))) return;
  const folder = (vscode.workspace.workspaceFolders || [])[0];
  const active = vscode.window.activeTextEditor && vscode.window.activeTextEditor.document.uri;
  const uri = active && active.scheme === "file" ? active : folder ? folder.uri : null;
  if (!uri) return;
  try {
    await sessionFor(uri);
  } catch (e) {
    reportError(`could not start duckdb. ${startHint(String(e.message ?? e))}`);
  }
}

/** A start-up failure, with the one cause that needs explaining. */
function startHint(msg) {
  // A 1.5 CLI cannot open a database file written by 2.0.
  return /version number|newer version/i.test(msg)
    ? `${msg} This database was written by a newer DuckDB than the CLI in grebe.duckdb.path.`
    : msg;
}

/** Byte offset `pos` within `text` -> UTF-16 index (DuckDB positions are
 *  offsets into the UTF-8 query string). */
function byteToIndex(text, pos) {
  return Buffer.from(text, "utf8").subarray(0, pos).toString("utf8").length;
}

/** What the active editor asks to run: the cursor's statement or the
 *  selection (`statement`), or the whole file (`file`). */
function fromEditor(client, mode) {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== "sql") {
    vscode.window.showInformationMessage("grebe: open a .sql file to run it with DuckDB.");
    return null;
  }
  const range = mode === "statement" ? client.code2ProtocolConverter.asRange(editor.selection) : null;
  return { doc: editor.document, range };
}

/** Run the statements of `doc` that `range` (an LSP range) touches, or all
 *  of them when `range` is null. */
async function run(client, { doc, range }) {
  // One run at a time: a second would wait, invisibly, behind the first.
  if (active) {
    const pick = await vscode.window.showWarningMessage(
      `grebe: ${active.label} is still running.`,
      "Cancel It",
    );
    if (pick === "Cancel It") await cancel();
    return;
  }
  const params = { textDocument: { uri: doc.uri.toString() } };
  if (range) params.range = range;
  let statements;
  try {
    statements = await client.sendRequest("grebe/statements", params);
  } catch (e) {
    vscode.window.showErrorMessage(`grebe: could not split statements (${e.message ?? e}).`);
    return;
  }
  if (!statements || statements.length === 0) {
    vscode.window.showInformationMessage("grebe: nothing to run here.");
    return;
  }

  runtimeErrors.delete(doc.uri);
  exportable.clear();
  const s = settings();
  results.begin({
    title: `${path.basename(doc.fileName)} — ${statements.length} statement${statements.length === 1 ? "" : "s"}`,
    detail: `running on ${s.database} with ${s.cli}…`,
  });

  let sess;
  try {
    sess = await sessionFor(doc.uri);
  } catch (e) {
    const msg = startHint(String(e.message ?? e));
    results.end(`Could not start duckdb: ${msg}`);
    reportError(`could not start duckdb. ${msg}`);
    return;
  }

  let ran = 0;
  let succeeded = 0;
  let failed = false;
  let stopped = null; // "cancel" | "timeout" when a statement was stopped
  startActivity(sess, `${path.basename(doc.fileName)}`);
  try {
  for (const [i, st] of statements.entries()) {
    // Cancelled between statements (the last one finished as the cancel
    // came in): the rest do not run.
    if (active.cancelled) {
      stopped = "cancel";
      break;
    }
    const range = client.protocol2CodeConverter.asRange(st.range);
    const entry = {
      id: ++nextResultId,
      uri: doc.uri.toString(),
      line: range.start.line,
      preview: st.text.split("\n")[0].slice(0, 120),
      result: null,
      // Types and export both run the query again, so only for queries.
      exportable: false,
    };
    results.running(
      statements.length === 1 ? "Running" : `Running statement ${i + 1} of ${statements.length}`,
    );
    let r;
    const t0 = Date.now();
    try {
      r = await sess.run(st.text, { timeoutMs: s.timeoutMs });
    } catch (e) {
      r = e && e.cancelled
        ? cancelledResult(e, Date.now() - t0)
        : { kind: "error", type: "Session", subtype: null, message: String(e.message ?? e), position: null };
      // The session is gone either way; the next run starts a new one.
      if (e && e.ended && session === sess) session = null;
    }
    ran++;
    if (r.kind === "rows") {
      // What the grid receives: column-major display text (columnar()).
      r = { kind: "rows", ms: r.ms, ...columnar(r, s.maxRows) };
      if (st.kind === "select") {
        entry.exportable = true;
        exportable.set(entry.id, { sql: st.text, uri: doc.uri });
        // DESCRIBE only plans. It also names the columns of an empty
        // result, which the JSON output (`[]`) cannot.
        const described = await sess.describe(st.text).catch(() => null);
        if (described && (r.columns.length === 0 || described.length === r.columns.length)) {
          if (r.columns.length === 0) {
            r.columns = described.map((c) => c.name);
            r.data = r.columns.map(() => []);
            r.num = r.columns.map(() => false);
          }
          r.types = described.map((c) => c.type);
        }
      }
    }
    const where = `${path.basename(doc.fileName)}:${range.start.line + 1}`;
    if (r.kind === "error") r.context = errorContext(st.text, r.position, range.start.line);
    entry.result = r;
    results.add(entry);
    log.info("duckdb", `${where} ${outcome(r)}`);
    if (r.kind === "cancelled") {
      stopped = r.reason;
      break;
    }
    if (r.kind !== "error") succeeded++;
    if (r.kind === "error") {
      failed = true;
      // The whole statement, and where in it DuckDB stopped, so the log
      // alone is enough to see what went wrong.
      log.info("duckdb", st.text);
      if (r.context) log.info("duckdb", caretLines(r.context));
      markError(doc, range, st.text, r);
      break; // a failure stops the run where it happened
    }
  }
  } finally {
    endActivity();
  }
  const skipped = statements.length - ran;
  results.end(
    stopped
      ? `${stopped === "timeout" ? "Timed out" : "Cancelled"}: ${succeeded} succeeded${skipped ? `, ${skipped} not run` : ""}.`
      : failed
        ? `Stopped at the error: ${ran - 1} succeeded${skipped ? `, ${skipped} not run` : ""}.`
        : `${ran} statement${ran === 1 ? "" : "s"} succeeded.`,
  );
  // A run may have created, dropped or attached something.
  for (const l of runListeners) l();
}

/** What a statement did, in a few words, for the log. */
function outcome(r) {
  const ms = r.ms === undefined ? "" : ` (${r.ms < 10 ? r.ms.toFixed(1) : Math.round(r.ms)} ms)`;
  if (r.kind === "cancelled") return `${r.reason === "timeout" ? "timed out" : "cancelled"}${ms}${r.ended ? `; ${r.message}` : ""}`;
  if (r.kind === "rows") return `${r.total} row${r.total === 1 ? "" : "s"} × ${r.columns.length}${ms}`;
  if (r.kind === "ok") return `ok${ms}`;
  if (r.kind === "text") return `text output${ms}`;
  return `${r.type} Error: ${r.message}`;
}

/**
 * The line of `text` that DuckDB's error points into, with the column
 * under the error: `{ line, text, column }`, `line` being the document line
 * (the statement starts on `firstLine`). `position` is a byte offset into
 * the statement, as DuckDB reports it; null when there is none.
 */
function errorContext(text, position, firstLine) {
  if (position === null || position === undefined) return null;
  const at = byteToIndex(text, position);
  const before = text.slice(0, at);
  const lineInStmt = before.split("\n").length - 1;
  const lineStart = before.lastIndexOf("\n") + 1;
  const lineEnd = text.indexOf("\n", lineStart);
  return {
    line: firstLine + lineInStmt,
    text: text.slice(lineStart, lineEnd < 0 ? text.length : lineEnd).replace(/\s+$/, ""),
    column: at - lineStart,
  };
}

/** `  4 | SELECT nope` over `    |        ^`. */
function caretLines(ctx) {
  const n = String(ctx.line + 1);
  return `${n} | ${ctx.text}\n${" ".repeat(n.length)} | ${" ".repeat(ctx.column)}^`;
}

function markError(doc, range, text, r) {
  let where = range;
  if (r.position !== null && r.position !== undefined) {
    const start = doc.offsetAt(range.start) + byteToIndex(text, r.position);
    const at = doc.positionAt(Math.min(start, doc.offsetAt(range.end)));
    const word = doc.getWordRangeAtPosition(at);
    where = word ?? new vscode.Range(at, at.translate(0, 1));
  }
  const d = new vscode.Diagnostic(where, `${r.type} Error: ${r.message}`, vscode.DiagnosticSeverity.Error);
  d.source = "duckdb";
  if (r.subtype) d.code = r.subtype;
  runtimeErrors.set(doc.uri, [d]);
}

async function chooseDatabase() {
  const pick = await vscode.window.showQuickPick(
    [
      { label: ":memory:", description: "a fresh in-memory database for this session" },
      { label: "$(folder-opened) Choose a file…", file: true },
    ],
    { placeHolder: "Database for running SQL" },
  );
  if (!pick) return;
  let value = ":memory:";
  if (pick.file) {
    const picked = await vscode.window.showOpenDialog({
      canSelectMany: false,
      filters: { DuckDB: ["duckdb", "db", "ddb"], "All files": ["*"] },
    });
    if (!picked || picked.length === 0) return;
    value = picked[0].fsPath;
  }
  const target = vscode.workspace.workspaceFolders
    ? vscode.ConfigurationTarget.Workspace
    : vscode.ConfigurationTarget.Global;
  await vscode.workspace.getConfiguration("grebe.duckdb").update("database", value, target);
}

// Listeners told when a run ends (the DuckDB explorer refreshes on it).
const runListeners = new Set();
function onDidRun(listener) {
  runListeners.add(listener);
  return { dispose: () => runListeners.delete(listener) };
}

/** The session the explorer should read: the running one if there is one,
 *  else one started for the workspace (or the active file's folder). */
async function currentSession() {
  if (session && session.alive) return session;
  const folder = (vscode.workspace.workspaceFolders || [])[0];
  const active = vscode.window.activeTextEditor && vscode.window.activeTextEditor.document.uri;
  const uri = active && active.scheme === "file" ? active : folder ? folder.uri : null;
  if (!uri) throw new Error("open a folder or a .sql file first");
  return sessionFor(uri);
}

/** The session only if it is already running -- never starts one. */
const liveSession = () => (session && session.alive ? session : null);

function restart() {
  if (session) session.dispose();
  session = null;
  vscode.window.setStatusBarMessage("grebe: DuckDB session restarted", 3000);
}

/**
 * A "▶ Run" CodeLens above every statement. The statements are the server's
 * (`grebe/statements`), the same split a run uses, so a lens can never sit
 * on something that is not one statement; there is no second parser here.
 * VS Code asks again after edits; `grebe.duckdb.codeLens: false` turns the
 * lenses off.
 */
class StatementLenses {
  constructor(clientReady) {
    this.clientReady = clientReady;
    this.changed = new vscode.EventEmitter();
    this.onDidChangeCodeLenses = this.changed.event;
  }

  refresh() {
    this.changed.fire();
  }

  async provideCodeLenses(doc) {
    if (!vscode.workspace.getConfiguration("grebe.duckdb").get("codeLens", true)) return [];
    const client = await this.clientReady();
    if (!client) return [];
    let statements;
    try {
      statements = await client.sendRequest("grebe/statements", { textDocument: { uri: doc.uri.toString() } });
    } catch {
      return [];
    }
    return (statements || []).map((st) => {
      const range = client.protocol2CodeConverter.asRange(st.range);
      return new vscode.CodeLens(new vscode.Range(range.start, range.start), {
        title: "▶ Run",
        tooltip: "Run this statement with DuckDB",
        command: "grebe.duckdb.runRange",
        arguments: [doc.uri.toString(), st.range],
      });
    });
  }

  dispose() {
    this.changed.dispose();
  }
}

/**
 * @param clientReady  () => Promise<LanguageClient|null>: the running
 *   language server, or null when it could not start. A function, because
 *   the client is replaced when the server restarts.
 */
function activate(context, clientReady) {
  output = log.tagged("duckdb");
  runtimeErrors = vscode.languages.createDiagnosticCollection("duckdb");
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  status.command = "grebe.duckdb.chooseDatabase";

  results.setExportHandler(exportResult);
  results.setRevealHandler(async (uri, line) => {
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(uri));
    const ed = await vscode.window.showTextDocument(doc, vscode.ViewColumn.One);
    const pos = new vscode.Position(line, 0);
    ed.selection = new vscode.Selection(pos, pos);
    ed.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);
  });

  const client = async () => {
    const c = await clientReady();
    if (!c) {
      vscode.window.showErrorMessage("grebe: the grebe language server is not running, so statements cannot be split.");
    }
    return c;
  };
  const withClient = (mode) => async () => {
    const c = await client();
    const target = c && fromEditor(c, mode);
    if (target) await run(c, target);
  };
  // The ▶ Run lens above each statement: that statement's own range, so it
  // runs exactly one statement whichever editor has focus.
  const runRange = async (uri, range) => {
    const c = await client();
    if (!c) return;
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(uri));
    await run(c, { doc, range });
  };
  lenses = new StatementLenses(clientReady);

  context.subscriptions.push(
    output,
    runtimeErrors,
    status,
    log.command("grebe.duckdb.runStatement", withClient("statement")),
    log.command("grebe.duckdb.runFile", withClient("file")),
    log.command("grebe.duckdb.runRange", runRange),
    vscode.languages.registerCodeLensProvider({ language: "sql" }, lenses),
    lenses,
    log.command("grebe.duckdb.restart", restart),
    log.command("grebe.duckdb.cancel", cancel),
    log.command("grebe.duckdb.chooseDatabase", chooseDatabase),
    // A runtime error describes the text it ran; once that text changes, it
    // no longer points at anything.
    vscode.workspace.onDidChangeTextDocument((e) => runtimeErrors.delete(e.document.uri)),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("grebe.duckdb.codeLens")) lenses.refresh();
      if (e.affectsConfiguration("grebe.duckdb.path") || e.affectsConfiguration("grebe.duckdb.database")) {
        applySettings();
      } else if (e.affectsConfiguration("grebe.duckdb.maxRows")) {
        restart();
      }
    }),
    { dispose: () => session && session.dispose() },
  );
  refreshStatus();
}

/** The language server changed (started, restarted): ask for lenses again. */
function clientChanged() {
  if (lenses) lenses.refresh();
}

/** A run or export is in progress. */
const isRunning = () => active !== null;

module.exports = { activate, clientChanged, cancel, isRunning, errorContext, caretLines, currentSession, liveSession, onDidRun, settings, resolveDatabase, cwdFor };
