# grebe for VS Code (and forks: Antigravity, Cursor, VSCodium, ...)

**Write, check and run DuckDB SQL without leaving the editor.** grebe lints
and formats DuckDB SQL with a parser built from DuckDB's own grammar, and
runs your SQL with **your own `duckdb` CLI**: the version you installed,
not a copy bundled into the editor.

- **Run SQL**: `Cmd/Ctrl+Enter` runs the statement at the cursor (or every
  statement in the selection), `Cmd/Ctrl+Shift+Enter` the file, **▶ Run**
  above a statement just that one. Results land in a fast grid you can put
  anywhere.
- **Catalog**: the tables, views and columns in your database, and in any
  `.duckdb` file you browse.
- **Data files in the grid**: open a `.parquet` file and its rows appear in
  the grid, with column types; CSV and TSV through **Open With...**.
- **Lint as you type, quick fixes, formatting** and DuckDB-aware
  highlighting.

## Install

Install **grebe** from the Extensions view (publisher `ncrzw9`), or:

```sh
code --install-extension ncrzw9.grebe
```

(substitute your editor's CLI: `antigravity-ide`, `cursor`, `codium`, ...)

Linting, formatting and highlighting work at once. On macOS, Linux and
Windows x64 the extension carries its own `grebe` binary, the same version
as the extension; on other platforms install it first (`cargo install
grebe`, or an archive from the
[releases](https://github.com/ncrzw9/grebe/releases)).

To **run** SQL, install the DuckDB CLI (macOS: `brew install duckdb`;
others: [duckdb.org](https://duckdb.org)). If the
editor does not find it on `PATH` -- an editor launched from the Dock often
does not inherit your shell's -- set its path:

```json
"grebe.duckdb.path": "/opt/homebrew/bin/duckdb"
```

## Is it running?

Two status bar items, bottom right:

- **grebe 0.6.0** with a check mark: the language server is up and linting.
  With an error icon on red, it could not start; click it for the reason.
- **DuckDB 1.5.5 · :memory:**: the CLI SQL runs with, and the database.
  Click it to choose `:memory:` or a database file. A warning icon means the
  CLI could not be used; hover it for why.

Paste this into a `.sql` file to see both at work:

```sql
CREATE TABLE events AS SELECT range AS id, DATE '2024-01-01' + range::INT AS ts FROM range(100);
SELECT * FROM events LIMIT 10;
select count(*) from events where year(ts) = 2024;
```

The last two lines get squiggles (`LIMIT` without `ORDER BY`, `count(*)`,
`year(ts)`); `Cmd/Ctrl+Shift+Enter` runs all three and shows the results.

## Running SQL

| key or command | what it does |
|---|---|
| `Cmd/Ctrl+Enter` | run the statement at the cursor, or every statement the selection touches |
| `Cmd/Ctrl+Shift+Enter` | run the whole file |
| **▶ Run** above a statement | run just that statement |
| ▶ in the editor title bar | the same as the keys, with the mouse |
| **grebe: Choose DuckDB Database** | `:memory:` or a `.duckdb` file |
| **grebe: Cancel Running Query** | stop the statement running now; the session carries on |
| **grebe: Restart DuckDB Session** | start fresh |

- **One session stays open**, like a terminal: `TEMP` tables, `SET` options
  and in-memory data survive from one run to the next.
- **Statements run one at a time** and each result shows under its own
  statement. grebe's own tokenizer splits them, so a `;` inside a string or
  a comment never splits a statement.
- **A run stops at the first error**, marks it in the editor where DuckDB
  reported it, and shows the line with a caret under the spot in the grid.
- **Relative paths** in SQL (`read_csv('data/orders.csv')`) resolve from
  the workspace folder, as if you ran `duckdb` from the project root.
- **DuckDB 1.5 and 2.0** both work; point `grebe.duckdb.path` at the one
  you want. A database file written by 2.0 cannot be opened by 1.5, and if
  that is why a run fails, the error says so.

## Stopping a long query

While a run is going, the status bar shows a clock (**DuckDB · 12 s**),
the editor's ▶ becomes ■, and the results show **Running statement 2 of 5**
with a **Cancel** button. Any of those, or **grebe: Cancel Running
Query**, stops it:

- **The query stops within milliseconds and the session carries on**:
  `TEMP` tables, `SET` options and in-memory data are all still there. The
  rest of the run does not run.
- **A cancelled export leaves nothing behind**: no partial file, and a file
  you were about to overwrite is left as it was.
- **`grebe.duckdb.queryTimeout`** cancels any statement that runs longer
  than that many seconds, if you want a safety net (off by default).
- Starting a run while another is going offers to cancel the first, rather
  than quietly queueing behind it.
- In the rare case DuckDB does not stop within 3 seconds, the session is
  ended instead and the next run starts a fresh one; the results say so.
  On **Windows**, where one process cannot interrupt another, cancelling
  always ends the session this way.

**No stray `duckdb` processes.** Every DuckDB process the extension starts
ends with it: on a normal shutdown, and also if the editor crashes or is
force-quit in the middle of a query, when a small watchdog process ends
them.

## The results grid

Results appear in the **DuckDB Results** view, which starts in the bottom
panel. Put it wherever suits you:

- **Drag its tab** to the side bar, the secondary side bar, or back to the
  panel, like any view. It keeps showing the current results when moved.
- **Open Results in Editor** (the icon in the view's title bar) puts the
  same results in an editor tab, which can sit in any editor group, split
  beside your SQL, or be moved into a window of its own (right-click the
  tab, **Move into New Window**). Both stay in step while open.

The grid draws only what is in view, so 100,000 rows or 200 columns scroll
at full speed.

- **Select**: click or drag cells; click a **header** for the column
  (Shift+click or drag across headers for several), a **row number** for
  the row. Arrows, `Shift`+arrows, `Home`/`End`, `PgUp`/`PgDn` move and
  extend; `Cmd/Ctrl`+arrows jump to the edge; `Ctrl+Space`/`Shift+Space`
  select the column/row; `Cmd/Ctrl+A` everything.
- **Copy**: `Cmd/Ctrl+C` copies tab-separated text that pastes into a
  spreadsheet, with the column names when whole columns are selected. NULL
  pastes as an empty cell.
- **Sort and size**: the **↕** at a header's right edge sorts ascending,
  descending, off (NULLs last). Drag a header's edge to resize it,
  double-click it to fit.
- **Export** a query's result as **CSV, TSV, Parquet or JSON**. DuckDB
  writes the file itself (`COPY`), so it has every row with exact types, not
  only what is on screen. Export runs the query again, so values from
  `now()` or `random()` may differ from what you saw, and it is offered
  only for queries, never for statements that change data.
- **Types** of each column show in its header.

## Catalog

The **DuckDB** icon in the activity bar opens the **Catalog**: everything
your session holds, refreshed after every run. Like any view, it can be
dragged to another side bar or the panel.

- **The session line** shows how much memory DuckDB is using, of its
  limit.
- **Databases**: the session's own (`:memory:` or a file) first, then every
  attached one, described in a few words: `attached · warehouse.duckdb ·
  read-only`, `attached · in-memory`, or its type when it is not DuckDB
  (`sqlite`, `postgres`, ...). Then **TEMP** tables and views.
- **In each schema**: tables and views, with estimated rows and column
  counts, and their columns with types; then **Macros** (with their
  parameters; hover for the definition), **Sequences** and **Types**.
- **Variables** set with `SET VARIABLE`, with their values; **Secrets**, by
  name, type and scope only (never the secret itself); and the
  **Extensions** loaded.

Click a table to preview it in the grid; right-click for **Show Columns**,
**Show Stats**, **Count Rows**, **Copy Qualified Name** or **Insert Name
into Editor**.

**Attach Database to Session...** (the plug icon on the Catalog, or
right-click a `.duckdb` or `.sqlite` file in the Explorer) attaches a file
to the session, read-only or read-write, under a name taken from the file:
`warehouse.duckdb` becomes `warehouse`, so `FROM warehouse.orders` works in
your SQL. **Detach** (on an attached database) removes it. Attaching
happens in the session, so it lasts until the session restarts, the same as
an `ATTACH` in your own SQL.

To look inside a `.duckdb` file without attaching it, right-click it in the
Explorer, then **Browse DuckDB File**: it opens **read-only** in a separate
session, so browsing can never change it or get in the way of a run. **Use
as Session Database** makes a file the database your runs use.

## Data files

**Open a `.parquet` file** and it opens in the grid, in an editor tab of its
own: the first `grebe.duckdb.maxRows` rows, each column's type in its
header, read again whenever the file changes on disk. For a **CSV or TSV**
(also `.csv.gz`), right-click it, **Open With...**, **DuckDB Grid**, or use
**Open in Grid** from the Explorer's context menu; choose it as the
default there if you want CSVs to always open as a grid.

More views, from the Explorer's context menu or the Command Palette
(*grebe: Data file*):

| | |
|---|---|
| **Show Columns** | names and types, as DuckDB reads them |
| **Show Stats** | `SUMMARIZE`: min, max, approx. distinct, avg, std, quartiles, count, null % per column |
| **Preview Rows** | the first 1,000 rows, in the results grid |
| **Show Parquet Metadata** | rows and row groups, then per column: physical type, compression, compressed and uncompressed bytes, min/max, nulls |
| **Show CSV Dialect** | what DuckDB detects: delimiter, quoting, header, column types, and a ready `read_csv(...)` call to copy |

In SQL, **hover a file path** (`FROM 'orders.parquet'`,
`read_csv('data/people.tsv')`, globs like `'data/*.parquet'`) to see its
columns and types, with links to the views above.

Data files are read with your `duckdb` CLI in a separate in-memory session:
looking at a file never touches the database your script is working on.

## When something goes wrong

Everything the extension does is logged in one place: the **grebe** output
channel (**grebe: Show Output**, or click either status bar item). Each line
is timestamped and tagged with where it came from: `server` (the language
server), `duckdb` (the session: every statement run with its outcome and
timing, and on a failure the whole statement and where DuckDB stopped),
`catalog`, `grid`.

- **Errors are shown when they happen**, not at your next run: a
  `grebe.duckdb.path` that is missing, not executable or not DuckDB; a
  broken `~/.duckdbrc`; a database file this CLI cannot open (checked the
  moment you choose it); a session that crashes while idle; a CLI that never
  answers (given up on after 20 seconds instead of hanging).
- **Every notification has Show Output**, and the ones a setting fixes have
  a button for that setting.
- **Nothing fails silently**: if a command hits an unexpected error, it is
  shown and logged with its stack trace -- please include that when
  reporting a bug.
- If you never set `grebe.duckdb.path` and have no `duckdb`, nothing nags
  you until you run something: linting and formatting do not need it.

## Settings

- `grebe.duckdb.path`: the `duckdb` CLI to run SQL with (default: `duckdb`
  on `PATH`). grebe never bundles or links DuckDB.
- `grebe.duckdb.database`: `:memory:` (the default) or a database file;
  relative paths resolve against the workspace folder.
- `grebe.duckdb.maxRows`: most rows kept per result, and read from a data
  file opened in the grid (default 100,000). The full row count of a query
  is always reported, and Export always writes every row.
- `grebe.duckdb.queryTimeout`: cancel a statement that runs longer than
  this many seconds (default 0: never).
- `grebe.duckdb.codeLens`: the **▶ Run** link above each statement (default
  on).
- `grebe.path`: a `grebe` executable of your own. Leave it empty to use the
  bundled binary, then `~/.local/bin/grebe`, then `grebe` on `PATH`. It is
  run with `--version` first and used only if it answers as grebe, so
  pointing it at the wrong program gives an error rather than a server that
  never answers. Changing it restarts the server.
- `grebe.select`: the MOD rules to lint for, same meaning as the CLI's
  `grebe check --select` flag: only the listed rules run, and any of them
  that are off by default get turned on. Defaults to `["ALL"]`, every MOD
  rule there is, so the editor is loud out of the box even though bare
  `grebe check` on the command line stays quiet, and stays loud as new
  rules ship. Parse errors (`PRS`) and other-dialect notices (`SRC`) are
  always shown unless `grebe.toml` sets them to `off` under `[severity]`.
  List codes explicitly to pick a subset instead; the change takes effect
  immediately. A code that doesn't name a real rule is dropped and reported
  with a warning.

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

- **No bundled DuckDB.** Running SQL uses the `duckdb` CLI you point it at;
  grebe itself links no database engine.
- **No completion.** The language server's exchange is `didOpen`,
  `didChange` (full sync), `didSave`, `didClose`, semantic tokens, code
  actions, formatting, and the statement boundaries a run uses.

Run `grebe rules` for the current rule table.

## Build from source

```sh
cd editors/vscode
npm install
npx @vscode/vsce package
```

This produces `grebe-0.6.0.vsix` in this directory: the universal package,
with no binary inside, so set `grebe.path` or put `grebe` on `PATH`.
`package-targets.js` builds the per-platform packages from the release
archives.

The tests run with Node's own test runner. `GREBE_BIN` names a `grebe` to
run the real language server against, `DUCKDB_CLI` a `duckdb` CLI to run
SQL with; tests that need one are skipped without it. The grid's browser
test needs Playwright:

```sh
GREBE_BIN=$(command -v grebe) DUCKDB_CLI=$(command -v duckdb) \
  NODE_PATH=$(npm root -g) node --test editors/vscode/test/*.test.js
```
