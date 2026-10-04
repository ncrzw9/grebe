// Data files at a glance: columns, stats, a preview, Parquet metadata and
// the CSV dialect DuckDB detects -- from the Explorer's context menu, the
// Command Palette, or by hovering a file path in SQL.
//
// These need an engine (reading Parquet footers, sniffing a CSV), so they
// run through the user's `duckdb` CLI like everything else that executes,
// and the grebe binary stays engine-free. They use their own in-memory
// session: inspecting a file never touches the database a script is
// working on, and never waits behind a long query in it.

"use strict";

const fs = require("fs");
const path = require("path");
const vscode = require("vscode");
const { Session } = require("./duckdb-session");
const { columnar } = require("./lenient-json");
const results = require("./results");
const log = require("./log");
const trust = require("./trust");

const DATA = /\.(parquet|csv|tsv|txt|json|jsonl|ndjson)(\.(gz|zst))?$/i;

/** A SQL string literal for `p`. */
const lit = (p) => `'${p.replace(/'/g, "''")}'`;

/** The table function that reads `p`, chosen by extension; DuckDB's own
 *  auto-detection (`FROM 'p'`) for anything else. */
function reader(p) {
  const ext = (p.toLowerCase().replace(/\.(gz|zst)$/, "").match(/\.([a-z]+)$/) || [])[1];
  if (ext === "parquet") return `read_parquet(${lit(p)})`;
  if (["csv", "tsv", "txt"].includes(ext)) return `read_csv(${lit(p)})`;
  if (["json", "jsonl", "ndjson"].includes(ext)) return `read_json(${lit(p)})`;
  return lit(p);
}

const isParquet = (p) => /\.parquet$/i.test(p);
const isDelimited = (p) => /\.(csv|tsv|txt)(\.(gz|zst))?$/i.test(p);

/** What each view runs. Each is one statement shown as one grid. */
const VIEWS = {
  columns: {
    title: "Columns",
    sql: (p) => `SELECT column_name AS "column", column_type AS "type" FROM (DESCRIBE FROM ${reader(p)})`,
  },
  stats: { title: "Stats", sql: (p) => `SUMMARIZE FROM ${reader(p)}` },
  preview: { title: "Preview", sql: (p) => `FROM ${reader(p)} LIMIT 1000` },
  parquet: {
    title: "Parquet metadata",
    when: isParquet,
    sql: (p) =>
      `SELECT path_in_schema AS "column", type, compression, sum(num_values)::BIGINT AS "values", ` +
      `sum(total_compressed_size)::BIGINT AS compressed_bytes, sum(total_uncompressed_size)::BIGINT AS uncompressed_bytes, ` +
      `min(stats_min) AS min, max(stats_max) AS max, sum(stats_null_count)::BIGINT AS nulls, count(*) AS row_groups ` +
      `FROM parquet_metadata(${lit(p)}) GROUP BY ALL ORDER BY min(column_id)`,
    // File-level facts first: rows, row groups, who wrote it.
    before: (p) => `SELECT num_rows, num_row_groups, format_version, created_by FROM parquet_file_metadata(${lit(p)})`,
  },
  dialect: {
    title: "CSV dialect",
    when: isDelimited,
    // One setting per row; `Prompt` is a ready read_csv(...) call to copy.
    sql: (p) => `UNPIVOT (SELECT COLUMNS(*)::VARCHAR FROM sniff_csv(${lit(p)})) ON COLUMNS(*) INTO NAME setting VALUE value`,
  },
};

let session = null;
let nextId = 1e9; // result ids of our own, apart from run.js's

function cliPath() {
  return vscode.workspace.getConfiguration("grebe.duckdb").get("path", "") || "duckdb";
}

async function inspector(cwd) {
  const key = JSON.stringify([cliPath(), cwd]);
  if (session && session.alive && session.__key === key) return session;
  if (session) session.dispose();
  session = new Session({ cli: cliPath(), database: ":memory:", cwd });
  session.__key = key;
  await session.start();
  return session;
}

function cwdOf(uri) {
  const folder = vscode.workspace.getWorkspaceFolder(uri);
  return folder ? folder.uri.fsPath : path.dirname(uri.fsPath);
}

/** Which file a command is about: the Explorer's selection, the active
 *  editor's file, or one the user picks. */
async function target(arg) {
  if (arg && typeof arg === "object" && arg.fsPath) return arg;
  if (typeof arg === "string") return vscode.Uri.parse(arg);
  const active = vscode.window.activeTextEditor && vscode.window.activeTextEditor.document.uri;
  if (active && DATA.test(active.fsPath)) return active;
  const picked = await vscode.window.showOpenDialog({
    canSelectMany: false,
    filters: { "Data files": ["parquet", "csv", "tsv", "txt", "json", "jsonl", "ndjson", "gz", "zst"] },
  });
  return picked && picked[0];
}

