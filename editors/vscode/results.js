// Where results are shown. The page itself is media/results-page.js (the
// keyboard grid); every value reaches it through postMessage and is written
// with textContent or drawn on a canvas -- query data is never HTML.
//
// One page, several possible hosts:
//
//   - the Results view, in a container of its own. It starts in the bottom
//     panel and can be dragged to either side bar or back, like any view.
//   - an editor tab ("Open Results in Editor"), which can sit in any editor
//     group or be moved into a window of its own.
//   - one tab per data file opened with the grid (see dataEditor.js), each
//     with its own content.
//
// A Channel is what a producer writes to: begin / add / end. It keeps every
// message since the last `begin` and replays them to a page when the page
// (re)loads, so a host that is created late, or a view that is moved and
// reloads, shows the current run rather than nothing.

"use strict";

const crypto = require("crypto");
const vscode = require("vscode");
const log = require("./log");

const VIEW_ID = "grebe.results";

let extensionUri = null;
const handlers = { reveal: () => {}, export: () => {}, cancel: () => {}, count: () => {} };

/** Where media/ lives; set once from activate(). */
function init(uri) {
  extensionUri = uri;
}

/** Called with (uri, line) when a statement's line link is clicked. */
function setRevealHandler(handler) {
  handlers.reveal = handler;
}

/** Called with (id) when a result's Count button is clicked. */
function setCountHandler(handler) {
  handlers.count = handler;
}

/** Called when the shared results' Cancel button is clicked. */
function setCancelHandler(handler) {
  handlers.cancel = handler;
}

/** Called with (id, format) when an export button is clicked. */
function setExportHandler(handler) {
  handlers.export = handler;
}

class Channel {
  constructor() {
    this.webviews = new Set();
    this.log = [];
    // What Cancel stops, for a channel with its own producer (a data
    // file's tab); the shared results use the handler set for them.
    this.onCancel = null;
  }

  /** Render into `webview` from now on; replays what is on screen now. */
  attach(webview) {
    const media = vscode.Uri.joinPath(extensionUri, "media");
    webview.options = { enableScripts: true, localResourceRoots: [media] };
    const script = webview.asWebviewUri(vscode.Uri.joinPath(media, "results-page.js"));
    webview.html = html(String(script), webview.cspSource);
    this.webviews.add(webview);
    const sub = webview.onDidReceiveMessage((m) => this.receive(webview, m));
    return {
      dispose: () => {
        this.webviews.delete(webview);
        sub.dispose();
      },
    };
  }

  receive(webview, m) {
    if (!m) return undefined;
    // The page has loaded (first time, or again after a move): bring it up
    // to date.
    if (m.type === "ready") {
      for (const msg of this.log) webview.postMessage(msg);
      return undefined;
    }
    if (m.type === "reveal") return handlers.reveal(m.uri, m.line);
    if (m.type === "cancel") return this.onCancel ? this.onCancel() : handlers.cancel();
    // The page cannot reach the system clipboard itself; the extension can.
    if (m.type === "copy" && typeof m.text === "string") {
      vscode.env.clipboard.writeText(m.text);
      vscode.window.setStatusBarMessage(`grebe: copied ${m.text.split("\n").length} line(s)`, 2500);
      return undefined;
    }
    // Returned so a caller (the tests) can wait for the file to be written.
    if (m.type === "export") return handlers.export(m.id, m.format);
    if (m.type === "count") return handlers.count(m.id);
    return undefined;
  }

  post(msg) {
    if (msg.type === "begin") this.log = [];
    this.log.push(msg);
    for (const w of this.webviews) w.postMessage(msg);
  }

  /** Start a run: clears the page and shows what is about to execute. */
  begin(header) {
    this.post({ type: "begin", header });
  }

  /** Something has started and may take a while: `label` and a clock in
   *  the header, with a Cancel button. Cleared by the next result or end. */
  running(label) {
    this.post({ type: "running", label, since: Date.now() });
  }

  /** One statement's outcome. The caller has capped the rows, turned them
   *  column-major (columnar()), and set `id` and `exportable`. */
  add(entry) {
    this.post({ type: "result", entry });
  }

