// The results page in a real browser: media/results-page.js loaded into
// headless Chromium, fed the messages the extension sends, and driven with
// the mouse and keyboard. Needs Playwright (`NODE_PATH=$(npm root -g)` with
// a global install); skipped without it.

"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const path = require("path");

let chromium = null;
try {
  ({ chromium } = require("playwright"));
} catch {
  // not installed: the tests below are skipped
}
const browserTest = chromium ? test : test.skip;

const PAGE = fs.readFileSync(path.join(__dirname, "..", "media", "results-page.js"), "utf8");

// The shell results.js serves, minus the CSP nonce, with the VS Code API
// replaced by a recorder.
function html() {
  return `<!DOCTYPE html><html><head><style>
    body { font-family: sans-serif; font-size: 12px; margin: 0; padding: 0 10px; background: #1e1e1e; color: #ccc;
      --vscode-editor-font-family: monospace; --vscode-editor-font-size: 12px; }
    .grid { outline: none; border: 1px solid #444; }
    .viewport { overflow: auto; position: relative; }
    .viewport canvas { position: sticky; top: 0; left: 0; display: block; }
    .sizer { pointer-events: none; }
  </style></head><body>
  <header><div id="title">No results yet.</div><div id="summary"></div></header>
  <main id="runs"></main>
  <script>
    window.sent = [];
    window.acquireVsCodeApi = () => ({ postMessage: (m) => window.sent.push(m) });
  </script>
  <script>${PAGE}</script></body></html>`;
}

function rowsResult(n) {
  const ids = [];
  const names = [];
  for (let i = 0; i < n; i++) {
    ids.push(String((i * 7) % n));
    names.push(i % 5 === 0 ? null : `name ${i}`);
  }
  return {
    kind: "rows",
    ms: 3.2,
    columns: ["id", "name"],
    types: ["BIGINT", "VARCHAR"],
    data: [ids, names],
    num: [true, false],
    total: n,
  };
}

