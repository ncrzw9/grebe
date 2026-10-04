// End to end without VS Code: run.js's real commands, driven against the real
// `grebe lsp` binary ($GREBE_BIN) for statement splitting and the real
// `duckdb` CLI ($DUCKDB_CLI) for execution. Only the `vscode` API is a
// stand-in, recording what the extension would show. Skipped unless both
// binaries are given.

"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const Module = require("module");
const { spawn } = require("child_process");

const CLI = process.env.DUCKDB_CLI;
const GREBE = process.env.GREBE_BIN;
const live = CLI && GREBE && fs.existsSync(CLI) && fs.existsSync(GREBE) ? test : test.skip;

// ------------------------------------------------------------ fake vscode --

class Position {
  constructor(line, character) {
    this.line = line;
    this.character = character;
  }
  translate(dl, dc) {
    return new Position(this.line + dl, this.character + dc);
  }
}
class Range {
  constructor(start, end) {
    this.start = start;
    this.end = end;
  }
}
class Selection extends Range {}
class CodeLens {
  constructor(range, command) {
    this.range = range;
    this.command = command;
  }
}
class EventEmitter {
  constructor() {
    this.event = () => ({ dispose() {} });
  }
  fire() {}
  dispose() {}
}
class MarkdownString {
  constructor() {
    this.value = "";
  }
  appendMarkdown(t) {
    this.value += t;
    return this;
  }
  appendCodeblock(t) {
    this.value += "\n```\n" + t + "\n```\n";
    return this;
  }
}
class Hover {
  constructor(contents, range) {
    this.contents = contents;
    this.range = range;
  }
}
class Diagnostic {
  constructor(range, message, severity) {
    Object.assign(this, { range, message, severity });
  }
}

function fakeDocument(fsPath, text) {
  const lineStarts = [0];
  for (let i = 0; i < text.length; i++) if (text[i] === "\n") lineStarts.push(i + 1);
  const uri = { fsPath, scheme: "file", toString: () => `file://${fsPath}` };
  return {
    uri,
    fileName: fsPath,
    languageId: "sql",
    getText: () => text,
    offsetAt: (p) => lineStarts[p.line] + p.character,
    positionAt(off) {
      let line = 0;
      while (line + 1 < lineStarts.length && lineStarts[line + 1] <= off) line++;
      return new Position(line, off - lineStarts[line]);
    },
    lineAt: (n) => ({ text: text.split("\n")[n] }),
    getWordRangeAtPosition(p) {
      const off = this.offsetAt(p);
      const m = /^[A-Za-z_][A-Za-z0-9_]*/.exec(text.slice(off));
      return m ? new Range(p, this.positionAt(off + m[0].length)) : undefined;
    },
  };
}