  end(summary) {
    this.post({ type: "end", summary });
  }

  /** A line under a result's export buttons: where the file went, or why
   *  not. Not replayed: it is about one click, not the result. */
  exported(id, message) {
    for (const w of this.webviews) w.postMessage({ type: "exported", id, message });
  }

  /** A result's full row count, once counted. */
  counted(id, total) {
    for (const w of this.webviews) w.postMessage({ type: "counted", id, total });
  }
}

// --- the shared results: the view and the optional editor tab -------------

const shared = new Channel();
let view = null; // vscode.WebviewView once resolved
let panel = null; // vscode.WebviewPanel while open

const provider = {
  resolveWebviewView(webviewView) {
    view = webviewView;
    const attached = shared.attach(webviewView.webview);
    webviewView.onDidDispose(() => {
      attached.dispose();
      if (view === webviewView) view = null;
    });
  },
};

/** Show the results somewhere visible without taking focus from the editor:
 *  the editor tab if one is open, else the view. */
async function show() {
  if (panel) {
    panel.reveal(undefined, true);
    return;
  }
  if (view) {
    view.show(true);
    return;
  }
  // The view has never been opened: its focus command creates it. Focus
  // then goes back to the editor the run came from.
  const editor = vscode.window.activeTextEditor;
  await vscode.commands.executeCommand(`${VIEW_ID}.focus`);
  if (editor) await vscode.window.showTextDocument(editor.document, { viewColumn: editor.viewColumn, preserveFocus: false });
}

/** The results in an editor tab, which can be dragged to any editor group
 *  or into a window of its own. */
function openInEditor() {
  if (panel) {
    panel.reveal();
    return;
  }
  panel = vscode.window.createWebviewPanel(
    "grebe.resultsEditor",
    "DuckDB Results",
    { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true },
    { enableScripts: true, retainContextWhenHidden: true },
  );
  const attached = shared.attach(panel.webview);
  panel.onDidDispose(() => {
    attached.dispose();
    panel = null;
  });
}

function begin(header) {
  shared.begin(header);
  show();
}

const add = (entry) => shared.add(entry);
const running = (label) => shared.running(label);
const end = (summary) => shared.end(summary);
const exported = (id, message) => shared.exported(id, message);
const counted = (id, total) => shared.counted(id, total);

function register(context) {
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(VIEW_ID, provider, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    log.command("grebe.results.openInEditor", openInEditor),
    log.command("grebe.results.show", () => show()),
  );
}

