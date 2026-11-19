# grebe for VS Code (and forks: Antigravity, Cursor, VSCodium, ...)

**Write, check and run DuckDB SQL without leaving the editor.** grebe lints
and formats DuckDB SQL with a parser built from DuckDB's own grammar, and
runs your SQL with **your own `duckdb` CLI**: the version you installed,
not a copy bundled into the editor.

- **Run SQL**: `Cmd/Ctrl+Enter` runs the statement at the cursor (or every
  statement in the selection), `Cmd/Ctrl+Shift+Enter` the file, **▶ Run**
  above a statement just that one. Results land in a fast grid you can put
  anywhere.
- **Catalog**: the tables, views and columns in your database, in any
  `.duckdb` file you browse, and in PostgreSQL databases you attach. The
  editor knows them too: it completes table and column names and marks the
  ones the session does not have.
- **One session for the whole window**, kept from file to file; tables
  that can outlive a reload (`:workspace:`); attachments that come back.
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

- **grebe 1.0.0** with a check mark: the language server is up and linting.
  With an error icon on red, it could not start; click it for the reason.
- **DuckDB 2.0.0 · :memory:**: the CLI SQL runs with, and the database.
  Click it to choose `:memory:`, `:workspace:` or a database file. A file
  running in a session of its own adds that session's name. A warning icon
  means the CLI could not be used; hover it for why.

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
| **grebe: Choose DuckDB Database** | `:memory:`, `:workspace:` or a `.duckdb` file |
| **grebe: Cancel Running Query** | stop the statement running now; the session carries on |
| **grebe: Restart DuckDB Session** | start fresh, and hand back all the memory the session held: the session of the file in front (also the ↻ on the Catalog) |
| **grebe: Delete Workspace Database** | delete what `:workspace:` has kept |

- **One session stays open**, like a terminal: `TEMP` tables, `SET` options
  and in-memory data survive from one run to the next, and from one file to
  the next: every file in the window runs in the same session, whichever
  folder it is in. The session ends when you restart it, choose another
  database or CLI, or close the window.
- **Statements run one at a time** and each result shows under its own
  statement. grebe's own tokenizer splits them, so a `;` inside a string or
  a comment never splits a statement.
- **A run stops at the first error**, marks it in the editor where DuckDB
  reported it, and shows the line with a caret under the spot in the grid.
- **Relative paths** in SQL (`read_csv('data/orders.csv')`) resolve from
  the workspace folder of the file being run, as if you ran `duckdb` from
  the project root; for a file outside every folder, from the file's own
  directory. With several folders open the session follows the file: no
  restart, nothing lost.
- **DuckDB 1.5 and 2.0** both work; point `grebe.duckdb.path` at the one
  you want. A database file written by 2.0 cannot be opened by 1.5, and if
  that is why a run fails, the error says so.

## Sessions

- **A session of its own for a file.** Start a file with

  ```sql
  -- grebe: session prod
  ```

  and it runs in a separate DuckDB process named `prod`: its tables, TEMP
  tables and settings are its own, and nothing it does touches the shared
  session. Every file with the same name shares that session. The status
  bar and the Catalog follow the file in front; **Restart DuckDB Session**
  restarts that one.
- **Its database.** A named session is in memory unless the comment says
  otherwise: `-- grebe: session prod warehouse/prod.duckdb` opens that file
  (relative to the workspace folder), `-- grebe: session prod :workspace:`
  keeps it with the workspace. It never opens the file the shared session
  has open, and naming that file in the comment is an error: two DuckDB
  processes cannot write one file.
- **`:workspace:`: tables that outlive the window.** Set
  `grebe.duckdb.database` to `:workspace:` (or choose it from the status
  bar) and the session's database is a file in the editor's own storage for
  this workspace. Tables and views are still there after a window reload,
  an editor restart or a crash, and nothing is added to your project.
  It needs a folder or workspace to be open; without one it is `:memory:`.
  `TEMP` tables and `SET` options still end with the session. **grebe:
  Delete Workspace Database** starts over. A file written by DuckDB 2.0
  cannot be opened by 1.5: after switching back to an older CLI, delete it.

### Connections

The databases your sessions attach are kept in one place, the
`grebe.duckdb.connections` setting:

```json
"grebe.duckdb.connections": [
  { "alias": "sales", "path": "data/sales.duckdb" },
  { "alias": "warehouse", "path": "postgresql://me@db.example.com/warehouse", "readOnly": true },
  { "alias": "archive", "path": "/data/archive.duckdb", "detached": true },
  { "alias": "stage", "path": "stage.duckdb", "session": "etl" }
]
```

