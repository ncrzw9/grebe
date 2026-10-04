// One place for everything the extension has to say: the "grebe" output
// channel, and notifications that lead back to it.
//
// Every line is timestamped and tagged with where it came from (server,
// duckdb, catalog, grid), so one channel can hold the language server, the
// DuckDB session and the views without their lines being confused. Commands
// are registered through `command()`, which catches whatever a handler
// throws: an unexpected error is logged with its stack and shown, never
// left in the extension host's own log where nobody looks.

"use strict";

const vscode = require("vscode");

let channel = null;

/** The channel, created on first use. */
function output() {
  if (!channel) channel = vscode.window.createOutputChannel("grebe");
  return channel;
}

function stamp() {
  return new Date().toLocaleTimeString();
}

/** One line, or several: each gets the time and the tag. */
function line(tag, text) {
  const out = output();
  for (const l of String(text).replace(/\s+$/, "").split("\n")) out.appendLine(`[${stamp()}] [${tag}] ${l}`);
}

function info(tag, text) {
  line(tag, text);
}

/** An error, with the stack when there is one worth reading. */
function error(tag, text, err) {
  line(tag, `error: ${text}`);
  if (err && err.stack && !(err.stack.startsWith(`Error: ${text}`) && err.stack.split("\n").length < 2)) {
    line(tag, err.stack);
  }
}

/**
 * Log an error and show it. The notification always offers the log; `more`
 * adds actions as `{ label: () => void }`. Not awaited by callers: the
 * notification stays until dismissed.
 */
function report(tag, text, more = {}) {
  error(tag, text);
  notify(text, more);
}

/** The notification half of report(), for an error already logged. */
function notify(text, more = {}) {
  const actions = [...Object.keys(more), "Show Output"];
  Promise.resolve(vscode.window.showErrorMessage(`grebe: ${text}`, ...actions)).then((pick) => {
    if (pick === "Show Output") output().show(true);
    else if (pick && more[pick]) more[pick]();
  });
}

/** A writer for one tag, shaped like an OutputChannel, for code that streams
 *  text (a process's stderr) as well as whole lines. */
function tagged(tag) {
  let partial = "";
  return {
    append(text) {
      partial += text;
      const cut = partial.lastIndexOf("\n");
      if (cut < 0) return;
      line(tag, partial.slice(0, cut));
      partial = partial.slice(cut + 1);
    },
    appendLine: (text) => line(tag, text),
    show: (preserveFocus) => output().show(preserveFocus),
    dispose() {},
  };
}

/** Register a command whose failures are reported rather than lost. */
function command(name, fn) {
  return vscode.commands.registerCommand(name, async (...args) => {
    try {
      return await fn(...args);
    } catch (e) {
      const message = e && e.message ? e.message : String(e);
      error("extension", `${name} failed: ${message}`, e);
      notify(`${name.replace(/^grebe\./, "")} failed: ${message}`);
      return undefined;
    }
  });
}

module.exports = { output, info, error, report, notify, tagged, command };
