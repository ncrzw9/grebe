// Finding and checking the grebe binary. No `vscode` import, so it can be
// tested with plain Node.
const { execFile } = require("child_process");
const fs = require("fs");
const os = require("os");
const path = require("path");

const EXE = process.platform === "win32" ? "grebe.exe" : "grebe";

// Where to look, in order. An explicit grebe.path is the only candidate:
// falling back past a path the user chose would hide their mistake. Without
// one, the binary bundled in a platform-specific package comes first, so the
// server matches the extension's version; then ~/.local/bin, because GUI apps
// launched from the Dock don't inherit the shell PATH; then PATH itself.
function candidates(explicit, extensionDir, home = os.homedir()) {
  if (explicit) return [expandHome(explicit, home)];
  const out = [];
  const bundled = path.join(extensionDir, "bin", EXE);
  if (fs.existsSync(bundled)) out.push(bundled);
  const local = path.join(home, ".local", "bin", EXE);
  if (fs.existsSync(local)) out.push(local);
  out.push(EXE);
  return out;
}

function expandHome(p, home) {
  return p === "~" || p.startsWith("~/") ? path.join(home, p.slice(1)) : p;
}

// Run `<command> --version` and accept it only if it answers like grebe.
// Starting an unchecked binary with `lsp` is how a wrong grebe.path goes
// wrong silently: `duckdb lsp` opens (and creates) a database file named
// `lsp` in the workspace, then reads the JSON-RPC as SQL and never answers.
// stdin is closed, so a program waiting for input sees end-of-file instead
// of hanging until the timeout.
function probe(command, timeoutMs = 5000) {
  return new Promise((resolve) => {
    const child = execFile(
      command,
      ["--version"],
      { timeout: timeoutMs, windowsHide: true },
      (err, stdout) => {
        const out = String(stdout ?? "").trim();
        const m = /^grebe (\d+\.\d+\.\d+\S*)$/.exec(out.split(/\r?\n/)[0] ?? "");
        if (m) return resolve({ ok: true, command, version: m[1] });
        let reason;
        if (err && err.code === "ENOENT") reason = "not found";
        else if (err && err.killed) reason = `no answer to --version within ${timeoutMs / 1000}s`;
        else if (err && typeof err.code === "string") reason = err.message;
        else reason = `--version printed ${JSON.stringify(out.slice(0, 80))}, not "grebe <version>"`;
        resolve({ ok: false, command, reason });
      },
    );
    child.stdin?.end();
  });
}

// The first candidate that answers like grebe, plus every failure on the
// way, so the log can say what was tried.
async function resolveServer(explicit, extensionDir, timeoutMs) {
  const tried = [];
  for (const command of candidates(explicit, extensionDir)) {
    const result = await probe(command, timeoutMs);
    if (result.ok) return { found: result, tried };
    tried.push(result);
  }
  return { found: undefined, tried };
}

module.exports = { candidates, probe, resolveServer, EXE };
