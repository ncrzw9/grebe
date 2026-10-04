// The extension's entry point. Two halves:
//
//   - `grebe lsp`, the language server: diagnostics, fixes, formatting,
//     highlighting. This file finds a working grebe binary, starts it, and
//     says so plainly when it cannot.
//   - running SQL with the user's own `duckdb` CLI (run.js), the results
//     grid (results.js), the Catalog view (catalog.js), data files in the
//     grid (inspect.js, dataEditor.js). The language server tells run.js
//     where statements begin and end; nothing else here needs it.
const cp = require("child_process");
const { commands, StatusBarAlignment, ThemeColor, window, workspace } = require("vscode");
const { LanguageClient, State } = require("vscode-languageclient/node");
const { resolveServer } = require("./server");
const logger = require("./log");
const results = require("./results");
const runner = require("./run");
const inspect = require("./inspect");
const catalog = require("./catalog");
const dataEditor = require("./dataEditor");
const { Session } = require("./duckdb-session");

// A server that has not answered `initialize` by then is not going to: a
// healthy grebe answers in milliseconds.
const INITIALIZE_TIMEOUT_MS = 10000;

let output;
let status;
let client;
let generation = 0;
// The running language client, or null once a start has failed. Replaced on
// every (re)start; run.js awaits whichever is current.
let ready = Promise.resolve(null);
let settle = () => {};

exports.activate = function activate(context) {
  // Created up front, not on the client's first log line, so "grebe" is in
  // the Output view from the start and every start attempt is recorded.
  output = logger.output();
  status = window.createStatusBarItem(StatusBarAlignment.Right, 100);
  status.name = "grebe";
  status.command = "grebe.showOutput";
  context.subscriptions.push(
    output,
    status,
    logger.command("grebe.showOutput", () => output.show(true)),
    logger.command("grebe.restart", () => restart(context)),
    // grebe.select reaches the running server on its own (see `synchronize`
    // below); a new grebe.path needs a new process.
    workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("grebe.path")) restart(context);
    }),
  );
  results.init(context.extensionUri);
  results.register(context);
  // The shared results' Cancel button stops whatever is filling them: a run,
  // an export, a catalog preview or a file inspection.
  results.setCancelHandler(() => {
    if (runner.isRunning()) runner.cancel();
    else Session.cancelAll();
  });
  runner.activate(context, () => ready);
  inspect.activate(context);
  dataEditor.activate(context);
  const catalogView = catalog.activate(context, {
    currentSession: runner.currentSession,
    liveSession: runner.liveSession,
    onDidRun: runner.onDidRun,
    settings: runner.settings,
    cli: () => runner.settings().cli,
  });
  // Trust granted: the folder's own grebe.path now applies, and DuckDB may
  // run.
  context.subscriptions.push(
    workspace.onDidGrantWorkspaceTrust(() => {
      restart(context);
      catalogView.refresh();
    }),
  );
  return start(context);
};

exports.deactivate = function deactivate() {
  // Every duckdb process ends with the extension, busy or not.
  Session.disposeAll();
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
  settle(null);
  ready = new Promise((resolve) => {
    settle = resolve;
  });
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
    settle(null);
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
    settle(next);
    runner.clientChanged();
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
    settle(null);
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
  logger.info("server", line);
}
