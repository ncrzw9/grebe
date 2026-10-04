// Data files opened in the results grid: a read-only custom editor for
// Parquet (the default for .parquet, which is not text) and for delimited
// files (offered through "Open With...", since a CSV is also useful as
// text). Each file gets its own tab with its own grid; the tab can sit in
// any editor group or be moved into a window of its own.
//
// The rows come from the user's `duckdb` CLI, in the in-memory session the
// file inspections use, so opening a file never touches the database a
// script is working on. Column types come from DESCRIBE and show in the
// grid's headers. The file is read again when it changes on disk.

"use strict";

const path = require("path");
const vscode = require("vscode");
const { columnar } = require("./lenient-json");
const { reader, inspector, cwdOf } = require("./inspect");
const { Channel } = require("./results");
const log = require("./log");

// Parquet opens in the grid by default; delimited files only when chosen.
// Two view types because a custom editor's priority covers all of its
// file patterns.
const VIEW_TYPE = "grebe.dataFile";
const DELIMITED_VIEW_TYPE = "grebe.delimitedFile";
const viewTypeFor = (uri) => (/\.parquet$/i.test(uri.fsPath) ? VIEW_TYPE : DELIMITED_VIEW_TYPE);

function maxRows() {
  return Math.max(1, vscode.workspace.getConfiguration("grebe.duckdb").get("maxRows", 100000));
}

/** Read `uri` into `channel`: its rows (up to grebe.duckdb.maxRows) with
 *  types, or the error that stopped it. */
async function load(channel, uri) {
  const file = uri.fsPath;
  const limit = maxRows();
  const sql = `FROM ${reader(file)} LIMIT ${limit + 1}`;
  channel.begin({ title: path.basename(file), detail: "reading…" });
  let r;
  let types = null;
  try {
    const sess = await inspector(cwdOf(uri));
    r = await sess.run(sql);
    if (r.kind === "rows") types = await sess.describe(`FROM ${reader(file)}`).catch(() => null);
  } catch (e) {
    r = { kind: "error", type: "Session", message: String(e.message ?? e) };
  }
  if (r.kind === "rows") {
    // One row past the limit was asked for, only to know whether there are
    // more: columnar() keeps `limit` and reports the count it saw.
    const more = r.rows.length > limit;
    r = { kind: "rows", ms: r.ms, ...columnar(r, limit) };
    if (types && types.length === r.columns.length) r.types = types.map((t) => t.type);
    if (r.columns.length === 0 && types) {
      r.columns = types.map((t) => t.name);
      r.types = types.map((t) => t.type);
      r.data = r.columns.map(() => []);
      r.num = r.columns.map(() => false);
    }
    // Not a count: the file has more rows than were read.
    if (more) r.more = true;
    channel.add({ id: 0, uri: uri.toString(), line: null, preview: sql, result: r, exportable: false });
    channel.end(more ? `first ${limit.toLocaleString()} rows (grebe.duckdb.maxRows) · ${file}` : file);
  } else {
    if (r.kind === "error") log.error("grid", `${file}: ${r.type} Error: ${r.message}`);
    channel.add({ id: 0, uri: uri.toString(), line: null, preview: sql, result: r, exportable: false });
    channel.end(r.kind === "error" ? "Could not read the file." : file);
  }
}

const provider = {
  openCustomDocument(uri) {
    return { uri, dispose() {} };
  },

  resolveCustomEditor(document, panel) {
    const channel = new Channel();
    const attached = channel.attach(panel.webview);
    const reload = () => load(channel, document.uri);
    const watcher = vscode.workspace.createFileSystemWatcher(
      new vscode.RelativePattern(vscode.Uri.file(path.dirname(document.uri.fsPath)), path.basename(document.uri.fsPath)),
    );
    let pending = null;
    // A writer may touch the file several times in a row: read once it
    // settles.
    const changed = () => {
      clearTimeout(pending);
      pending = setTimeout(reload, 300);
    };
    watcher.onDidChange(changed);
    watcher.onDidCreate(changed);
    panel.onDidDispose(() => {
      clearTimeout(pending);
      watcher.dispose();
      attached.dispose();
    });
    return reload();
  },
};

function activate(context) {
  context.subscriptions.push(
    ...[VIEW_TYPE, DELIMITED_VIEW_TYPE].map((type) =>
      vscode.window.registerCustomEditorProvider(type, provider, {
        webviewOptions: { retainContextWhenHidden: true },
        supportsMultipleEditorsPerDocument: true,
      }),
    ),
    // From the Explorer's context menu or the Command Palette.
    log.command("grebe.openInGrid", async (uri) => {
      if (!uri) {
        const picked = await vscode.window.showOpenDialog({
          canSelectMany: false,
          filters: { "Data files": ["parquet", "csv", "tsv", "gz"] },
        });
        uri = picked && picked[0];
      }
      if (uri) await vscode.commands.executeCommand("vscode.openWith", uri, viewTypeFor(uri));
    }),
  );
}

module.exports = { activate, load, provider, viewTypeFor, VIEW_TYPE, DELIMITED_VIEW_TYPE };