function html(scriptUri, cspSource) {
  const nonce = crypto.randomBytes(16).toString("base64");
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'nonce-${nonce}'; script-src 'nonce-${nonce}' ${cspSource};">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<style nonce="${nonce}">
  html, body { height: 100%; }
  [hidden] { display: none !important; }
  body { margin: 0; padding: 0; display: flex; flex-direction: column; overflow: hidden;
    font-family: var(--vscode-font-family); font-size: 12px; color: var(--vscode-foreground); background: var(--vscode-editor-background); }
  button { font: inherit; color: var(--vscode-textLink-foreground); background: none; border: none; padding: 0 4px; cursor: pointer; }
  button:hover { text-decoration: underline; }
  button:disabled { opacity: 0.6; cursor: default; text-decoration: none; }
  .meta, .note, .ms { color: var(--vscode-descriptionForeground); }
  #bar { display: flex; gap: 10px; align-items: baseline; padding: 4px 10px; flex: none; min-width: 0; }
  #title { font-weight: 600; white-space: nowrap; }
  #summary { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .running { margin-left: auto; display: inline-flex; gap: 8px; align-items: baseline; white-space: nowrap; }
  .running button { color: var(--vscode-button-foreground); background: var(--vscode-button-background); padding: 0 8px; border-radius: 2px; }
  .running button:hover { background: var(--vscode-button-hoverBackground); text-decoration: none; }
  .messages { overflow: auto; padding: 4px 10px; font-family: var(--vscode-editor-font-family); font-size: 11px; }
  .line { display: flex; gap: 8px; align-items: baseline; white-space: nowrap; line-height: 17px; color: var(--vscode-descriptionForeground); }
  .line .mark { width: 1ch; flex: none; }
  .line.ok .mark { color: var(--vscode-testing-iconPassed, #73c991); }
  .line.error, .line.error .mark { color: var(--vscode-errorForeground); }
  .line.cancelled .mark { color: var(--vscode-editorWarning-foreground); }
  .line .sql { overflow: hidden; text-overflow: ellipsis; flex: 1; }
  .line .outcome { flex: none; }
  .ln { color: var(--vscode-textLink-foreground); cursor: pointer; text-decoration: none; flex: none; }
  .detail { margin: 2px 0 0 calc(1ch + 8px); white-space: pre-wrap; }
  .detail.error { color: var(--vscode-errorForeground); }
  .detail.cancelled { color: var(--vscode-editorWarning-foreground); }
  pre.context { margin: 2px 0 2px calc(1ch + 8px); color: var(--vscode-descriptionForeground); font-family: inherit; }
  #tabs { flex: none; display: flex; align-items: center; gap: 12px; padding: 0 10px; border-bottom: 1px solid var(--vscode-panel-border); min-width: 0; }
  #tablist { display: flex; gap: 2px; overflow-x: auto; min-width: 0; }
  #filter { margin-left: auto; display: flex; gap: 4px; align-items: center; flex: none; padding: 2px 0; }
  #filter input, #filter select { font: inherit; font-size: 11px; color: var(--vscode-input-foreground); background: var(--vscode-input-background); border: 1px solid var(--vscode-input-border, transparent); border-radius: 2px; padding: 1px 4px; }
  #filter input { width: 16em; }
  #filter input:focus, #filter select:focus { outline: 1px solid var(--vscode-focusBorder); outline-offset: -1px; }
  #filter input.invalid { border-color: var(--vscode-inputValidation-errorBorder, red); }
  .tab { color: var(--vscode-descriptionForeground); padding: 3px 8px; border-bottom: 1px solid transparent; margin-bottom: -1px; white-space: nowrap; }
  .tab:hover { color: var(--vscode-foreground); text-decoration: none; }
  .tab.active { color: var(--vscode-foreground); border-bottom-color: var(--vscode-focusBorder); }
  #main { flex: 1; min-height: 0; display: flex; }
  #main > * { flex: 1; min-width: 0; }
  pre.text { margin: 0; padding: 6px 10px; overflow: auto; font-family: var(--vscode-editor-font-family); font-size: 12px; }
  .empty { padding: 6px 10px; color: var(--vscode-descriptionForeground); }
  #status { flex: none; display: flex; gap: 12px; align-items: baseline; padding: 3px 10px; border-top: 1px solid var(--vscode-panel-border); white-space: nowrap; min-width: 0; }
  #status .spacer { flex: 1; }
  #status .actions { display: inline-flex; gap: 2px; align-items: baseline; }
  .grid { outline: none; display: flex; }
  .viewport { overflow: auto; position: relative; flex: 1; }
  .viewport canvas { position: sticky; top: 0; left: 0; display: block; }
  .sizer { pointer-events: none; }
</style>
</head>
<body>
<header id="bar"><span id="title">No results yet.</span><span id="summary" class="meta">Run a statement, or open a data file.</span></header>
<nav id="tabs" hidden><div id="tablist"></div><div id="filter" hidden><input id="filter-text" type="text" placeholder="Filter" aria-label="Filter rows" spellcheck="false"><select id="filter-mode" aria-label="How to match"><option value="contains">contains</option><option value="equals">equals</option><option value="starts">starts with</option><option value="regex">regex</option></select><select id="filter-col" aria-label="Which column"></select></div></nav>
<div id="main"></div>
<footer id="status" hidden></footer>
<script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`;
}

module.exports = {
  Channel,
  VIEW_ID,
  init,
  register,
  begin,
  add,
  running,
  end,
  exported,
  counted,
  show,
  openInEditor,
  setRevealHandler,
  setExportHandler,
  setCancelHandler,
  setCountHandler,
  html,
};