An entry is `{ alias, path, type?, readOnly?, detached?, session? }`:
`alias` is the name SQL uses (`sales.orders`), `path` a database file
(relative to the workspace folder) or a connection string, `type` what it
is when the path does not say (`sqlite`, `mysql`; a `postgresql://` URI is
`postgres`), `session` the `-- grebe: session NAME` session it belongs to
(without it, the shared session).

- **The Catalog writes it.** Attaching a file or a PostgreSQL database
  from the Catalog adds an entry. **Detach** sets `"detached": true` and
  lets go of the database; **Attach Again** clears it. The **X**
  (Forget) removes the entry, and detaches it if it is attached.
- **Where.** A new entry goes to the settings that already have
  `grebe.duckdb.connections` -- workspace folder, workspace or user -- when
  exactly one of them does; otherwise to your user settings. Entries from
  all three are read together; for the same alias the more specific one
  wins. The X removes an entry from every one that has it.
- **Edit it by hand, and the session follows.** A new entry is attached, a
  removed or newly detached one is detached, and one whose path, type or
  `readOnly` changed is detached and attached again. The Catalog is drawn
  again.
- **An `ATTACH` in your SQL is not saved.** It lasts as long as the
  session. In the Catalog such a database has a **Keep** pin, which adds
  it to the setting as it is attached.
- **Starting a session never waits for a connection.** The session is
  ready as soon as DuckDB is. Database files are attached right after,
  in the background, without asking anything; one that cannot be (a file
  that moved) says why in the Catalog, with **Attach Again**.
