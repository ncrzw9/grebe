// Kills our `duckdb` processes if the editor dies without shutting them down.
//
// A CLI busy with a query does not notice its parent is gone: its stdin
// closes, but it reads stdin only between statements. Measured: with the
// extension host killed mid-query, the CLI was re-parented to init and kept
// running the query at full CPU. A clean shutdown is already handled
// (deactivate disposes every session); this covers the unclean one -- a
// crash, a force-quit, a `kill -9`.
//
// The watchdog is a second, idle Node process (the editor's own runtime,
// run as Node) holding a pipe from the extension host. It is told each
// session's pid as the session starts and ends. When the pipe closes, which
// the OS does however the extension host ends, it kills every pid it still
// holds and exits. It is idle the rest of the time, so it notices at once.

"use strict";

const { spawn } = require("child_process");

const SCRIPT = `
const pids = new Set();
let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (d) => {
  buf += d;
  for (let i; (i = buf.indexOf("\\n")) >= 0; ) {
    const [op, pid] = buf.slice(0, i).split(" ");
    buf = buf.slice(i + 1);
    if (op === "+") pids.add(Number(pid));
    else if (op === "-") pids.delete(Number(pid));
  }
});
const reap = () => {
  for (const pid of pids) {
    try { process.kill(pid, "SIGKILL"); } catch {}
  }
  process.exit(0);
};
process.stdin.on("end", reap);
process.stdin.on("error", reap);
`;

let child = null;

function ensure() {
  if (child && child.exitCode === null && !child.killed) return child;
  child = spawn(process.execPath, ["-e", SCRIPT], {
    // Inside the editor, process.execPath is the editor's Electron binary;
    // this makes it behave as plain Node.
    env: { ...process.env, ELECTRON_RUN_AS_NODE: "1" },
    stdio: ["pipe", "ignore", "ignore"],
    windowsHide: true,
  });
  child.on("error", () => {
    child = null;
  });
  child.stdin.on("error", () => {});
  // The watchdog must never be what keeps this process alive.
  child.unref();
  if (child.stdin.unref) child.stdin.unref();
  return child;
}

/** Kill `pid` if this process ends without saying otherwise. */
function watch(pid) {
  if (!pid) return;
  try {
    ensure().stdin.write(`+ ${pid}\n`);
  } catch {
    // No watchdog: the session still works, only the safety net is gone.
  }
}

/** `pid` has ended, or been ended: stop watching it (pids are reused). */
function unwatch(pid) {
  if (!pid || !child) return;
  try {
    child.stdin.write(`- ${pid}\n`);
  } catch {
    // Nothing to do: the watchdog is gone.
  }
}

/** The watchdog's own pid, for tests. */
const pid = () => (child ? child.pid : null);

module.exports = { watch, unwatch, pid };
