// friendly.js against a stand-in for the `vscode` API. With GREBE_BIN set
// the edits come from the real `grebe lsp`; the other tests use a client
// that answers from a script.
const assert = require("node:assert/strict");
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const Module = require("node:module");
const path = require("node:path");
const { test, beforeEach } = require("node:test");

const GREBE = process.env.GREBE_BIN;

// --- the stand-in -----------------------------------------------------------
class Position {
  constructor(line, character) {
    this.line = line;
    this.character = character;
  }
}
class Range {
  constructor(start, end) {
    this.start = start;
    this.end = end;
  }
}
let messages; // what was shown to the user
let registered; // command name -> handler
let activeEditor;
const vscode = {
  Position,
  Range,
  window: {
    get activeTextEditor() {
      return activeEditor;
    },
    showInformationMessage: (m) => { messages.push(m); return Promise.resolve(undefined); },
    showErrorMessage: (m) => { messages.push(m); return Promise.resolve(undefined); },
    createOutputChannel: () => ({ appendLine() {}, show() {}, dispose() {} }),
  },
  commands: {
    registerCommand: (name, fn) => { registered.set(name, fn); return { dispose() {} }; },
  },
};
const realLoad = Module._load;
Module._load = function (request, ...rest) {
  return request === "vscode" ? vscode : realLoad.call(this, request, ...rest);
};
const friendly = require("../friendly");

beforeEach(() => {
  messages = [];
  registered = new Map();
  activeEditor = undefined;
});

/** An editor over `text`. `select` is [startLine, startChar, endLine, endChar]. */
function editorOf(text, select, uri = "file:///work/q.sql") {
  const offsetAt = (lines, p) => lines.slice(0, p.line).reduce((n, l) => n + l.length + 1, 0) + p.character;
  const doc = {
    languageId: "sql",
    uri: { scheme: uri.split(":")[0], toString: () => uri },
    version: 1,
    text,
  };
  const [sl, sc, el, ec] = select ?? [0, 0, 0, 0];
  return {
    document: doc,
    selection: { start: new Position(sl, sc), end: new Position(el, ec), isEmpty: sl === el && sc === ec },
    edits: 0,
    // What TextEditor.edit does: every replacement is a position in the text
    // as it was, applied together.
    edit(build) {
      const pending = [];
      build({ replace: (range, newText) => pending.push({ range, newText }) });
      const lines = doc.text.split("\n");
      pending.sort((a, b) => offsetAt(lines, b.range.start) - offsetAt(lines, a.range.start));
      for (const e of pending) {
        const from = offsetAt(lines, e.range.start);
        const to = offsetAt(lines, e.range.end);
        doc.text = doc.text.slice(0, from) + e.newText + doc.text.slice(to);
      }
      doc.version++;
      this.edits++;
      return Promise.resolve(true);
    },
  };
}

/** A client that records what it was asked and answers `answer`. */
function scripted(answer) {
  const asked = [];
  return {
    asked,
    sendRequest(method, params) {
      asked.push({ method, params });
      return Promise.resolve(typeof answer === "function" ? answer() : answer);
    },
  };
}

/** The words of `sql`: what it says, whatever lines it is laid out on. */
const words = (sql) => sql.split(/\s+/).filter(Boolean);

// --- a real `grebe lsp` -------------------------------------------------------
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
    sendRequest(method, params) {
      const myId = ++id;
      return new Promise((resolve) => {
        pending.set(myId, resolve);
        send({ id: myId, method, params });
      });
    },
    sendNotification: (method, params) => send({ method, params }),
    stop: () => proc.kill(),
  };
}

async function serverWith(t, editor) {
  const client = lspClient(GREBE);
  t.after(() => client.stop());
  await client.sendRequest("initialize", { processId: null, rootUri: null, capabilities: {} });
  client.sendNotification("initialized", {});
  client.sendNotification("textDocument/didOpen", {
    textDocument: { uri: editor.document.uri.toString(), languageId: "sql", version: 1, text: editor.document.text },
  });
  return client;
}

const CONVENTIONAL = [
  "select region, count(*) from sales where year = 2024 group by region;",
  "select * from sales;",
  "from sales select region;",
  "",
].join("\n");