- **A server is attached when it is first needed**: when a statement you
  run refers to it (`warehouse.public.orders`, `USE warehouse`), when you
  open it in the Catalog, or with **Connect**. Only then does a
  server that wants a password ask for it. Each attach has 20 seconds: a
  server that does not answer is given up on (PostgreSQL connections are
  given libpq's `connect_timeout` for that), and the run that needed it
  goes on and fails with DuckDB's own error. A session runs one statement
  at a time, so a statement behind an attach waits at most for what is
  left of those 20 seconds; while a password prompt is open the session
  is free.
- **Until then the editor still knows its tables**, from what the
  [catalog index](#the-catalog-in-the-editor) kept of it last time:
  completion, the `CAT` checks and the Catalog work without connecting.
  It shows as *not connected*, and opening it (or **Connect**) connects.
- **Passwords are never written.** One in a connection string is dropped;
  the server asks when it needs one, and the answer is used for that
  attach only.
- With `grebe.duckdb.reattach: false` nothing is attached when a session
  starts, files included: each is attached when a statement refers to it
  or when you ask.
- An earlier version kept attachments per workspace, out of sight. They
  are moved to the setting once (to your user settings, unless the rule
  above says otherwise), and the log says how many.

## Catalog

The **DuckDB** icon in the activity bar opens the **Catalog**: everything
your session holds, kept up to date as you run. Like any view, it can be
dragged to another side bar or the panel.

- **Every running session** has a line of its own when files run in named
  sessions; the one the file in front uses is the open one.

- **The session line** shows how much memory DuckDB is using, of its
  limit. Its **↻** (also in the Catalog's title bar) restarts the session,
  which frees all of it without reloading the window. It asks first when
  the session is running: an in-memory database goes with it, while a
  database file is kept. The Catalog stays usable during a long query; the
  memory figure catches up when DuckDB is free.
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

The databases are drawn from the same index the editor completes from
(see [The catalog in the editor](#the-catalog-in-the-editor)), so the
Catalog asks DuckDB nothing about them, however many tables are attached:

- **Your own databases** -- the session's, in-memory and TEMP ones,
  attached DuckDB files -- are read once after a run, for the editor and
  the Catalog together, and the Catalog shows the result: a new table, a
  new row estimate. With a foreign database attached, a run of nothing
  but queries reads nothing. They look as they always did.
- **A foreign database** -- PostgreSQL, SQLite, MySQL -- is listed
  closed, with when it was indexed: `indexed 5 min ago`. Open it and its
  schemas, tables, views and columns come from the index, at once, also
  right after a restart, from what was kept. A database that has never
  been indexed says *indexing…* until it has been, a moment after it is
  attached. What someone else changes on the server appears when the
  index is read again: **Refresh Connection Index** on the database, or
  the Catalog's **Refresh** button. A PostgreSQL table shows the server's
  own row estimate, when it has one. PostgreSQL's system schemas
  (`pg_catalog`, `information_schema`) are not listed.
- **A long list** -- more than 500 tables or views in one schema -- is
  shown as folders of 500, each named by its first and last table:
  `orders_2019 … orders_2023`. Tables are in name order, so the one you
  want is two clicks away, and nothing is built for a folder or a
  database until you open it.
- After a run the Catalog asks the session only for its variables,
  secrets, extensions and memory figure, and never while a run is in
  progress: a run that starts does not wait for the Catalog.
- A database the index holds nothing of (*not in the editor: too large*,
  or *not indexed* because it could not be read) is read from the session
  when you open it. So is everything with `grebe.duckdb.catalogIndex` or
  `grebe.duckdb.catalogInEditor` off: then the Catalog reads every
  database after every run, lists foreign databases open, and shows
  lists whole.

Click a table to preview it in the grid; right-click for **Show Columns**,
**Show Stats**, **Count Rows**, **Copy Qualified Name** or **Insert Name
into Editor**.

**Attach Database to Session...** (the plug icon on the Catalog, or
right-click a `.duckdb` or `.sqlite` file in the Explorer) attaches a file
to the session, read-only or read-write, under a name taken from the file:
`warehouse.duckdb` becomes `warehouse`, so `FROM warehouse.orders` works in
your SQL, and adds it to [`grebe.duckdb.connections`](#connections).

Every connection in the setting is listed, attached or not:

- *attached*: **Detach** lets go of it and marks it detached; the **X**
  takes it out of the setting.
- *detached*, or *not attached* with the reason it could not be: **Attach
  Again**, or the **X**.
- *not connected*: a server, attached when first needed. Its tables are
  listed from the index while it is not connected; opening it, or **Connect**,
  connects it.
- *attached when the session starts*: before the session runs.

A database attached by an `ATTACH` in your SQL has a **Keep** pin instead
of the X: it is not in the setting until you keep it. Table and view
tooltips give the qualified name, the kind, the row estimate when there is
one and the number of columns. The files you browse are kept with the
workspace too.

### The catalog in the editor

After every run the language server is told which tables, views, columns
and macros the session holds, so in a `.sql` file:

- **Completion** offers them: tables and views after `FROM` and `JOIN`,
  the columns of the tables in the statement wherever a column goes,
  SELECT-first or FROM-first, and after `alias.` or `table.` their columns.
  An item is the name and, for a column, its type. A column two tables
  have is offered for each, inserted as `alias.column`; a table in more
  than one schema is offered for each, with `database.schema` beside it.
  Right after a join's `ON` the condition the keys imply comes first
  (`o.customer_id = c.id`). In `* EXCLUDE (`, `USING (` and
  `INSERT INTO t (` only the columns that can go there are offered. At
  most 200 items come back, the best first; typing narrows the rest.
- **Hover** on a table, a view or an alias gives one line -- full name,
  kind, row estimate, primary key -- and its first 20 columns with their
  types. On a column, one line: `table.column`, its type, `NOT NULL`, and
  `key`, `unique` or `indexed`. A column is found among the query's
  catalog tables even beside a CTE, a subquery or a file, unless one of
  those names a column of the same name too; one that two tables have
  shows both. A name in more than one schema lists each place. A macro
  shows its kind; built-in functions have no hover.
- **Expand \* to columns** (a refactoring in the lightbulb menu, kind
  `refactor.rewrite`) writes a `SELECT *` or `alias.*` out as its columns,
  in table order, qualified when the query reads more than one table, and
  leaves out the ones an `EXCLUDE` names. It is offered only when every
  table of the query is one the catalog knows.
- **A table or column the session does not have** is marked (`CAT001`,
  `CAT002`), with the nearest name when there is one. Tables a file
  creates itself, CTEs, files (`'data.csv'`) and table functions are never
  marked.

A session with no tables yet says nothing, and a file in a named session
is checked against that session only. `grebe.duckdb.catalogInEditor: false`
turns it off. The same check runs from the command line with a snapshot:
`grebe check --catalog catalog.json` (see the
[grebe README](https://github.com/ncrzw9/grebe#catalog)).

The editor keeps this **database by database**, so a large connection
costs its price once and cannot take the rest down with it:

- **Local databases** -- the session's own, in-memory and TEMP ones,
  attached DuckDB files -- are read again after a run, and only the ones
  that changed are sent to the language server.
- **A foreign database** -- PostgreSQL, SQLite, MySQL, anything that is
  not DuckDB's own -- is read **once**, when it is first attached. It is
  read again when a statement changes it (`CREATE`, `ALTER`, `DROP`, ...
  that names it, or any of them while it is the current database), when
  its index is older than `grebe.duckdb.catalogIndexMaxAge` minutes
  (default 60), and when you ask: **Refresh Connection Index** on the
  database in the Catalog, or the Catalog's **Refresh** button. Tables
  someone else creates on the server appear after one of those. Its
  system schemas (`pg_catalog`, `information_schema`) are not indexed;
  names in them are neither completed nor marked.
- **Between windows** the index of a foreign database is kept in the
  editor's storage for the workspace, so its names are there at once,
  and for a server in `grebe.duckdb.connections` before it is connected
  at all. The file is named by a hash of the connection and holds table
  and column names; the connection string and its password are never
  written. It is kept as long as the setting names the connection, and
  deleted when it no longer does.
- **A database of more than 500,000 columns** is left out, alone: the
  Catalog marks it *not in the editor: too large*, the output channel
  says so once, and every other database still completes and is checked.
- The index also records what DuckDB (or, for PostgreSQL, the server's
  own catalog) knows of each table: whether a column may be NULL, the
  estimated row count, the primary key, unique constraints and indexes.
  Hover shows them, and the join condition comes from them.

Reading a database for the editor happens after a run's results are
shown, never during a run, and one database at a time. A run that starts
while one is being read does not wait for it: the reading is interrupted
and taken up again afterwards. (Not on Windows, where DuckDB cannot be
interrupted without ending the session: there the run waits for the one
query in flight. And DuckDB walks every attached database to answer any
question about its catalog, so with a very large connection attached the
re-reading after a `CREATE` takes a moment: about 0.2 s with 5,000
PostgreSQL tables. A run of nothing but queries reads nothing.)

`grebe.duckdb.catalogIndex: false` goes back to reading everything after
every run and sending it as one snapshot, with a limit of 100,000 columns
in all, past which the editor gets no catalog.

### PostgreSQL

From the plug icon choose **PostgreSQL...**, or run **grebe: Attach
PostgreSQL Database...**, and give a connection string, either libpq's
`key=value` form or a URI:

```
dbname=ub host=localhost port=5432 user=postgres
postgresql://postgres@localhost:5432/ub
```

then a name to query it by (the database name unless you change it) and
read-only or read-write. grebe loads DuckDB's `postgres` extension,
installing it first the one time it is not installed, and attaches it; its
schemas and tables appear in the Catalog as
`local (attached · postgres · ub@localhost:5432 · read-only)`, and
`FROM local.public.customers` works in your SQL.

- **Passwords**: leave it out of the string and grebe asks for it, masked,
  if the server wants one; it is used for that attach and not stored.
  The connection is added to `grebe.duckdb.connections` without it; in a
  later session it is attached when first needed, and asks again then.
  `PGPASSWORD` and `~/.pgpass` work too, as for `psql`. A password you do
  give is never shown: the Catalog, the output channel and error messages
  print `password=***`.
- **The same in SQL**: an `ATTACH` you run yourself works the same way and
  shows up in the Catalog too:

  ```sql
  ATTACH 'dbname=ub host=localhost port=5432 user=postgres' AS local (TYPE postgres, READ_ONLY);
  ```

  DuckDB loads the extension for it on its own, unless your `~/.duckdbrc`
  turns off `autoinstall_known_extensions` or `autoload_known_extensions`.
  It lasts for the session; **Keep** in the Catalog saves it.
- Row estimates are not shown for PostgreSQL tables: DuckDB has none for
  them. **Count Rows** gives the real number.

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

## Restricted Mode

In a folder VS Code has not been told to trust, grebe still lints and
formats. Running SQL, the Catalog and data files wait until you trust the
folder (**Manage Workspace Trust**): a folder's SQL can read and write
files, and its own `grebe.path`, `grebe.duckdb.path` and
`grebe.duckdb.database` settings are not used until then.

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
- `grebe.duckdb.database`: `:memory:` (the default), `:workspace:` (a
  database kept with the workspace, see [Sessions](#sessions)) or a
  database file; relative paths resolve against the workspace folder.
- `grebe.duckdb.reattach`: attach the database files of
  `grebe.duckdb.connections` when a session starts (default on). Off:
  nothing is attached at start; each connection is attached when a
  statement refers to it or when you ask.
- `grebe.duckdb.connections`: the connections your sessions attach, as
  `{ alias, path, type?, readOnly?, detached?, session? }` (see
  [Connections](#connections)).
- `grebe.duckdb.catalogInEditor`: give the language server the session's
  tables and columns for completion and the `CAT` checks (default on).
- `grebe.duckdb.catalogIndex`: keep that database by database, reading a
  foreign database once instead of after every run (default on; see
  [The catalog in the editor](#the-catalog-in-the-editor)).
- `grebe.duckdb.catalogIndexMaxAge`: minutes after which a foreign
  database is read again (default 60; 0 never does by age).
- `grebe.duckdb.maxRows`: most rows a query shows, and read from a data file
  opened in the grid (default 10,000). DuckDB stops producing rows there;
  **Count** gives the full total and Export always writes every row.
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
  `grebe check --select` flag. Empty, the default, is the rules that are on
  by default, as bare `grebe check` runs them. List codes to pick your own
  set (any that are off by default are turned on), `["FRIENDLY"]` for the
  friendly-SQL rewrites, or `["ALL"]` for every MOD rule. Parse errors
  (`PRS`), other-dialect notices (`SRC`) and catalog checks (`CAT`) are
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

## Rewriting as friendly SQL

**grebe: Rewrite as Friendly SQL** (Command Palette, or the editor's context
menu) turns conventional SQL into DuckDB's friendly SQL and formats the
result:

```sql
SELECT region, count(*) AS orders FROM sales WHERE year = 2024 GROUP BY region;
-- becomes
FROM sales SELECT region, orders: count() WHERE year = 2024 GROUP BY ALL;
```

With a selection it rewrites the statements the selection touches and leaves
the rest of the file alone; with none, the whole file. A statement it has
nothing to change in is not reformatted. It is one undo step.

It applies the rules the CLI calls `FRIENDLY`, the same as
`grebe check --fix --select FRIENDLY`: FROM before SELECT (MOD041), a bare
`SELECT *` dropped (MOD022), `GROUP BY ALL` (MOD003), `ORDER BY ALL`
(MOD013), `count()` (MOD001), `count_if` (MOD030) and prefix aliases
(MOD023). Each rewrite returns the same rows as the SQL it replaces. A
query with a comment between `SELECT` and the end of its `FROM` clause keeps
its clause order, so the comment stays where you put it, and a statement
marked `-- grebe: ignore` is not rewritten. `grebe.select` does not change
what the command applies.

The rewrite is also a code action of kind `source.grebe.friendly`, for the
whole file. To run it on every save:

```json
"[sql]": {
  "editor.codeActionsOnSave": { "source.grebe.friendly": "explicit" }
}
```

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
`grebe format`. Layout has a few knobs and they live in `grebe.toml`'s
`[format]` table (`indent_size`, `inline_threshold`, `keyword_case`,
`function_paren_space`, `keyword_paren_space`, `semicolon`, `comma`,
`logical_operator`, `function_case`, `type_case`, `indent_style`,
`blank_lines_between_statements`, `max_blank_lines_in_statement`,
`list_layout`, `case_layout`, and `style` for a named bundle of them), found
by walking upward from the file being formatted; the editor's `tabSize` is
ignored on purpose, so the editor and CI never disagree about a file. The
same `grebe.toml` also supplies `select`, `[severity]` overrides and
`foreign-heads` to the diagnostics — an explicit `grebe.select` setting in the
editor wins over the file's `select`.

## What this does NOT do

- **No bundled DuckDB.** Running SQL uses the `duckdb` CLI you point it at;
  grebe itself links no database engine.
- **No go-to-definition, rename or references.** The language server's
  exchange is `didOpen`, `didChange` (full sync), `didSave`, `didClose`,
  semantic tokens, completion, hover, code actions, formatting, and the
  statement boundaries a run uses.

Run `grebe rules` for the current rule table.

## Build from source

```sh
cd editors/vscode
npm install
npx @vscode/vsce package
```

This produces `grebe-1.0.0.vsix` in this directory: the universal package,
with no binary inside, so set `grebe.path` or put `grebe` on `PATH`.
`package-targets.js` builds the per-platform packages from the release
archives.

The tests run with Node's own test runner. `GREBE_BIN` names a `grebe` to
run the real language server against, `DUCKDB_CLI` a `duckdb` CLI to run
SQL with, `PG_TEST_CONN` a PostgreSQL connection string (with a password)
for the attach test; tests that need one are skipped without it. The grid's browser
test needs Playwright:

```sh
GREBE_BIN=$(command -v grebe) DUCKDB_CLI=$(command -v duckdb) \
  NODE_PATH=$(npm root -g) node --test editors/vscode/test/*.test.js
```
