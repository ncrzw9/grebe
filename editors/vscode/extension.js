// Thin client for `grebe lsp`: find a working grebe binary, spawn the server
// over stdio, and hand everything to vscode-languageclient. All linting and
// formatting behavior lives in the server; this file only makes sure the
// server starts, and says so plainly when it does not.
const cp = require("child_process");
const { commands, StatusBarAlignment, ThemeColor, window, workspace } = require("vscode");
const { LanguageClient, State } = require("vscode-languageclient/node");
const { resolveServer } = require("./server");

// A server that has not answered `initialize` by then is not going to: a
// healthy grebe answers in milliseconds.
const INITIALIZE_TIMEOUT_MS = 10000;

let output;
let status;
let client;
let generation = 0;

exports.activate = function activate(context) {
  // Created up front, not on the client's first log line, so "grebe" is in
  // the Output view from the start and every start attempt is recorded.
  output = window.createOutputChannel("grebe");
  status = window.createStatusBarItem(StatusBarAlignment.Right, 100);
  status.name = "grebe";
  status.command = "grebe.showOutput";
  context.subscriptions.push(
    output,
    status,
    commands.registerCommand("grebe.showOutput", () => output.show(true)),
    commands.registerCommand("grebe.restart", () => restart(context)),
    // grebe.select reaches the running server on its own (see `synchronize`
    // below); a new grebe.path needs a new process.
    workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("grebe.path")) restart(context);
    }),
  );
  return start(context);
};

exports.deactivate = function deactivate() {
  return stopClient();
};

async function restart(context) {
  await stopClient();
  await start(context);
}

async function stopClient() {
  const old = client;
  client = undefined;
  if (!old) return;
  // stop() throws unless the client got as far as starting; a server that
  // never ran has nothing to shut down.
  try {
    if (old.needsStop()) await old.stop();
  } catch (err) {
    log(`stop: ${err.message ?? err}`);
  }
}

async function start(context) {
  const attempt = ++generation;
  const cfg = workspace.getConfiguration("grebe");
  const explicit = cfg.get("path", "");
  setStatus("$(sync~spin) grebe", "Starting the grebe language server");

  const { found, tried } = await resolveServer(explicit, context.extensionPath);
  if (attempt !== generation) return; // superseded by a restart meanwhile
  for (const t of tried) log(`skipped ${t.command}: ${t.reason}`);
  if (!found) {
    const where = explicit
      ? `grebe.path is "${explicit}", which is not a working grebe binary (${tried[0].reason}).`
      : "No grebe binary was found: not bundled with this extension, not in ~/.local/bin, not on PATH.";
    fail(where);
    return;
  }
  log(`using ${found.command} (grebe ${found.version})`);

  let server;
  const next = new LanguageClient(
    "grebe",
    "grebe",
    () => {
      server = cp.spawn(found.command, ["lsp"], { windowsHide: true });
      return Promise.resolve(server);
    },
    {
      documentSelector: [{ scheme: "file", language: "sql" }],
      outputChannel: output,
      // Handed to the server once, in `initialize`'s initializationOptions.
      initializationOptions: { select: cfg.get("select", []) },
      // Registering the "grebe" section makes the client send
      // workspace/didChangeConfiguration whenever any grebe.* setting
      // changes, which lets the server re-lint open documents in place.
      synchronize: { configurationSection: "grebe" },
    },
  );
  client = next;
  next.onDidChangeState(({ newState }) => {
    if (client !== next) return;
    if (newState === State.Running) {
      setStatus(`$(check) grebe ${found.version}`, `grebe ${found.version} is linting SQL files\n${found.command}`);
    } else if (newState === State.Stopped) {
      setStatus("$(error) grebe", "The grebe language server stopped. Click for its output.", true);
    }
  });

  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(
      () => reject(new Error(`"${found.command} lsp" did not answer initialize within ${INITIALIZE_TIMEOUT_MS / 1000}s`)),
      INITIALIZE_TIMEOUT_MS,
    );
  });
  try {
    await Promise.race([next.start(), timeout]);
    log("server running");
  } catch (err) {
    if (client !== next) return;
    server?.kill();
    client = undefined;
    try {
      await next.dispose();
    } catch {
      // Already torn down by the failed start.
    }
    fail(err.message ?? String(err));
  } finally {
    clearTimeout(timer);
  }
}

function fail(message) {
  log(message);
  setStatus("$(error) grebe", `${message}\nClick for the grebe output.`, true);
  window
    .showErrorMessage(`grebe: ${message}`, "Open Settings", "Show Output", "Install grebe")
    .then((choice) => {
      if (choice === "Open Settings") commands.executeCommand("workbench.action.openSettings", "grebe.path");
      else if (choice === "Show Output") output.show(true);
      else if (choice === "Install grebe")
        commands.executeCommand("vscode.open", "https://github.com/ncrzw9/grebe#install");
    });
}

function setStatus(text, tooltip, error = false) {
  status.text = text;
  status.tooltip = tooltip;
  status.backgroundColor = error ? new ThemeColor("statusBarItem.errorBackground") : undefined;
  status.show();
}

function log(line) {
  output.appendLine(`[${new Date().toLocaleTimeString()}] ${line}`);
}