browserTest("the grid draws, selects, copies and sorts", async (t) => {
  const browser = await chromium.launch();
  t.after(() => browser.close());
  const page = await browser.newPage({ viewport: { width: 900, height: 700 } });
  await page.setContent(html());

  // The page says it is ready as soon as it loads, so the host can replay.
  assert.deepEqual(await page.evaluate(() => window.sent), [{ type: "ready" }]);

  await page.evaluate((result) => {
    window.postMessage({ type: "begin", header: { title: "q.sql — 1 statement", detail: "running…" } }, "*");
    window.postMessage({ type: "result", entry: { id: 1, uri: "file:///q.sql", line: 0, preview: "SELECT id, name FROM t", result, exportable: true } }, "*");
    window.postMessage({ type: "end", summary: "1 statement succeeded." }, "*");
  }, rowsResult(100000));
  await page.waitForSelector("canvas");
  await page.waitForTimeout(100);

  assert.equal(await page.textContent("#title"), "q.sql — 1 statement");
  assert.equal(await page.textContent("#summary"), "1 statement succeeded.");
  assert.match(await page.textContent(".bar"), /100,000 rows × 2/);

  // Something was drawn: the canvas is not one flat colour.
  const colours = await page.evaluate(() => {
    const c = document.querySelector("canvas");
    const d = c.getContext("2d").getImageData(0, 0, c.width, c.height).data;
    const seen = new Set();
    for (let i = 0; i < d.length; i += 4 * 97) seen.add(`${d[i]},${d[i + 1]},${d[i + 2]}`);
    return seen.size;
  });
  assert.ok(colours > 3, `${colours} colours drawn`);

  // Scrolling 100,000 rows: the viewport scrolls, nothing is rebuilt.
  const box = await page.locator("canvas").boundingBox();
  const cellsBefore = await page.evaluate(() => document.querySelectorAll("*").length);
  await page.mouse.move(box.x + 200, box.y + 200);
  await page.mouse.wheel(0, 50000);
  await page.waitForTimeout(50);
  assert.ok((await page.evaluate(() => document.querySelector(".viewport").scrollTop)) > 0);
  assert.equal(await page.evaluate(() => document.querySelectorAll("*").length), cellsBefore, "no per-row DOM");
  await page.evaluate(() => (document.querySelector(".viewport").scrollTop = 0));
  await page.waitForTimeout(50);

  // Click the first data cell of `name`, extend two rows down, copy.
  const HEAD = 22;
  const ROW = 20;
  const cell = (row, x) => [box.x + x, box.y + HEAD + row * ROW + ROW / 2];
  // `name` starts after the row numbers and `id`: read where from a header
  // click's tooltip rather than assuming a width.
  let nameX = null;
  for (let x = 40; x < 600; x += 10) {
    await page.mouse.move(box.x + x, box.y + HEAD / 2);
    const title = await page.evaluate(() => document.querySelector("canvas").title);
    if (title.startsWith("name")) {
      nameX = x;
      break;
    }
  }
  assert.ok(nameX, "found the name column");
  await page.mouse.click(...cell(1, nameX));
  await page.keyboard.down("Shift");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.up("Shift");
  await page.keyboard.press("Control+C");
  let copies = await page.evaluate(() => window.sent.filter((m) => m.type === "copy"));
  assert.deepEqual(copies.at(-1).text.split("\n"), ["name 1", "name 2", "name 3"]);

  // Shift+click extends from the anchor to the clicked cell.
  await page.mouse.click(...cell(1, nameX));
  await page.keyboard.down("Shift");
  await page.mouse.click(...cell(2, nameX));
  await page.keyboard.up("Shift");
  await page.keyboard.press("Control+C");
  copies = await page.evaluate(() => window.sent.filter((m) => m.type === "copy"));
  assert.deepEqual(copies.at(-1).text.split("\n"), ["name 1", "name 2"]);

  // A whole column copies with its header; NULL copies as an empty cell.
  await page.mouse.click(box.x + nameX, box.y + HEAD / 2);
  await page.keyboard.press("Control+C");
  copies = await page.evaluate(() => window.sent.filter((m) => m.type === "copy"));
  const lines = copies.at(-1).text.split("\n");
  assert.equal(lines.length, 100001);
  assert.deepEqual(lines.slice(0, 3), ["name", "", "name 1"]);

  // Sort by id ascending with the header's sort toggle: the first row
  // becomes 0, then 0's neighbour in sorted order.
  let idSort = null;
  for (let x = 20; x < nameX; x += 2) {
    await page.mouse.move(box.x + x, box.y + HEAD / 2);
    if ((await page.evaluate(() => document.querySelector("canvas").title)).startsWith("Sort")) {
      idSort = x;
      break;
    }
  }
  assert.ok(idSort, "found the sort toggle on id");
  await page.mouse.click(box.x + idSort, box.y + HEAD / 2);
  await page.mouse.click(...cell(0, idSort - 20));
  await page.keyboard.down("Shift");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.up("Shift");
  await page.keyboard.press("Control+C");
  copies = await page.evaluate(() => window.sent.filter((m) => m.type === "copy"));
  assert.deepEqual(copies.at(-1).text.split("\n"), ["0", "1"]);

  // Export buttons post which format was asked for.
  await page.click(".export button >> text=parquet");
  const exp = await page.evaluate(() => window.sent.filter((m) => m.type === "export"));
  assert.deepEqual(exp, [{ type: "export", id: 1, format: "parquet" }]);
});