async function show(view, arg) {
  if (!trust.ok("Reading a data file")) return;
  const uri = await target(arg);
  if (!uri) return;
  const file = uri.fsPath;
  const v = VIEWS[view];
  results.begin({ title: `${path.basename(file)} — ${v.title}`, detail: file });
  let sess;
  try {
    sess = await inspector(cwdOf(uri));
  } catch (e) {
    results.end(`Could not start duckdb: ${e.message ?? e}`);
    log.report("duckdb", `could not start duckdb to read ${path.basename(file)}: ${e.message ?? e}`);
    return;
  }
  const sqls = [v.before, v.sql].filter(Boolean).map((f) => f(file));
  for (const sql of sqls) {
    results.running(`Reading ${path.basename(file)}`);
    let r;
    try {
      r = await sess.run(sql);
    } catch (e) {
      r = e && e.cancelled
        ? { kind: "cancelled", reason: "cancel", ended: true, message: e.message }
        : { kind: "error", type: "Session", message: String(e.message ?? e) };
    }
    if (r.kind === "rows") r = { kind: "rows", ms: r.ms, ...columnar(r, 100000) };
    results.add({ id: ++nextId, uri: uri.toString(), line: null, preview: sql, result: r, exportable: false });
    if (r.kind === "error") {
      log.error("duckdb", `${v.title} of ${file}: ${r.type} Error: ${r.message}`);
      break;
    }
    if (r.kind === "cancelled") {
      log.info("duckdb", `${v.title} of ${file}: cancelled`);
      break;
    }
  }
  results.end(file);
}

// --- hover ----------------------------------------------------------------

const cache = new Map(); // "path|mtime" -> [{ name, type }]

/** The quoted string under `pos` on its line, if it names a data file. */
function literalAt(doc, pos) {
  const line = doc.lineAt(pos.line).text;
  const re = /'((?:[^']|'')*)'/g;
  for (let m; (m = re.exec(line)); ) {
    if (pos.character < m.index || pos.character > m.index + m[0].length) continue;
    const value = m[1].replace(/''/g, "'");
    if (!DATA.test(value) || /^[a-z0-9+]+:\/\//i.test(value)) return null; // local files only
    const range = new vscode.Range(pos.line, m.index, pos.line, m.index + m[0].length);
    return { value, range };
  }
  return null;
}

async function describe(file, cwd) {
  const glob = /[*?[]/.test(file);
  const mtime = glob ? 0 : fs.statSync(file).mtimeMs;
  const key = `${file}|${mtime}`;
  if (cache.has(key)) return cache.get(key);
  const sess = await inspector(cwd);
  const r = await sess.run(`DESCRIBE FROM ${reader(file)}`);
  if (r.kind !== "rows") return null;
  const name = r.columns.indexOf("column_name");
  const type = r.columns.indexOf("column_type");
  const cols = r.rows.map((row) => ({ name: row[name], type: row[type] }));
  cache.set(key, cols);
  return cols;
}

const hoverProvider = {
  async provideHover(doc, pos) {
    if (!trust.trusted()) return null;
    const hit = literalAt(doc, pos);
    if (!hit) return null;
    const cwd = cwdOf(doc.uri);
    const file = path.isAbsolute(hit.value) ? hit.value : path.join(cwd, hit.value);
    if (!/[*?[]/.test(file) && !fs.existsSync(file)) return null;
    let cols;
    try {
      cols = await Promise.race([describe(file, cwd), new Promise((r) => setTimeout(() => r(undefined), 4000))]);
    } catch {
      return null;
    }
    // Too slow for a hover (a large CSV being sniffed): stop it, so it does
    // not hold up the next inspection. Only if it is the hover's own query.
    if (cols === undefined && session && session.running && session.running.startsWith(`DESCRIBE FROM ${reader(file)}`)) {
      session.cancel("timeout");
    }
    if (!cols) return null;
    const md = new vscode.MarkdownString(undefined, true);
    const views = ["stats", "preview", ...(isParquet(file) ? ["parquet"] : []), ...(isDelimited(file) ? ["dialect"] : [])];
    md.isTrusted = { enabledCommands: views.map((v) => `grebe.inspect.${v}`) };
    const arg = encodeURIComponent(JSON.stringify([vscode.Uri.file(file).toString()]));
    const links = views.map((v) => `[${VIEWS[v].title}](command:grebe.inspect.${v}?${arg})`).join(" · ");
    const width = Math.min(Math.max(...cols.map((c) => c.name.length), 4), 40);
    const shown = cols.slice(0, 60).map((c) => `${c.name.padEnd(width)}  ${c.type}`);
    if (cols.length > 60) shown.push(`… ${cols.length - 60} more`);
    md.appendMarkdown(`**${path.basename(hit.value)}** — ${cols.length} column${cols.length === 1 ? "" : "s"}  \n${links}\n`);
    md.appendCodeblock(shown.join("\n"), "text");
    return new vscode.Hover(md, hit.range);
  },
};

function activate(context) {
  for (const view of Object.keys(VIEWS)) {
    context.subscriptions.push(log.command(`grebe.inspect.${view}`, (arg) => show(view, arg)));
  }
  context.subscriptions.push(
    vscode.languages.registerHoverProvider({ language: "sql" }, hoverProvider),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("grebe.duckdb.path") && session) {
        session.dispose();
        session = null;
      }
    }),
    { dispose },
  );
}

/** Stop what the inspection session is running, if anything. */
function cancel() {
  return session ? session.cancel("cancel") : false;
}

/** End the inspection session, if one is running. */
function dispose() {
  if (session) session.dispose();
  session = null;
}

module.exports = { activate, dispose, cancel, reader, literalAt, VIEWS, show, hoverProvider, inspector, cwdOf, DATA };
