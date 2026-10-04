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
const handlers = { reveal: () => {}, export: () => {} };

/** Where media/ lives; set once from activate(). */
function init(uri) {
  extensionUri = uri;
}

/** Called with (uri, line) when a statement's line link is clicked. */
function setRevealHandler(handler) {
  handlers.reveal = handler;
}

/** Called with (id, format) when an export button is clicked. */
function setExportHandler(handler) {
  handlers.export = handler;
}

class Channel {
  constructor() {
    this.webviews = new Set();
    this.log = [];
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
    // The page cannot reach the system clipboard itself; the extension can.
    if (m.type === "copy" && typeof m.text === "string") {
      vscode.env.clipboard.writeText(m.text);
      vscode.window.setStatusBarMessage(`grebe: copied ${m.text.split("\n").length} line(s)`, 2500);
      return undefined;
    }
    // Returned so a caller (the tests) can wait for the file to be written.
    if (m.type === "export") return handlers.export(m.id, m.format);
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
const end = (summary) => shared.end(summary);
const exported = (id, message) => shared.exported(id, message);

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
  body { font-family: var(--vscode-font-family); font-size: 12px; color: var(--vscode-foreground); background: var(--vscode-editor-background); padding: 0 10px 16px; margin: 0; }
  header { position: sticky; top: 0; background: var(--vscode-editor-background); padding: 6px 0 4px; border-bottom: 1px solid var(--vscode-panel-border); z-index: 3; display: flex; gap: 10px; align-items: baseline; }
  #title { font-weight: 600; }
  .meta, .note { color: var(--vscode-descriptionForeground); }
  section { margin-top: 10px; }
  .stmt { font-family: var(--vscode-editor-font-family); font-size: 11px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; color: var(--vscode-descriptionForeground); }
  .stmt a { color: var(--vscode-textLink-foreground); cursor: pointer; text-decoration: none; margin-right: 2px; }
  .error { color: var(--vscode-errorForeground); white-space: pre-wrap; font-family: var(--vscode-editor-font-family); font-size: 12px; margin-top: 2px; }
  pre.context { font-family: var(--vscode-editor-font-family); font-size: 12px; margin: 2px 0 0; color: var(--vscode-descriptionForeground); overflow-x: auto; }
  pre.text { font-family: var(--vscode-editor-font-family); font-size: 12px; overflow-x: auto; margin: 4px 0; }
  .bar { display: flex; flex-wrap: wrap; gap: 4px 14px; align-items: baseline; margin: 2px 0 3px; font-size: 11px; }
  .export { display: inline-flex; gap: 2px; align-items: baseline; margin-left: auto; }
  .export button { font: inherit; padding: 0 4px; cursor: pointer; color: var(--vscode-textLink-foreground); background: none; border: none; }
  .export button:hover { text-decoration: underline; }
  .grid { outline: none; border: 1px solid var(--vscode-panel-border); }
  .grid:focus { border-color: var(--vscode-focusBorder); }
  .viewport { overflow: auto; position: relative; }
  .viewport canvas { position: sticky; top: 0; left: 0; display: block; }
  .sizer { pointer-events: none; }
</style>
</head>
<body>
<header><div id="title">No results yet. Run a statement, or open a data file.</div><div id="summary" class="meta"></div></header>
<main id="runs"></main>
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
  end,
  exported,
  show,
  openInEditor,
  setRevealHandler,
  setExportHandler,
  html,
};
