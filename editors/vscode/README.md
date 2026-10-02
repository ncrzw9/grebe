# grebe for VS Code (and forks: Antigravity, Cursor, VSCodium, ...)

Thin client for `grebe lsp` — a stdio LSP server that lints and formats
DuckDB SQL. It spawns the `grebe` binary and hands everything to
`vscode-languageclient`; all behavior lives in the server.

## Install

1. Get the `.vsix` (see [Build from source](#build-from-source)).
2. Install it one of two ways:

   Command line:

   ```sh
   code --install-extension grebe-0.4.1.vsix
   ```

   (substitute your editor's CLI — `antigravity-ide`, `cursor`, `codium`, ...)

   Or from the Extensions view: open the `...` menu in the Extensions
   sidebar, choose **Install from VSIX...**, and pick the file.

3. Make sure the `grebe` binary is reachable. If `grebe` is already on your
   `PATH`, there is nothing else to do. Otherwise, open Settings and set
   `grebe.path` to the full path of the binary (for example
   `~/.local/bin/grebe`). GUI-launched editors do not always inherit your
   shell's `PATH`, so this matters even if `grebe` works fine from a
   terminal.

4. Open a `.sql` file. If the server starts, lint diagnostics from `grebe`
   appear as squiggles in the editor. If it does not start (missing binary,
   wrong path, crash on launch), you'll get an error notification naming the
   `grebe.path` setting rather than silent failure.

## Settings

- `grebe.path` — path to the `grebe` executable. When unset,
  `~/.local/bin/grebe` is used if that file exists, and otherwise `grebe`
  on `PATH`.
- `grebe.select` — the MOD rules to lint for, same meaning as the CLI's
  `grebe check --select` flag: only the listed rules run, and any of them
  that are off by default get turned on. Defaults to `["ALL"]`, every MOD
  rule there is, so the editor is loud out of the box even though bare
  `grebe check` on the command line stays quiet -- and stays loud as new
  rules ship, with no setting to update. Parse errors (`PRS`) and
  other-dialect notices (`SRC`) are always shown unless `grebe.toml` sets
  them to `off` under `[severity]`. List codes explicitly to pick a subset
  instead; the change takes effect immediately, no reload needed. A code
  that doesn't name a real rule is dropped and reported with a warning
  notification rather than silently ignored.

## Syntax highlighting

VS Code's built-in SQL grammar is generic ANSI-ish SQL and doesn't know
DuckDB-specific keywords, so words like `DESCRIBE`, `SUMMARIZE`, `PIVOT`,
`UNPIVOT` and `QUALIFY` show up unhighlighted in a `.sql` file. This
extension contributes a small TextMate **injection grammar**
(`syntaxes/duckdb-keywords.injection.json`) that layers highlighting for
those words on top of the built-in grammar, scoped as `keyword.other.duckdb`.

That file is **generated**, not hand-edited — from
`crates/grebe-syntax/vendor/grammar/keywords/reserved_keyword.list`, the DuckDB grammar's own
list of reserved keywords, which `grebe`'s parser already vendors verbatim.
Regenerate it after that list changes with:

```sh
node editors/vscode/build-grammar.js
```

The server also sends **semantic tokens**, which are derived from the
actual parse and so know what a name *does*: in `FROM main.data`,
`main` is coloured as a namespace and `data` as a table, even though `data` is
also a DuckDB keyword. Those override the injection grammar wherever the
server is running, and they cover the unreserved keywords listed as gaps below
(`ASOF`, `SEMI`, `ANTI`, `MACRO`, `POSITIONAL`). The injection grammar stays
because it needs no server: it colours the file before the extension has
started, and if `grebe` fails to launch.

It only covers *reserved* keywords, deliberately. A reserved keyword can
never be a DuckDB identifier, so highlighting one is always correct.
DuckDB's other keyword lists (unreserved, column-name, function-name,
type-name) are full of words that are also legal identifiers — `NAME`,
`TYPE`, `VALUE`, `KEY`, and so on — and injecting those would paint ordinary
column and table names as if they were keywords, which is worse than
leaving them uncoloured. The practical effect: some DuckDB keywords a user might
expect to see highlighted — `ASOF`, `SEMI`, `ANTI`, `MACRO`, `POSITIONAL` —
are unreserved and so are **not** highlighted by this grammar, on purpose.

## Fixing a finding

Put the cursor on a squiggle and open the quick-fix menu (`Cmd/Ctrl .`). Rules
that carry a fix offer one, named for the rule so a list of several stays
legible. Fixes that could change what a query returns are offered too, with
"(unsafe — may change results)" in the title: the editor shows you the diff
before applying and undo is one keystroke, which is the right place to make
that call.

The same fixes are available in bulk from the CLI: `grebe check PATH --fix`,
plus `--unsafe` for the second band.

## Silencing a finding

Put `-- grebe: ignore[CODE]` on the statement — either on a line before it, or
at the end of its first line:

```sql
-- grebe: ignore[MOD001]
SELECT count(*) FROM t;

SELECT count(*) FROM u; -- grebe: ignore[MOD001]
```

Several codes: `-- grebe: ignore[MOD001, MOD025]`. Bare `-- grebe: ignore`
suppresses every code for that statement, including `PRS001`, which is how you
silence a file the parser cannot read. It applies to that one statement only,
and the CLI honours it identically — a suppression that works in the editor but
not in CI would be worse than none.

## Formatting

**Format Document** (and format-on-save, if you turn it on for SQL) asks the
server for `textDocument/formatting`, which runs the same formatter as
`grebe format`. Layout has three knobs and they live in `grebe.toml`'s
`[format]` table (`indent_size`, `inline_threshold`, `keyword_case`), found
by walking upward from the file being formatted; the editor's `tabSize` is
ignored on purpose, so the editor and CI never disagree about a file. The
same `grebe.toml` also supplies `select`, `[severity]` overrides and
`foreign-heads` to the diagnostics — an explicit `grebe.select` setting in the
editor wins over the file's `select`.

## What this does NOT do

- **No completion, no hover.** The exchange is `didOpen`, `didChange` (full
  sync), `didSave`, `didClose`, semantic tokens, code actions and
  formatting.
- **No detector for `SRC004 unchecked-statement`.** It is registered but
  cannot fire; the other 28 rules in the registry have detectors. Run
  `grebe rules` for the current table.

## Build from source

```sh
cd editors/vscode
npm install
npx @vscode/vsce package
```

This produces `grebe-0.4.1.vsix` in this directory.