// --- with the real server -------------------------------------------------
test("no selection: the whole file is rewritten and formatted", { skip: !GREBE }, async (t) => {
  const editor = editorOf(CONVENTIONAL);
  const client = await serverWith(t, editor);
  assert.equal(await friendly.rewrite(editor, client), 1);
  assert.deepEqual(
    words(editor.document.text),
    words("FROM sales SELECT region, count() WHERE year = 2024 GROUP BY ALL; FROM sales; FROM sales SELECT region;"),
  );
  assert.equal(editor.edits, 1, "one undo step");
  assert.deepEqual(messages, []);
});

test("a selection: only the statements it touches are rewritten", { skip: !GREBE }, async (t) => {
  // From inside the second statement to inside the third.
  const editor = editorOf(CONVENTIONAL, [1, 4, 2, 3]);
  const client = await serverWith(t, editor);
  assert.equal(await friendly.rewrite(editor, client), 1);
  const lines = editor.document.text.split("\n");
  assert.equal(lines[0], CONVENTIONAL.split("\n")[0], "the first statement is as it was");
  assert.deepEqual(words(lines.slice(1).join(" ")), words("FROM sales; from sales select region;"));
});

test("friendly SQL already: nothing changes, and a message says so", { skip: !GREBE }, async (t) => {
  const text = "from   sales select region;\n";
  const editor = editorOf(text);
  const client = await serverWith(t, editor);
  assert.equal(await friendly.rewrite(editor, client), 0);
  assert.equal(editor.document.text, text);
  assert.equal(editor.edits, 0);
  assert.match(messages[0], /nothing to rewrite in this file/);
});

// --- without one ------------------------------------------------------------
test("the request carries the selection, or no range without one", async () => {
  const edit = { range: { start: { line: 0, character: 0 }, end: { line: 0, character: 16 } }, newText: "FROM t SELECT a;" };
  let client = scripted([edit]);
  let editor = editorOf("SELECT a FROM t;\n");
  assert.equal(await friendly.rewrite(editor, client), 1);
  assert.deepEqual(client.asked, [{ method: "grebe/friendly", params: { textDocument: { uri: "file:///work/q.sql" } } }]);
  assert.equal(editor.document.text, "FROM t SELECT a;\n");

  client = scripted([edit]);
  editor = editorOf("SELECT a FROM t;\n", [0, 2, 0, 9]);
  await friendly.rewrite(editor, client);
  assert.deepEqual(client.asked[0].params.range, {
    start: { line: 0, character: 2 },
    end: { line: 0, character: 9 },
  });
});

test("edits for text that changed meanwhile are not applied", async () => {
  const editor = editorOf("SELECT a FROM t;\n");
  const client = scripted(() => {
    editor.document.version++; // typed while the server was working
    return [{ range: { start: { line: 0, character: 0 }, end: { line: 0, character: 16 } }, newText: "FROM t SELECT a;" }];
  });
  assert.equal(await friendly.rewrite(editor, client), 0);
  assert.equal(editor.document.text, "SELECT a FROM t;\n");
  assert.match(messages[0], /changed while it was being rewritten/);
});

test("a file the server does not have, or no server: a message, no request", async () => {
  const client = scripted([]);
  assert.equal(await friendly.rewrite(editorOf("SELECT a FROM t;\n", null, "untitled:Untitled-1"), client), 0);
  assert.equal(client.asked.length, 0);
  assert.match(messages[0], /saved to disk/);

  assert.equal(await friendly.rewrite(editorOf("SELECT a FROM t;\n"), null), 0);
  assert.match(messages[1], /language server is not running/);
});

test("the command is registered and runs on the active editor", async () => {
  const client = scripted([]);
  friendly.activate({ subscriptions: [] }, () => Promise.resolve(client));
  const run = registered.get("grebe.friendly");
  assert.ok(run, "grebe.friendly is registered");
  assert.equal(await run(), 0, "no editor: nothing to do");
  assert.equal(client.asked.length, 0);
  activeEditor = editorOf("SELECT a FROM t;\n");
  await run();
  assert.equal(client.asked[0].method, "grebe/friendly");
});

test("package.json offers the command in the palette and the editor's context menu", () => {
  const pkg = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "package.json"), "utf8"));
  const command = pkg.contributes.commands.find((c) => c.command === "grebe.friendly");
  assert.equal(command.title, "Rewrite as Friendly SQL");
  assert.equal(command.category, "grebe");
  const entry = pkg.contributes.menus["editor/context"].find((m) => m.command === "grebe.friendly");
  assert.match(entry.when, /editorLangId == sql/);
});
