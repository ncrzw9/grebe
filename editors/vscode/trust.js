// Workspace trust. In a folder VS Code has not been told to trust
// (Restricted Mode), grebe still lints and formats: the bundled binary and
// the user's own settings are the user's choice, not the folder's. Anything
// that runs the duckdb CLI waits for trust: a folder's SQL can read and
// write files, and VS Code withholds the folder's own grebe.path and
// grebe.duckdb.path until then (package.json lists them as restricted).

"use strict";

const vscode = require("vscode");

/** True if the folder is trusted. Otherwise says why `what` needs it, with
 *  a button to VS Code's trust dialog, and returns false. */
function ok(what) {
  if (trusted()) return true;
  Promise.resolve(
    vscode.window.showWarningMessage(
      `grebe: ${what} needs a trusted folder. This folder is open in Restricted Mode; linting and formatting still work.`,
      "Manage Workspace Trust",
    ),
  ).then((pick) => {
    if (pick) vscode.commands.executeCommand("workbench.trust.manage");
  });
  return false;
}

/** Whether DuckDB may run here, without saying anything. */
function trusted() {
  return vscode.workspace.isTrusted !== false;
}

module.exports = { ok, trusted };
