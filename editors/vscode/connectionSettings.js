// The `grebe.duckdb.connections` setting, read and written through VS Code:
// the one place connections are kept (connections.js has the rules).
//
//   - Read: the entries of the user, workspace and workspace folder
//     settings together; for the same session and alias the most specific
//     wins.
//   - A new entry goes to the scope that already has the setting when
//     exactly one does, else to the user settings.
//   - Forget takes an entry out of every scope that has it; Detach and
//     Attach Again set or clear `detached` where the entry is.
//
// Only the first workspace folder is used for folder settings and for
// relative paths, as everywhere else in the extension.

"use strict";

const vscode = require("vscode");
const connections = require("./connections");

const SECTION = "grebe.duckdb";
const KEY = "connections";

function folder() {
  return (vscode.workspace.workspaceFolders || [])[0] || null;
}

function config() {
  const f = folder();
  return f ? vscode.workspace.getConfiguration(SECTION, f.uri) : vscode.workspace.getConfiguration(SECTION);
}

/** What VS Code holds of the setting in each scope. */
function inspect() {
  const cfg = config();
  return (cfg.inspect && cfg.inspect(KEY)) || { globalValue: cfg.get(KEY, undefined) };
}

const VALUE_OF = { workspaceFolder: "workspaceFolderValue", workspace: "workspaceValue", global: "globalValue" };

function target(scope) {
  const T = vscode.ConfigurationTarget;
  return scope === "workspaceFolder" ? T.WorkspaceFolder : scope === "workspace" ? T.Workspace : T.Global;
}

/** Every entry, of every session. */
function read() {
  const f = folder();
  return connections.merge(inspect(), f ? f.uri.fsPath : null);
}

/** The folder a file inside it is written relative to, in `scope`. */
function relTo(scope) {
  const f = folder();
  return f && scope !== "global" ? f.uri.fsPath : null;
}

async function write(scope, value) {
  await config().update(KEY, value && value.length ? value : undefined, target(scope));
}

/** Add `entries` (or replace the ones with the same session and alias), in
 *  the scope chosen for new entries. Resolves to that scope. */
async function add(entries) {
  const info = inspect();
  const scope = connections.chooseScope(info);
  let value = Array.isArray(info[VALUE_OF[scope]]) ? [...info[VALUE_OF[scope]]] : [];
  for (const e of entries) {
    value = connections.editItem(value, connections.sessionOf(e), e.alias, () => null);
    value.push(connections.toSetting(e, relTo(scope)));
  }
  await write(scope, value);
  return scope;
}

/** Take the entry for `session` and `alias` out of every scope. Resolves
 *  to the scopes it was in. */
async function remove(session, alias) {
  const info = inspect();
  const scopes = connections.scopesWith(info, session, alias);
  for (const scope of scopes) await write(scope, connections.editItem(info[VALUE_OF[scope]], session, alias, () => null));
  return scopes;
}

/** Mark the entry for `session` and `alias` detached (or not), in the
 *  scope it is read from. False when there is no such entry. */
async function setDetached(session, alias, detached) {
  const info = inspect();
  const [scope] = connections.scopesWith(info, session, alias);
  if (!scope) return false;
  const value = connections.editItem(info[VALUE_OF[scope]], session, alias, (item) => {
    const { detached: _, ...rest } = item;
    return detached ? { ...rest, detached: true } : rest;
  });
  await write(scope, value);
  return true;
}

module.exports = { read, add, remove, setDetached, inspect };
