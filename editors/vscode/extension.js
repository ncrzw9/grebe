// Thin client for `grebe lsp`: spawn the server over stdio and
// hand everything to vscode-languageclient. All behavior lives in the server.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { window, workspace } = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

let client;

// GUI apps launched from the Dock don't inherit the shell PATH, so a bare
// "grebe" often won't resolve. Unless grebe.path is set, prefer
// ~/.local/bin/grebe, a common per-user install location, when it exists.
function serverCommand(cfg) {
  const explicit = cfg.get("path", "");
  if (explicit) return explicit;
  const shim = path.join(os.homedir(), ".local", "bin", "grebe");
  return fs.existsSync(shim) ? shim : "grebe";
}

exports.activate = function activate() {
  const cfg = workspace.getConfiguration("grebe");
  const command = serverCommand(cfg);
  client = new LanguageClient(
    "grebe",
    "grebe",
    { command, args: ["lsp"] },
    {
      documentSelector: [{ scheme: "file", language: "sql" }],
      // Handed to the server once, in `initialize`'s initializationOptions.
      initializationOptions: { select: cfg.get("select", []) },
      // Registering the "grebe" section here makes vscode-languageclient
      // send workspace/didChangeConfiguration (with the whole "grebe"
      // section, select included) whenever the user edits any grebe.*
      // setting -- that's what lets the server re-lint open documents
      // without an editor restart.
      synchronize: { configurationSection: "grebe" },
    },
  );
  client.start().catch((err) => {
    window.showErrorMessage(
      `grebe: could not start "${command} lsp" (${err.message ?? err}). ` +
        'Check the "grebe.path" setting if grebe is not on your PATH.',
    );
  });
};

exports.deactivate = function deactivate() {
  return client ? client.stop() : undefined;
};
