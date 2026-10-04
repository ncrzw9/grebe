// extension.js against a stand-in for the `vscode` API. The stand-in client
// does a real LSP `initialize` exchange with the spawned server, so with
// GREBE_BIN set the real `grebe lsp` is exercised end to end.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const Module = require("node:module");
const os = require("node:os");
const path = require("node:path");
const { test, beforeEach } = require("node:test");

const unix = process.platform !== "win32";
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-ext-"));
function script(name, body) {
  const file = path.join(tmp, name);
  fs.writeFileSync(file, `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  return file;
}

// --- the stand-in -----------------------------------------------------------
const State = { Stopped: 1, Starting: 3, Running: 2 };
let settings;
let shown; // error notifications
let statusItem;
let spawned; // server processes the client started
let configListeners;

class FakeClient {
  constructor(id, name, serverOptions) {
    this.serverOptions = serverOptions;
    this.state = State.Stopped;
    this.listeners = [];
  }
  onDidChangeState(fn) {
    this.listeners.push(fn);
  }
  setState(s) {
    this.state = s;
    for (const fn of this.listeners) fn({ newState: s });
  }
  needsStop() {
    return this.state !== State.Stopped;
  }
  async start() {
    this.setState(State.Starting);
    const proc = await this.serverOptions();
    this.proc = proc;
    spawned.push(proc);
    const body = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { processId: null, rootUri: null, capabilities: {} } });
    proc.stdin.write(`Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`);
    await new Promise((resolve, reject) => {
      let buf = "";
      proc.stdout.on("data", (d) => {
        buf += d;
        if (/"id"\s*:\s*1/.test(buf) && /"result"/.test(buf)) resolve();
      });
      proc.on("exit", () => reject(new Error("server exited")));
    });
    this.setState(State.Running);
  }
  async stop() {
    if (this.state === State.Stopped) throw new Error("Client is not running and can't be stopped.");
    this.proc?.kill();
    this.setState(State.Stopped);
  }
  async dispose() {
    this.proc?.kill();
  }
}

const vscode = {
  StatusBarAlignment: { Right: 2 },
  ThemeColor: class { constructor(id) { this.id = id; } },
  window: {
    createOutputChannel: () => ({ lines: [], appendLine(l) { this.lines.push(l); }, show() {}, dispose() {} }),
    createStatusBarItem: () => (statusItem = { show() {}, dispose() {} }),
    showErrorMessage: (m) => { shown.push(m); return Promise.resolve(undefined); },
  },
  workspace: {
    getConfiguration: () => ({ get: (k, d) => (k in settings ? settings[k] : d) }),
    onDidChangeConfiguration: (fn) => { configListeners.push(fn); return { dispose() {} }; },
  },
  commands: { registerCommand: () => ({ dispose() {} }), executeCommand() {} },
};
// What runs SQL and shows results has its own tests (run-e2e.test.js);
// here it only has to be activated, and told when the server changes.
let clientChanges = 0;
const quiet = {
  "./results": { init() {}, register() {}, setCancelHandler() {} },
  "./run": { activate() {}, clientChanged: () => clientChanges++, cancel() {}, isRunning: () => false, currentSession() {}, liveSession() {}, onDidRun() {}, settings: () => ({}) },
  "./inspect": { activate() {} },
  "./catalog": { activate() {} },
  "./dataEditor": { activate() {} },
};
const realLoad = Module._load;
Module._load = function (request, ...rest) {
  if (request === "vscode") return vscode;
  if (request in quiet) return quiet[request];
  if (request === "vscode-languageclient/node") return { LanguageClient: FakeClient, State };
  return realLoad.call(this, request, ...rest);
};
const extension = require("../extension");

function context() {
  return { subscriptions: [], extensionPath: fs.mkdtempSync(path.join(tmp, "ext-")) };
}
beforeEach(() => {
  settings = {};
  shown = [];
  spawned = [];
  configListeners = [];
});

// --- the cases from the field ----------------------------------------------
test("grebe.path pointing at duckdb: an error naming the setting, nothing spawned, no file written", { skip: !unix }, async () => {
  const workspace = fs.mkdtempSync(path.join(tmp, "ws-"));
  settings.path = script("duckdb", `touch "${workspace}/lsp"; echo "v1.5.5 (Ossivalis)"`);
  await extension.activate(context());
  assert.equal(spawned.length, 0);
  assert.match(shown[0], /grebe\.path is ".*duckdb", which is not a working grebe binary/);
  assert.match(statusItem.text, /error/);
  await extension.deactivate(); // must not throw: the client never started
  // --version did run the fake (it touched the file); `lsp` never did.
  assert.deepEqual(fs.readdirSync(workspace), ["lsp"]);
});

test("a server that never answers initialize is stopped with an error", { skip: !unix, timeout: 20000 }, async () => {
  settings.path = script("silent", 'if [ "$1" = --version ]; then echo "grebe 0.5.1"; else exec sleep 60; fi');
  await extension.activate(context());
  assert.equal(spawned.length, 1);
  assert.match(shown[0], /did not answer initialize within 10s/);
  await new Promise((r) => setTimeout(r, 100));
  assert.ok(spawned[0].killed || spawned[0].exitCode !== null, "the hung server is killed");
  await extension.deactivate();
});

test("no grebe anywhere: an error saying where it looked", async () => {
  const savedPath = process.env.PATH;
  process.env.PATH = tmp; // no grebe here
  try {
    await extension.activate({ ...context(), extensionPath: tmp });
  } finally {
    process.env.PATH = savedPath;
  }
  assert.match(shown[0] ?? "", /No grebe binary was found/);
  await extension.deactivate();
});

test("the real grebe starts, and the status bar shows its version", { skip: !process.env.GREBE_BIN }, async () => {
  settings.path = process.env.GREBE_BIN;
  await extension.activate(context());
  assert.deepEqual(shown, []);
  assert.match(statusItem.text, /^\$\(check\) grebe \d+\.\d+\.\d+/);
  assert.ok(clientChanges > 0, "run.js hears that the server is up");
  await extension.deactivate();
  assert.ok(spawned[0].killed, "deactivate stops the server");
});

test("changing grebe.path restarts the server", { skip: !process.env.GREBE_BIN }, async () => {
  settings.path = process.env.GREBE_BIN;
  await extension.activate(context());
  assert.equal(spawned.length, 1);
  for (const fn of configListeners) fn({ affectsConfiguration: (s) => s === "grebe.path" });
  await new Promise((r) => setTimeout(r, 1500));
  assert.equal(spawned.length, 2);
  assert.match(statusItem.text, /^\$\(check\) grebe/);
  await extension.deactivate();
});