function makeVscode(config) {
  const shown = { posted: [], errors: [], warnings: [], infos: [], log: [], diagnostics: new Map(), commands: new Map(), lensProviders: [], docs: new Map() };
  const disposable = { dispose() {} };
  const vscode = {
    Position,
    Range,
    Selection,
    Diagnostic,
    CodeLens,
    EventEmitter,
    MarkdownString,
    Hover,
    ViewColumn: { One: 1, Beside: -2 },
    StatusBarAlignment: { Right: 2 },
    DiagnosticSeverity: { Error: 0 },
    ConfigurationTarget: { Global: 1, Workspace: 2 },
    TextEditorRevealType: { InCenter: 2 },
    Uri: {
      parse: (s) => ({ toString: () => s, fsPath: s.replace(/^file:\/\//, "") }),
      file: (p) => ({ toString: () => `file://${p}`, fsPath: p }),
      joinPath: (u, ...parts) => ({ toString: () => path.join(u.fsPath, ...parts), fsPath: path.join(u.fsPath, ...parts) }),
    },
    env: { clipboard: { writeText: async (t) => (shown.clipboard = t) } },
    workspace: {
      getConfiguration: () => ({ get: (k, d) => (k in config ? config[k] : d), update: async () => {} }),
      getWorkspaceFolder: () => undefined,
      asRelativePath: (u) => path.basename(typeof u === "string" ? u : u.fsPath),
      onDidChangeTextDocument: () => disposable,
      openTextDocument: async (uri) => shown.docs.get(uri.toString()),
      onDidChangeConfiguration: (h) => {
        shown.configChanged = h;
        return disposable;
      },
    },
    window: {
      activeTextEditor: null,
      showInformationMessage: async (m) => {
        shown.infos.push(m);
        return undefined; // no button clicked
      },
      showErrorMessage: async (m) => {
        shown.errors.push(m);
      },
      showWarningMessage: async (m) => {
        shown.warnings.push(m);
      },
      showSaveDialog: async () => shown.saveTo,
      setStatusBarMessage: () => disposable,
      createOutputChannel: () => ({
        append: (t) => shown.log.push(t),
        appendLine: (t) => shown.log.push(`${t}\n`),
        show() {},
        dispose() {},
      }),
      createStatusBarItem: () => ({ show() {}, dispose() {} }),
      registerWebviewViewProvider: (id, provider) => {
        shown.viewProvider = provider;
        return disposable;
      },
      createWebviewPanel: () => ({
        webview: {
          html: "",
          postMessage: (m) => shown.posted.push(m),
          onDidReceiveMessage: (h) => {
            shown.fromPage = h;
            return disposable;
          },
          asWebviewUri: (u) => u,
          cspSource: "vscode-resource:",
        },
        reveal() {},
        onDidDispose: () => disposable,
      }),
    },
    languages: {
      registerCodeLensProvider: (_selector, provider) => {
        shown.lensProviders.push(provider);
        return disposable;
      },
      createDiagnosticCollection: () => ({
        set: (uri, ds) => shown.diagnostics.set(uri.toString(), ds),
        delete: (uri) => shown.diagnostics.delete(uri.toString()),
        dispose() {},
      }),
    },
    commands: {
      executeCommand: async (name, ...args) => {
        shown.executed = shown.executed || [];
        shown.executed.push([name, ...args]);
      },
      registerCommand: (name, fn) => {
        shown.commands.set(name, fn);
        return disposable;
      },
    },
  };
  return { vscode, shown };
}

// ------------------------------------------------- real grebe lsp client --

function lspClient(bin) {
  const proc = spawn(bin, ["lsp"], { stdio: ["pipe", "pipe", "inherit"] });
  let buf = Buffer.alloc(0);
  let id = 0;
  const pending = new Map();
  proc.stdout.on("data", (d) => {
    buf = Buffer.concat([buf, d]);
    for (;;) {
      const head = buf.indexOf("\r\n\r\n");
      if (head < 0) return;
      const len = Number(/Content-Length: (\d+)/i.exec(buf.subarray(0, head).toString())[1]);
      if (buf.length < head + 4 + len) return;
      const msg = JSON.parse(buf.subarray(head + 4, head + 4 + len).toString());
      buf = buf.subarray(head + 4 + len);
      if (msg.id !== undefined && pending.has(msg.id)) {
        pending.get(msg.id)(msg.result);
        pending.delete(msg.id);
      }
    }
  });
  const send = (msg) => {
    const body = Buffer.from(JSON.stringify({ jsonrpc: "2.0", ...msg }));
    proc.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
    proc.stdin.write(body);
  };
  return {
    request(method, params) {
      const myId = ++id;
      return new Promise((resolve) => {
        pending.set(myId, resolve);
        send({ id: myId, method, params });
      });
    },
    notify: (method, params) => send({ method, params }),
    // The shape run.js uses from vscode-languageclient.
    sendRequest(method, params) {
      return this.request(method, params);
    },
    code2ProtocolConverter: {
      asRange: (r) => ({
        start: { line: r.start.line, character: r.start.character },
        end: { line: r.end.line, character: r.end.character },
      }),
    },
    protocol2CodeConverter: {
      asRange: (r) =>
        new Range(new Position(r.start.line, r.start.character), new Position(r.end.line, r.end.character)),
    },
    stop() {
      proc.kill();
    },
  };
}


/** A page for the results to render into: what the Results view gives the
 *  extension once VS Code resolves it. Messages it receives land in
 *  shown.posted; shown.fromPage sends one back as the page would. */
function fakeWebview(shown, posted = shown.posted) {
  return {
    html: "",
    options: {},
    postMessage: (m) => posted.push(m),
    onDidReceiveMessage: (h) => {
      shown.fromPage = h;
      return { dispose() {} };
    },
    asWebviewUri: (u) => u,
    cspSource: "vscode-resource:",
  };
}

function resolveResultsView(vscode, shown) {
  const results = require("../results");
  results.init({ fsPath: path.join(__dirname, "..") });
  const subs = [];
  results.register({ subscriptions: subs });
  const webview = fakeWebview(shown);
  shown.viewProvider.resolveWebviewView({ webview, show() {}, onDidDispose: () => ({ dispose() {} }) });
  return webview;
}

// ------------------------------------------------------------------ test --

live("run file and run statement, end to end", async (t) => {
  const { vscode, shown } = makeVscode({ path: CLI, database: ":memory:", maxRows: 2 });
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../run", "../results", "../log"]) delete require.cache[require.resolve(m)];
  const runner = require("../run");

  const client = lspClient(GREBE);
  t.after(() => client.stop());
  await client.request("initialize", { capabilities: {} });

  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-e2e-"));
  const file = path.join(dir, "load.sql");
  const sql = [
    "-- a load script",
    "CREATE TABLE t AS SELECT range AS i, 'row;' || range AS label FROM range(5);",
    "SELECT i, label FROM t ORDER BY i;",
    "SELECT nope FROM t;",
    "SELECT 'never runs';",
  ].join("\n");
  fs.writeFileSync(file, sql);
  const doc = fakeDocument(file, sql);
  shown.docs.set(doc.uri.toString(), doc);
  client.notify("textDocument/didOpen", {
    textDocument: { uri: doc.uri.toString(), languageId: "sql", version: 1, text: sql },
  });

  const subs = [];
  runner.activate({ subscriptions: subs, extensionUri: { fsPath: path.join(__dirname, "..") } }, () => Promise.resolve(client));
  t.after(() => subs.forEach((s) => s.dispose && s.dispose()));
  resolveResultsView(vscode, shown);

  // --- run the whole file: stops at the error, marks it, skips the rest.
  vscode.window.activeTextEditor = { document: doc, selection: new Selection(new Position(0, 0), new Position(0, 0)) };
  await shown.commands.get("grebe.duckdb.runFile")();

  const results = shown.posted.filter((m) => m.type === "result").map((m) => m.entry);
  assert.equal(results.length, 3, JSON.stringify(shown.posted, null, 1));
  assert.equal(results[0].result.kind, "ok");
  assert.equal(results[0].line, 1, "the leading comment is not part of the statement");
  const rows = results[1].result;
  assert.equal(rows.kind, "rows");
  assert.deepEqual(rows.columns, ["i", "label"]);
  // Capped inside DuckDB: maxRows (2) and one more, to know there are more.
  assert.equal(rows.more, true);
  assert.equal(rows.data[0].length, 2);
  assert.deepEqual(rows.data, [["0", "1"], ["row;0", "row;1"]], "capped at maxRows, column-major text");
  assert.deepEqual(rows.num, [true, false]);
  assert.equal(results[2].result.kind, "error");
  assert.equal(results[2].result.type, "Binder");
  const end = shown.posted.findLast((m) => m.type === "end");
  assert.match(end.summary, /Stopped at the error: 2 succeeded, 1 not run/);

  const diags = shown.diagnostics.get(doc.uri.toString());
  assert.equal(diags.length, 1);
  assert.equal(diags[0].range.start.line, 3);
  assert.equal(diags[0].range.start.character, "SELECT ".length, "points at `nope`");
  assert.equal(diags[0].source, "duckdb");

  // The error says where: the line, and a caret under `nope` -- in the grid
  // and in the log, with the statement in full.
  assert.deepEqual(results[2].result.context, { line: 3, text: "SELECT nope FROM t;", column: 7 });
  const logged = shown.log.join("");
  assert.match(logged, /\[duckdb\] load\.sql:2 ok/);
  assert.match(logged, /\[duckdb\] load\.sql:3 2\+ rows × 2/);
  assert.match(logged, /\[duckdb\] load\.sql:4 Binder Error: Referenced column "nope" not found/);
  assert.match(logged, /\[duckdb\] 4 \| SELECT nope FROM t;\n.*\[duckdb\]   \|        \^/);

  // --- run one statement at the cursor, in the same session: t still exists.
  shown.posted.length = 0;
  vscode.window.activeTextEditor = { document: doc, selection: new Selection(new Position(2, 3), new Position(2, 3)) };
  await shown.commands.get("grebe.duckdb.runStatement")();
  const one = shown.posted.filter((m) => m.type === "result");
  assert.equal(one.length, 1);
  assert.equal(one[0].entry.result.more, true);
  assert.equal(shown.errors.length, 0, shown.errors.join("\n"));

  // --- a ▶ Run lens above each statement, and clicking one runs just that.
  assert.equal(shown.lensProviders.length, 1);
  const lenses = await shown.lensProviders[0].provideCodeLenses(doc);
  assert.deepEqual(
    lenses.map((l) => l.range.start.line),
    [1, 2, 3, 4],
    "one lens per statement, on its first line, past the leading comment",
  );
  assert.ok(lenses.every((l) => l.command.command === "grebe.duckdb.runRange"));
  shown.posted.length = 0;
  await shown.commands.get("grebe.duckdb.runRange")(...lenses[1].command.arguments);
  const clicked = shown.posted.filter((m) => m.type === "result");
  assert.equal(clicked.length, 1, "exactly the one statement");
  assert.equal(clicked[0].entry.line, 2);
  assert.equal(clicked[0].entry.result.more, true);

  // Count asks DuckDB for the whole total.
  shown.posted.length = 0;
  await shown.fromPage({ type: "count", id: clicked[0].entry.id });
  assert.deepEqual(shown.posted.filter((m) => m.type === "counted"), [{ type: "counted", id: clicked[0].entry.id, total: 5 }]);
  shown.posted.length = 0;

  // --- types from DESCRIBE, and export straight from the panel.
  const entry = clicked[0].entry;
  assert.deepEqual(entry.result.types, ["BIGINT", "VARCHAR"]);
  assert.equal(entry.exportable, true);
  const out = path.join(dir, "export 'quoted'.parquet");
  shown.saveTo = { fsPath: out, toString: () => out };
  shown.posted.length = 0;
  await shown.fromPage({ type: "export", id: entry.id, format: "parquet" });
  assert.ok(fs.existsSync(out), "the Parquet file was written");
  const notes = shown.posted.filter((m) => m.type === "exported").map((m) => m.message);
  assert.match(notes.at(-1), /^Saved /, notes.join(" | "));

  // --- copy from the grid goes to the system clipboard via the extension.
  await shown.fromPage({ type: "copy", text: "i\tlabel\n0\trow;0" });
  assert.equal(shown.clipboard, "i\tlabel\n0\trow;0");

  // --- an INSERT is never exportable; an empty SELECT still names columns.
  shown.posted.length = 0;
  const extra = "INSERT INTO t SELECT 99, 'x';\nSELECT * FROM t WHERE i < 0;";
  const doc2 = fakeDocument(path.join(dir, "more.sql"), extra);
  shown.docs.set(doc2.uri.toString(), doc2);
  client.notify("textDocument/didOpen", {
    textDocument: { uri: doc2.uri.toString(), languageId: "sql", version: 1, text: extra },
  });
  vscode.window.activeTextEditor = { document: doc2, selection: new Selection(new Position(0, 0), new Position(0, 0)) };
  await shown.commands.get("grebe.duckdb.runFile")();
  const [ins, empty] = shown.posted.filter((m) => m.type === "result").map((m) => m.entry);
  assert.equal(ins.exportable, false);
  assert.deepEqual([empty.result.total, empty.result.columns], [0, ["i", "label"]]);
});

live("data files: columns, stats, parquet metadata, csv dialect, and hover", async (t) => {
  const { vscode, shown } = makeVscode({ path: CLI, database: ":memory:", maxRows: 100 });
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../inspect", "../results", "../log"]) delete require.cache[require.resolve(m)];
  const inspect = require("../inspect");
  resolveResultsView(vscode, shown);
  const subs = [];
  vscode.languages.registerHoverProvider = () => ({ dispose() {} });
  inspect.activate({ subscriptions: subs });
  t.after(() => subs.forEach((s) => s.dispose && s.dispose()));

  // Real files, written by the same CLI.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-inspect-"));
  const { Session } = require("../duckdb-session");
  const w = new Session({ cli: CLI, database: ":memory:", cwd: dir });
  await w.start();
  await w.run("COPY (SELECT range AS id, 'c' || range AS name, CASE WHEN range % 4 = 0 THEN NULL ELSE range * 1.5 END AS amt FROM range(1000)) TO 'orders.parquet'");
  await w.run("COPY (SELECT range AS id, 'c' || range AS name FROM range(50)) TO 'people.tsv' (DELIMITER '\\t')");
  w.dispose();

  const results = () => shown.posted.filter((m) => m.type === "result").map((m) => m.entry.result);
  const parquet = vscode.Uri.file(path.join(dir, "orders.parquet"));
  const tsv = vscode.Uri.file(path.join(dir, "people.tsv"));

  shown.posted.length = 0;
  await shown.commands.get("grebe.inspect.columns")(parquet);
  let [r] = results();
  assert.deepEqual(r.columns, ["column", "type"]);
  assert.deepEqual(r.data, [["id", "name", "amt"], ["BIGINT", "VARCHAR", "DECIMAL(21,1)"]]);

  shown.posted.length = 0;
  await shown.commands.get("grebe.inspect.stats")(parquet);
  [r] = results();
  assert.ok(r.columns.includes("null_percentage"), r.columns.join());
  assert.equal(r.data[r.columns.indexOf("column_name")][2], "amt");

  shown.posted.length = 0;
  await shown.commands.get("grebe.inspect.parquet")(parquet);
  const [file, cols] = results();
  assert.equal(file.data[file.columns.indexOf("num_rows")][0], "1000");
  assert.deepEqual(cols.data[cols.columns.indexOf("column")], ["id", "name", "amt"]);
  assert.equal(cols.data[cols.columns.indexOf("nulls")][2], "250");

  shown.posted.length = 0;
  await shown.commands.get("grebe.inspect.dialect")(tsv);
  [r] = results();
  const settings = r.data[r.columns.indexOf("setting")];
  const values = r.data[r.columns.indexOf("value")];
  assert.equal(values[settings.indexOf("Delimiter")], "\t");
  assert.match(values[settings.indexOf("Prompt")], /^FROM read_csv\(/);

  shown.posted.length = 0;
  await shown.commands.get("grebe.inspect.preview")(tsv);
  [r] = results();
  assert.equal(r.total, 50);

  // Hover a path in SQL: the column list, relative to the file's folder.
  const sql = "SELECT * FROM 'orders.parquet' JOIN read_csv('people.tsv') USING (id);\nSELECT 'not a file', 'https://x.test/a.parquet';";
  const doc = fakeDocument(path.join(dir, "q.sql"), sql);
  const h = await inspect.hoverProvider.provideHover(doc, new Position(0, 20));
  assert.ok(h, "a hover for orders.parquet");
  assert.match(h.contents.value, /\*\*orders\.parquet\*\* — 3 columns/);
  assert.match(h.contents.value, /amt\s+DECIMAL\(21,1\)/);
  assert.match(h.contents.value, /command:grebe\.inspect\.stats/);
  const h2 = await inspect.hoverProvider.provideHover(doc, new Position(0, 48));
  assert.match(h2.contents.value, /people\.tsv\*\* — 2 columns/);
  assert.match(h2.contents.value, /CSV dialect/);
  assert.equal(await inspect.hoverProvider.provideHover(doc, new Position(1, 10)), null, "not a data file");
  assert.equal(await inspect.hoverProvider.provideHover(doc, new Position(1, 30)), null, "URLs are left alone");
  assert.equal(shown.errors.length, 0, shown.errors.join("\n"));
});

live("Catalog: session contents, temp tables, a browsed file", async (t) => {
  const { vscode, shown } = makeVscode({ path: CLI, database: ":memory:", maxRows: 100 });
  class TreeItem {
    constructor(label, state) {
      this.label = label;
      this.collapsibleState = state;
    }
  }
  Object.assign(vscode, {
    TreeItem,
    ThemeIcon: class {
      constructor(id) {
        this.id = id;
      }
    },
    TreeItemCollapsibleState: { None: 0, Collapsed: 1, Expanded: 2 },
  });
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../catalog", "../results", "../log"]) delete require.cache[require.resolve(m)];
  const { Explorer } = require("../catalog");
  resolveResultsView(vscode, shown);
  const { Session } = require("../duckdb-session");

  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-catalog-"));
  const other = path.join(dir, "warehouse.duckdb");
  const w = new Session({ cli: CLI, database: other, cwd: dir });
  await w.start();
  await w.run("CREATE TABLE dim_customer AS SELECT range AS id FROM range(42)");
  w.dispose();
  await new Promise((r) => setTimeout(r, 200)); // let the writer release its lock

  let sess = null;
  const ex = new Explorer({
    currentSession: async () => sess,
    liveSession: () => sess,
    settings: () => ({ cli: CLI, database: ":memory:" }),
    cli: () => CLI,
  });
  t.after(() => {
    ex.dispose();
    if (sess) sess.dispose();
  });
  const labels = (nodes) => nodes.map((n) => `${n.item.label}${n.item.description ? " (" + n.item.description + ")" : ""}`);

  // Before a session exists: say so, don't start one behind your back.
  let [root] = await ex.getChildren();
  assert.deepEqual(labels(await ex.getChildren(root)), ["Start DuckDB session (or run a statement)"]);

  sess = new Session({ cli: CLI, database: ":memory:", cwd: dir });
  await sess.start();
  await sess.run("CREATE SCHEMA staging");
  await sess.run("CREATE TABLE staging.orders AS SELECT range AS id, 'c' || range AS name FROM range(100)");
  await sess.run("CREATE VIEW v_orders AS SELECT id FROM staging.orders");
  await sess.run("CREATE TEMP TABLE scratch AS SELECT 1 AS a");

  [root] = await ex.getChildren();
  const dbs = await ex.getChildren(root);
  assert.deepEqual(labels(dbs).slice(0, 2), ["memory (in-memory)", "temp (TEMP objects)"], "the session database, then temp");
  const schemas = await ex.getChildren(dbs[0]);
  assert.deepEqual(labels(schemas), ["main (1)", "staging (1)"]);
  const [orders] = await ex.getChildren(schemas[1]);
  assert.equal(labels([orders])[0], "orders (~100 rows · 2 cols)");
  assert.equal(orders.item.contextValue, "grebe.table");
  assert.deepEqual(labels(await ex.getChildren(orders)), ["id (BIGINT)", "name (VARCHAR)"]);
  const [view] = await ex.getChildren(schemas[0]);
  assert.equal(view.item.contextValue, "grebe.view");
  assert.deepEqual(labels(await ex.getChildren(dbs[1])), ["scratch (~1 row · 1 col · temp)"]);

  // Preview goes to the grid.
  shown.posted.length = 0;
  await ex.show(orders, "Preview", `FROM "memory"."staging"."orders" LIMIT 1000`);
  const res = shown.posted.find((m) => m.type === "result").entry.result;
  assert.equal(res.total, 100);

  // A .duckdb file, browsed read-only (twice is fine).
  await ex.browseFile({ fsPath: other });
  await ex.browseFile({ fsPath: other });
  const roots = await ex.getChildren();
  assert.deepEqual(labels(roots).map((l) => l.replace(/ \(.* in use\)$/, "")), ["Session · :memory:", "warehouse.duckdb (read-only)"]);
  assert.deepEqual(labels(await ex.getChildren(roots[1])), ["dim_customer (~42 rows · 1 col)"]);
  await ex.closeFile(roots[1]);
  assert.equal((await ex.getChildren()).length, 1);

  // --- everything else the session holds.
  for (const q of [
    "CREATE MACRO add1(x) AS x + 1",
    "CREATE MACRO staging.recent(n) AS TABLE SELECT * FROM staging.orders ORDER BY id DESC LIMIT n",
    "CREATE SEQUENCE order_ids",
    "CREATE TYPE mood AS ENUM ('ok', 'bad')",
    "SET VARIABLE cutoff = DATE '2024-01-01'",
    "ATTACH ':memory:' AS scratchpad",
  ]) {
    assert.notEqual((await sess.run(q)).kind, "error", q);
  }
  [root] = await ex.getChildren();
  assert.match(root.item.description, /^[\d.]+ \w+ of [\d.]+ \w+ in use$/, "memory in use on the session line");
  const top = await ex.getChildren(root);
  assert.deepEqual(labels(top).slice(0, 3), ["memory (in-memory)", "scratchpad (attached · in-memory)", "temp (TEMP objects)"]);
  assert.equal(top[1].item.contextValue, "grebe.attachedDb");
  assert.equal(top[0].item.contextValue, "grebe.db", "the session's own database cannot be detached");
  assert.deepEqual(labels(top.slice(3)).map((l) => l.replace(/\d+ loaded/, "N loaded")), ["Variables (1)", "Extensions (N loaded)"]);
  assert.deepEqual(labels(await ex.getChildren(top[3])), ["cutoff (2024-01-01 · DATE)"]);
  const inMain = await ex.getChildren((await ex.getChildren(top[0]))[0]);
  assert.deepEqual(labels(inMain), ["v_orders (1 col)", "Macros (1)", "Sequences (1)", "Types (1)"]);
  assert.deepEqual(labels(await ex.getChildren(inMain[1])), ["add1(x) (macro)"]);
  assert.equal((await ex.getChildren(inMain[1]))[0].item.tooltip, "(x + 1)");
  assert.deepEqual(labels(await ex.getChildren(inMain[3])), ["mood (ENUM)"]);
  const inStaging = await ex.getChildren((await ex.getChildren(top[0]))[1]);
  assert.deepEqual(labels(inStaging), ["orders (~100 rows · 2 cols)", "Macros (1)"]);
  assert.deepEqual(labels(await ex.getChildren(inStaging[1])), ["recent(n) (table macro)"]);

  // --- attach a file to the session, read-only, then detach it.
  vscode.window.showQuickPick = async (items) => items[0];
  await ex.attach({ fsPath: other });
  let dbsNow = labels(await ex.getChildren(root));
  assert.ok(dbsNow.includes("warehouse (attached · warehouse.duckdb · read-only)"), dbsNow.join(" | "));
  const r = await sess.run("SELECT count(*) AS n FROM warehouse.dim_customer");
  assert.deepEqual(r.rows, [[42]], "queries can use it by its alias");
  assert.equal((await sess.run("CREATE TABLE warehouse.x AS SELECT 1")).kind, "error", "read-only means read-only");
  // A second attach of another file with a clashing alias gets its own.
  const other2 = path.join(dir, "sub", "warehouse.duckdb");
  fs.mkdirSync(path.dirname(other2));
  fs.copyFileSync(other, other2);
  await ex.attach({ fsPath: other2 });
  dbsNow = labels(await ex.getChildren(root));
  assert.ok(dbsNow.some((l) => l.startsWith("warehouse_ (attached")), dbsNow.join(" | "));
  const wh = (await ex.getChildren(root)).find((n) => n.item.label === "warehouse");
  await ex.detach(wh);
  dbsNow = labels(await ex.getChildren(root));
  assert.ok(!dbsNow.some((l) => l.startsWith("warehouse (")), dbsNow.join(" | "));
  // A file that is not a database: the error is shown, with the reason.
  const notDb = path.join(dir, "notes.duckdb");
  fs.writeFileSync(notDb, "not a database");
  await ex.attach({ fsPath: notDb });
  assert.match(shown.errors.at(-1), /^grebe: could not attach notes\.duckdb: IO Error: .*not a valid DuckDB database file/);
});

/** Wait until `cond()` holds (a notification is shown without awaiting). */
async function until(cond, what, ms = 10000) {
  const t0 = Date.now();
  while (!cond()) {
    if (Date.now() - t0 > ms) assert.fail(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

live("DuckDB problems are shown when they happen, not at the next run", async (t) => {
  const config = { path: path.join(os.tmpdir(), "no-such-dir", "duckdb"), database: ":memory:" };
  const { vscode, shown } = makeVscode(config);
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../run", "../results", "../log"]) delete require.cache[require.resolve(m)];
  const runner = require("../run");
  const subs = [];
  runner.activate({ subscriptions: subs, extensionUri: { fsPath: path.join(__dirname, "..") } }, () => Promise.resolve(null));
  t.after(() => subs.forEach((s) => s.dispose && s.dispose()));

  // --- a path the user set that does not exist: shown on activation.
  await until(() => shown.errors.length === 1, "the missing-CLI notification");
  assert.match(shown.errors[0], /^grebe: No duckdb CLI at ".*no-such-dir.*duckdb" \(not found\)\. Running SQL needs a working DuckDB CLI\.$/);

  // --- the same problem again is not shown twice.
  shown.configChanged({ affectsConfiguration: (k) => k === "grebe.duckdb.database" });
  await new Promise((r) => setTimeout(r, 300));
  assert.equal(shown.errors.length, 1);

  // --- a database this CLI cannot open: shown when it is chosen.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-e2e-"));
  const bad = path.join(dir, "bad.duckdb");
  fs.writeFileSync(bad, "not a database");
  vscode.workspace.workspaceFolders = [{ uri: vscode.Uri.file(dir) }];
  Object.assign(config, { path: CLI, database: bad });
  shown.configChanged({ affectsConfiguration: (k) => k === "grebe.duckdb.path" });
  await until(() => shown.errors.length === 2, "the bad-database notification");
  assert.match(shown.errors[1], /^grebe: could not start duckdb\. IO Error: .*bad\.duckdb" exists, but it is not a valid DuckDB database file!/);

  // --- a good database starts a session at once; if it dies, that is shown.
  config.database = ":memory:";
  shown.configChanged({ affectsConfiguration: (k) => k === "grebe.duckdb.database" });
  await until(() => runner.liveSession() && !runner.liveSession().starting, "the session to start");
  assert.equal(shown.errors.length, 2);
  runner.liveSession().proc.kill("SIGKILL");
  await until(() => shown.warnings.length === 1, "the session-ended notification");
  assert.match(shown.warnings[0], /^grebe: the DuckDB session ended: duckdb exited \(SIGKILL\)\. The next run starts a new session/);
  assert.equal(runner.liveSession(), null);
  assert.match(shown.log.join(""), /duckdb: .* v\d+\.\d+\.\d+/);
});

live("no DuckDB on PATH and no path set: nobody is nagged until they run", async (t) => {
  const { vscode, shown } = makeVscode({ database: ":memory:" });
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  const prevPath = process.env.PATH;
  process.env.PATH = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-empty-path-"));
  t.after(() => {
    Module._load = origLoad;
    process.env.PATH = prevPath;
  });
  for (const m of ["../run", "../results", "../log"]) delete require.cache[require.resolve(m)];
  const runner = require("../run");
  const subs = [];
  runner.activate({ subscriptions: subs, extensionUri: { fsPath: path.join(__dirname, "..") } }, () => Promise.resolve(null));
  t.after(() => subs.forEach((s) => s.dispose && s.dispose()));
  await until(() => shown.log.some((l) => /No duckdb CLI at "duckdb"/.test(l)), "the log line");
  assert.deepEqual(shown.errors, []);
});

live("results: a page that loads late, or reloads after a move, shows the current run", async (t) => {
  const { vscode, shown } = makeVscode({ path: CLI, database: ":memory:", maxRows: 100 });
  const panels = [];
  vscode.window.createWebviewPanel = () => {
    const posted = [];
    const p = {
      posted,
      webview: fakeWebview(shown, posted),
      reveal() {},
      onDidDispose: (h) => {
        p.close = h;
        return { dispose() {} };
      },
    };
    panels.push(p);
    return p;
  };
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../results", "../log"]) delete require.cache[require.resolve(m)];
  const results = require("../results");
  results.init({ fsPath: path.join(__dirname, "..") });
  results.register({ subscriptions: [] });

  // A run before any page exists: the view has never been opened, so its
  // focus command is what creates it.
  results.begin({ title: "first", detail: "" });
  results.add({ id: 1, uri: "", line: null, preview: "SELECT 1", result: { kind: "ok", ms: 1 } });
  results.end("done");
  assert.deepEqual(shown.executed.map((c) => c[0]), ["grebe.results.focus"]);

  // The view resolves; its page loads and says so; it gets the run.
  const viewPosted = [];
  shown.viewProvider.resolveWebviewView({ webview: fakeWebview(shown, viewPosted), show() {}, onDidDispose: () => ({ dispose() {} }) });
  assert.match(fakeWebview(shown).html + "", /^$/); // (a fresh fake has no page)
  shown.fromPage({ type: "ready" });
  assert.deepEqual(viewPosted.map((m) => m.type), ["begin", "result", "end"]);

  // A second run replaces the first: a page that reloads now (the view was
  // dragged to the side bar) sees only the second.
  results.begin({ title: "second", detail: "" });
  results.end("done again");
  viewPosted.length = 0;
  shown.fromPage({ type: "ready" });
  assert.deepEqual(viewPosted.map((m) => [m.type, m.header?.title ?? m.summary]), [["begin", "second"], ["end", "done again"]]);

  // Open in Editor: a tab with the same content, kept in step with the view.
  results.openInEditor();
  assert.equal(panels.length, 1);
  shown.fromPage({ type: "ready" });
  assert.deepEqual(panels[0].posted.map((m) => m.type), ["begin", "end"]);
  results.begin({ title: "third", detail: "" });
  assert.equal(panels[0].posted.at(-1).header.title, "third");
  assert.equal(viewPosted.at(-1).header.title, "third");
  // Opening it again shows the same tab rather than a second one.
  results.openInEditor();
  assert.equal(panels.length, 1);
  // Closed: later runs still reach the view.
  panels[0].close();
  panels[0].posted.length = 0;
  results.begin({ title: "fourth", detail: "" });
  assert.equal(panels[0].posted.length, 0);
  assert.equal(viewPosted.at(-1).header.title, "fourth");
});

live("data files open in the grid: rows with types, a row cap, a reload on change", async (t) => {
  const config = { path: CLI, database: ":memory:", maxRows: 100 };
  const { vscode, shown } = makeVscode(config);
  const watchers = new Map(); // file name -> its change handler
  Object.assign(vscode, {
    RelativePattern: class {
      constructor(base, pattern) {
        Object.assign(this, { base, pattern });
      }
    },
  });
  vscode.workspace.createFileSystemWatcher = (pattern) => {
    return {
      onDidChange: (h) => watchers.set(pattern.pattern, h),
      onDidCreate: () => {},
      dispose() {},
    };
  };
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../dataEditor", "../inspect", "../results", "../log"]) delete require.cache[require.resolve(m)];
  require("../results").init({ fsPath: path.join(__dirname, "..") });
  const dataEditor = require("../dataEditor");
  t.after(() => require("../inspect").dispose());

  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-grid-"));
  const { Session } = require("../duckdb-session");
  const w = new Session({ cli: CLI, database: ":memory:", cwd: dir });
  await w.start();
  await w.run("COPY (SELECT range AS id, 'c' || range AS name, range * 1.5 AS amt FROM range(250)) TO 'orders.parquet'");
  await w.run("COPY (SELECT range AS id, 'p' || range AS who FROM range(7)) TO 'people.csv'");
  await w.run("COPY (SELECT 1 AS id WHERE false) TO 'empty.parquet'");

  const open = async (file) => {
    const posted = [];
    let closed = null;
    const panel = { webview: fakeWebview(shown, posted), onDidDispose: (h) => (closed = h) };
    const uri = vscode.Uri.file(path.join(dir, file));
    await dataEditor.provider.resolveCustomEditor(dataEditor.provider.openCustomDocument(uri), panel);
    return { posted, close: () => closed && closed(), result: () => posted.filter((m) => m.type === "result").at(-1).entry.result, end: () => posted.filter((m) => m.type === "end").at(-1).summary };
  };

  // Parquet: the first maxRows rows, more marked, types in the headers.
  const orders = await open("orders.parquet");
  let r = orders.result();
  assert.equal(r.kind, "rows");
  assert.deepEqual(r.columns, ["id", "name", "amt"]);
  assert.deepEqual(r.types, ["BIGINT", "VARCHAR", "DECIMAL(21,1)"]);
  assert.equal(r.data[0].length, 100);
  assert.equal(r.more, true);
  assert.equal(orders.end(), path.join(dir, "orders.parquet"), "the header says where; the status line, how many");

  // CSV: every row, no cap reached.
  const people = await open("people.csv");
  r = people.result();
  assert.deepEqual([r.columns, r.data[0].length, r.more], [["id", "who"], 7, undefined]);
  assert.deepEqual(r.types, ["BIGINT", "VARCHAR"]);

  // An empty file still shows its columns.
  r = (await open("empty.parquet")).result();
  assert.deepEqual([r.columns, r.types, r.data[0].length], [["id"], ["INTEGER"], 0]);

  // A file that is not what its name says: the error, in the grid's place.
  fs.writeFileSync(path.join(dir, "broken.parquet"), "not parquet at all");
  r = (await open("broken.parquet")).result();
  assert.equal(r.kind, "error");

  // Rewritten on disk: read again.
  await w.run("COPY (SELECT range AS id, 'p' || range AS who FROM range(3)) TO 'people.csv'");
  w.dispose();
  people.posted.length = 0;
  watchers.get("people.csv")();
  await until(() => people.posted.some((m) => m.type === "end"), "the reload");
  assert.equal(people.result().data[0].length, 3);
  people.close();

  // Parquet opens with the default grid editor, delimited files with the
  // one offered through Open With.
  assert.equal(dataEditor.viewTypeFor(vscode.Uri.file("/x/a.parquet")), "grebe.dataFile");
  assert.equal(dataEditor.viewTypeFor(vscode.Uri.file("/x/a.csv")), "grebe.delimitedFile");
  assert.deepEqual(shown.errors, []);
});

test("a command that throws is logged with its stack and shown, not lost", async (t) => {
  const { vscode, shown } = makeVscode({});
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  delete require.cache[require.resolve("../log")];
  const log = require("../log");
  log.command("grebe.catalog.preview", () => {
    throw new TypeError("cannot read properties of undefined (reading 'object')");
  });
  assert.equal(await shown.commands.get("grebe.catalog.preview")(), undefined);
  assert.deepEqual(shown.errors, ["grebe: catalog.preview failed: cannot read properties of undefined (reading 'object')"]);
  const logged = shown.log.join("");
  assert.match(logged, /\[extension\] error: grebe\.catalog\.preview failed: cannot read/);
  assert.match(logged, /\[extension\] TypeError: .*\n.*\[extension\]\s+at /, "the stack is in the log");
});

test("errorContext: the line DuckDB points into, and the column, as the editor counts", () => {
  const { errorContext, caretLines } = (() => {
    const { vscode } = makeVscode({});
    const origLoad = Module._load;
    Module._load = function (request, ...rest) {
      return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
    };
    try {
      for (const m of ["../run", "../results", "../log"]) delete require.cache[require.resolve(m)];
      return require("../run");
    } finally {
      Module._load = origLoad;
    }
  })();
  const sql = "SELECT 'é' AS a,\n       nope\nFROM t";
  // DuckDB's position is a byte offset; `é` is two bytes, one UTF-16 unit.
  const at = Buffer.byteLength("SELECT 'é' AS a,\n       ");
  const ctx = errorContext(sql, at, 10);
  assert.deepEqual(ctx, { line: 11, text: "       nope", column: 7 });
  assert.equal(caretLines(ctx), "12 |        nope\n   |        ^");
  assert.equal(errorContext(sql, null, 0), null);
});

live("cancel: a long statement stops, the rest of the run does not run, the session lives on", { skip: process.platform === "win32" }, async (t) => {
  const config = { path: CLI, database: ":memory:", maxRows: 100, queryTimeout: 0 };
  const { vscode, shown } = makeVscode(config);
  const contexts = [];
  vscode.commands.executeCommand = async (name, ...args) => {
    if (name === "setContext") contexts.push(args);
  };
  let warningPick;
  vscode.window.showWarningMessage = async (m, ...actions) => {
    shown.warnings.push(m);
    return actions.includes(warningPick) ? warningPick : undefined;
  };
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../run", "../results", "../log", "../duckdb-session"]) delete require.cache[require.resolve(m)];
  const runner = require("../run");
  const results = require("../results");
  const client = lspClient(GREBE);
  t.after(() => client.stop());
  await client.request("initialize", { capabilities: {} });
  const subs = [];
  runner.activate({ subscriptions: subs, extensionUri: { fsPath: path.join(__dirname, "..") } }, () => Promise.resolve(client));
  t.after(() => subs.forEach((s) => s.dispose && s.dispose()));
  t.after(() => require("../duckdb-session").Session.disposeAll());
  resolveResultsView(vscode, shown);
  results.setCancelHandler(() => runner.cancel());

  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "grebe-cancel-"));
  const open = (name, sql) => {
    const doc = fakeDocument(path.join(dir, name), sql);
    shown.docs.set(doc.uri.toString(), doc);
    client.notify("textDocument/didOpen", { textDocument: { uri: doc.uri.toString(), languageId: "sql", version: 1, text: sql } });
    vscode.window.activeTextEditor = { document: doc, selection: new Selection(new Position(0, 0), new Position(0, 0)) };
    return doc;
  };
  const endless = "SELECT count(*) FROM range(3000000) a, range(3000000) b WHERE (a.range * b.range) % 1000003 = 7;";
  open("long.sql", ["CREATE TEMP TABLE keep AS SELECT 42 AS v;", endless, "CREATE TABLE never AS SELECT 1;"].join("\n"));

  // --- cancel from the command while statement 2 runs.
  const running = shown.commands.get("grebe.duckdb.runFile")();
  await until(() => shown.posted.some((m) => m.type === "running" && /2 of 3/.test(m.label)), "statement 2 to start");
  assert.equal(runner.isRunning(), true);
  assert.deepEqual(contexts.at(-1), ["grebe.duckdb.running", true], "the Stop button replaces Run");

  // A second run meanwhile is not queued behind it; it offers to cancel.
  await shown.commands.get("grebe.duckdb.runStatement")();
  assert.match(shown.warnings.at(-1), /long\.sql is still running/);

  await new Promise((r) => setTimeout(r, 300));
  const t0 = Date.now();
  await shown.commands.get("grebe.duckdb.cancel")();
  await running;
  assert.ok(Date.now() - t0 < 2000, `stopped in ${Date.now() - t0} ms`);
  const outcomes = shown.posted.filter((m) => m.type === "result").map((m) => m.entry.result.kind);
  assert.deepEqual(outcomes, ["ok", "cancelled"], "the third statement never ran");
  assert.equal(shown.posted.findLast((m) => m.type === "end").summary, "Cancelled: 1 succeeded, 1 not run.");
  assert.deepEqual(contexts.at(-1), ["grebe.duckdb.running", false]);
  assert.equal(runner.isRunning(), false);
  assert.match(shown.log.join(""), /\[duckdb\] cancelling long\.sql after/);
  assert.match(shown.log.join(""), /\[duckdb\] long\.sql:2 cancelled/);

  // Same session: the TEMP table is still there; the cancelled run's last
  // statement never created its table.
  shown.posted.length = 0;
  open("after.sql", "SELECT v FROM keep;\nSELECT count(*) AS n FROM duckdb_tables() WHERE table_name = 'never';");
  await shown.commands.get("grebe.duckdb.runFile")();
  const after = shown.posted.filter((m) => m.type === "result").map((m) => m.entry.result);
  assert.deepEqual(after[0].data, [["42"]]);
  assert.deepEqual(after[1].data, [["0"]]);

  // --- cancel from the grid's Cancel button.
  shown.posted.length = 0;
  open("again.sql", endless);
  const again = shown.commands.get("grebe.duckdb.runFile")();
  await until(() => shown.posted.some((m) => m.type === "running"), "the run to start");
  await new Promise((r) => setTimeout(r, 300));
  await shown.fromPage({ type: "cancel" });
  await again;
  assert.equal(shown.posted.findLast((m) => m.type === "result").entry.result.kind, "cancelled");

  // --- a timeout does it unasked.
  config.queryTimeout = 1;
  shown.posted.length = 0;
  open("slow.sql", endless);
  await shown.commands.get("grebe.duckdb.runFile")();
  const timed = shown.posted.findLast((m) => m.type === "result").entry.result;
  assert.deepEqual([timed.kind, timed.reason], ["cancelled", "timeout"]);
  assert.ok(timed.ms >= 1000 && timed.ms < 3000, `${timed.ms} ms`);
  assert.equal(shown.posted.findLast((m) => m.type === "end").summary, "Timed out: 0 succeeded.");
  assert.deepEqual(shown.errors, []);
});

test("results: Cancel goes to the channel's own handler, else to the shared one", () => {
  const { vscode, shown } = makeVscode({});
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  try {
    for (const m of ["../results", "../log"]) delete require.cache[require.resolve(m)];
    const results = require("../results");
    results.init({ fsPath: path.join(__dirname, "..") });
    let shared = 0;
    let own = 0;
    results.setCancelHandler(() => shared++);
    const view = new results.Channel();
    view.attach(fakeWebview(shown, []));
    shown.fromPage({ type: "cancel" });
    const file = new results.Channel();
    file.onCancel = () => own++;
    file.attach(fakeWebview(shown, []));
    shown.fromPage({ type: "cancel" });
    assert.deepEqual([shared, own], [1, 1]);
  } finally {
    Module._load = origLoad;
  }
});

live("Restricted Mode: nothing starts DuckDB, and each place says why", async (t) => {
  const { vscode, shown } = makeVscode({ path: CLI, database: ":memory:", maxRows: 100 });
  vscode.workspace.isTrusted = false;
  class TreeItem {
    constructor(label, state) {
      this.label = label;
      this.collapsibleState = state;
    }
  }
  Object.assign(vscode, {
    TreeItem,
    ThemeIcon: class {
      constructor(id) {
        this.id = id;
      }
    },
    TreeItemCollapsibleState: { None: 0, Collapsed: 1, Expanded: 2 },
    RelativePattern: class {},
  });
  vscode.workspace.createFileSystemWatcher = () => ({ onDidChange() {}, onDidCreate() {}, dispose() {} });
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../run", "../results", "../log", "../trust", "../catalog", "../inspect", "../dataEditor", "../duckdb-session"]) {
    delete require.cache[require.resolve(m)];
  }
  const { Session } = require("../duckdb-session");
  const runner = require("../run");
  const subs = [];
  runner.activate({ subscriptions: subs, extensionUri: { fsPath: path.join(__dirname, "..") } }, () => Promise.resolve(null));
  t.after(() => subs.forEach((s) => s.dispose && s.dispose()));
  resolveResultsView(vscode, shown);

  // Run: a warning that names Restricted Mode, and no session.
  const doc = fakeDocument(path.join(os.tmpdir(), "r.sql"), "SELECT 1;");
  vscode.window.activeTextEditor = { document: doc, selection: new Selection(new Position(0, 0), new Position(0, 0)) };
  await shown.commands.get("grebe.duckdb.runFile")();
  assert.match(shown.warnings.at(-1), /Running SQL needs a trusted folder\. This folder is open in Restricted Mode; linting and formatting still work\./);
  assert.equal(runner.liveSession(), null);

  // Catalog: says so in the tree, and its start command does not start one.
  const { Explorer } = require("../catalog");
  const ex = new Explorer({ currentSession: runner.currentSession, liveSession: runner.liveSession, settings: runner.settings, cli: () => CLI });
  t.after(() => ex.dispose());
  const [root] = await ex.getChildren();
  const [msg] = await ex.getChildren(root);
  assert.equal(msg.item.label, "Restricted Mode: trust this folder to use DuckDB");
  assert.equal(msg.item.command.command, "workbench.trust.manage");
  await assert.rejects(runner.currentSession(), /Restricted Mode/);

  // A data file opened in the grid: the reason, in the tab.
  const dataEditor = require("../dataEditor");
  const posted = [];
  await dataEditor.provider.resolveCustomEditor(
    dataEditor.provider.openCustomDocument(vscode.Uri.file(path.join(os.tmpdir(), "x.parquet"))),
    { webview: fakeWebview(shown, posted), onDidDispose() {} },
  );
  assert.match(posted.find((m) => m.type === "end").summary, /Restricted Mode/);
  assert.equal(posted.some((m) => m.type === "result"), false);

  // Hover: nothing, quietly.
  const inspect = require("../inspect");
  const sql = fakeDocument(path.join(os.tmpdir(), "h.sql"), "FROM 'x.parquet'");
  assert.equal(await inspect.hoverProvider.provideHover(sql, new Position(0, 8)), null);

  // Not one duckdb process was started.
  assert.equal(Session.cancelAll(), false);
  assert.equal(shown.errors.length, 0, shown.errors.join("\n"));
});

live("file commands given a tree item (not a file) ask for a file instead of failing", async (t) => {
  const { vscode, shown } = makeVscode({ path: CLI, database: ":memory:", maxRows: 100 });
  class TreeItem {
    constructor(label, state) {
      this.label = label;
      this.collapsibleState = state;
    }
  }
  Object.assign(vscode, {
    TreeItem,
    ThemeIcon: class {
      constructor(id) {
        this.id = id;
      }
    },
    TreeItemCollapsibleState: { None: 0, Collapsed: 1, Expanded: 2 },
  });
  const dialogs = [];
  vscode.window.showOpenDialog = async (opts) => {
    dialogs.push(opts.openLabel || "open");
    return undefined; // the user cancels
  };
  vscode.window.createTreeView = () => ({ dispose() {} });
  vscode.window.registerCustomEditorProvider = () => ({ dispose() {} });
  const origLoad = Module._load;
  Module._load = function (request, ...rest) {
    return request === "vscode" ? vscode : origLoad.call(this, request, ...rest);
  };
  t.after(() => {
    Module._load = origLoad;
  });
  for (const m of ["../catalog", "../dataEditor", "../inspect", "../results", "../log", "../trust"]) delete require.cache[require.resolve(m)];
  require("../results").init({ fsPath: path.join(__dirname, "..") });
  const subs = [];
  require("../catalog").activate({ subscriptions: subs }, {
    currentSession: async () => null,
    liveSession: () => null,
    onDidRun: () => ({ dispose() {} }),
    settings: () => ({ cli: CLI, database: ":memory:" }),
    cli: () => CLI,
  });
  require("../dataEditor").activate({ subscriptions: subs });
  t.after(() => subs.forEach((s) => s.dispose && s.dispose()));

  // What VS Code hands a view's title-bar command: the selected tree item.
  const treeItem = { kind: "group", item: new TreeItem("Extensions", 1), children: [] };
  for (const name of ["grebe.catalog.browseFile", "grebe.catalog.attach", "grebe.openInGrid"]) {
    await shown.commands.get(name)(treeItem);
  }
  await shown.commands.get("grebe.catalog.useAsDatabase")(treeItem);
  assert.deepEqual(shown.errors, [], "no command failed");
  assert.equal(dialogs.length, 3, "browse, attach and open-in-grid each asked for a file");
});