browserTest("errors, text, OK and a capped data file render as such", async (t) => {
  const browser = await chromium.launch();
  t.after(() => browser.close());
  const page = await browser.newPage();
  await page.setContent(html());
  await page.evaluate((capped) => {
    window.postMessage({ type: "begin", header: { title: "t", detail: "" } }, "*");
    window.postMessage({ type: "result", entry: { id: 1, line: 0, uri: "u", preview: "CREATE TABLE t (a INT)", result: { kind: "ok", ms: 1.5 } } }, "*");
    window.postMessage({ type: "result", entry: { id: 2, line: 1, uri: "u", preview: "EXPLAIN SELECT 1", result: { kind: "text", text: "PROJECTION", ms: 2 } } }, "*");
    window.postMessage({ type: "result", entry: { id: 3, line: 2, uri: "u", preview: "SELECT nope", result: { kind: "error", type: "Binder", message: 'Referenced column "nope" not found', position: 7, context: { line: 2, text: "SELECT nope", column: 7 } } } }, "*");
    window.postMessage({ type: "result", entry: { id: 4, line: null, uri: "u", preview: "FROM read_parquet('x')", result: capped } }, "*");
  }, { ...rowsResult(100), more: true });
  await page.waitForSelector("canvas");
  assert.match(await page.textContent("main"), /OK · 1\.5 ms/);
  assert.equal(await page.textContent("pre.text"), "PROJECTION");
  assert.equal(await page.textContent(".error"), 'Binder Error: Referenced column "nope" not found');
  assert.equal(await page.textContent("pre.context"), "3 | SELECT nope\n  |        ^");
  assert.match(await page.textContent(".bar"), /^first 100 rows × 2/);
  // A line link goes back to the statement; a data file has none.
  await page.click(".stmt a >> text=L3");
  const reveal = await page.evaluate(() => window.sent.filter((m) => m.type === "reveal"));
  assert.deepEqual(reveal, [{ type: "reveal", uri: "u", line: 2 }]);
  assert.equal(await page.locator("section").nth(3).locator(".stmt a").count(), 0);
});

browserTest("a running statement shows its clock and a Cancel button", async (t) => {
  const browser = await chromium.launch();
  t.after(() => browser.close());
  const page = await browser.newPage();
  await page.setContent(html());
  await page.evaluate(() => {
    window.postMessage({ type: "begin", header: { title: "load.sql — 3 statements", detail: "" } }, "*");
    window.postMessage({ type: "running", label: "Running statement 2 of 3", since: Date.now() - 65000 }, "*");
  });
  await page.waitForSelector(".running");
  assert.match(await page.textContent(".running"), /^Running statement 2 of 3 · 1 min 5 s/);
  await page.waitForTimeout(1100);
  assert.match(await page.textContent(".running"), /1 min [67] s/, "the clock ticks");

  await page.click(".running button");
  assert.deepEqual(await page.evaluate(() => window.sent.filter((m) => m.type === "cancel")), [{ type: "cancel" }]);
  assert.equal(await page.textContent(".running button"), "Cancelling…");
  assert.equal(await page.isDisabled(".running button"), true, "one click, one cancel");

  // The result arrives: the clock goes, the result says what happened.
  await page.evaluate(() => {
    window.postMessage({ type: "result", entry: { id: 1, line: 1, uri: "u", preview: "SELECT ...", result: { kind: "cancelled", reason: "cancel", ms: 65400 } } }, "*");
    window.postMessage({ type: "result", entry: { id: 2, line: 2, uri: "u", preview: "SELECT ...", result: { kind: "cancelled", reason: "timeout", ms: 30000 } } }, "*");
    window.postMessage({ type: "result", entry: { id: 3, line: 3, uri: "u", preview: "SELECT ...", result: { kind: "cancelled", reason: "cancel", ended: true, ms: 3100, message: "DuckDB did not stop within 3 s, so the session was ended: TEMP tables and in-memory data are gone." } } }, "*");
    window.postMessage({ type: "end", summary: "Cancelled: 1 succeeded, 1 not run." }, "*");
  });
  await page.waitForSelector(".cancelled");
  await page.waitForTimeout(50);
  assert.equal(await page.locator(".running").count(), 0);
  const notes = await page.locator(".cancelled").allTextContents();
  assert.deepEqual(notes, [
    "Cancelled after 1 min 5 s.",
    "Timed out (grebe.duckdb.queryTimeout) after 30.0 s.",
    "Cancelled after 3,100 ms. DuckDB did not stop within 3 s, so the session was ended: TEMP tables and in-memory data are gone.",
  ]);
});
