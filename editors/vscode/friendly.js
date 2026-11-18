// "grebe: Rewrite as Friendly SQL": turn conventional SQL into DuckDB's
// friendly SQL (FROM first, GROUP BY ALL, count() and the rest of what
// `grebe check --fix --select FRIENDLY` applies), then format what changed.
//
// The language server does the rewriting (`grebe/friendly`) and answers
// with text edits; this file asks, and applies them as one undo step. With
// a selection only the statements it touches are rewritten; without one,
// the whole document.

"use strict";

const vscode = require("vscode");
const log = require("./log");

/**
 * Rewrite `editor`'s document through `client`. Resolves to how many edits
 * were applied; 0 with a message saying why when there was nothing to do.
 */
async function rewrite(editor, client) {
  const doc = editor.document;
  if (doc.languageId !== "sql" || doc.uri.scheme !== "file") {
    vscode.window.showInformationMessage("grebe: Rewrite as Friendly SQL works on SQL files saved to disk.");
    return 0;
  }
  if (!client) {
    vscode.window.showErrorMessage("grebe: the language server is not running, so there is nothing to rewrite with.");
    return 0;
  }
  const params = { textDocument: { uri: doc.uri.toString() } };
  const selection = editor.selection;
  if (selection && !selection.isEmpty) {
    params.range = {
      start: { line: selection.start.line, character: selection.start.character },
      end: { line: selection.end.line, character: selection.end.character },
    };
  }
  const version = doc.version;
  const edits = await client.sendRequest("grebe/friendly", params);
  if (!edits || edits.length === 0) {
    const where = params.range ? "the selected statements" : "this file";
    vscode.window.showInformationMessage(`grebe: nothing to rewrite in ${where}.`);
    return 0;
  }
  // The edits are positions in the text the server saw. Typing since then
  // moved them; applying them now would overwrite the wrong text.
  if (doc.version !== version) {
    vscode.window.showInformationMessage("grebe: the file changed while it was being rewritten. Run the command again.");
    return 0;
  }
  const applied = await editor.edit((builder) => {
    for (const e of edits) {
      const range = new vscode.Range(
        new vscode.Position(e.range.start.line, e.range.start.character),
        new vscode.Position(e.range.end.line, e.range.end.character),
      );
      builder.replace(range, e.newText);
    }
  });
  if (!applied) throw new Error("the editor refused the edit");
  return edits.length;
}

/** Register the command. `clientReady()` resolves to the running language
 *  client, or null when there is none. */
function activate(context, clientReady) {
  context.subscriptions.push(
    log.command("grebe.friendly", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) return 0;
      return rewrite(editor, await clientReady());
    }),
  );
}

module.exports = { activate, rewrite };
