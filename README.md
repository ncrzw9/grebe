# grebe

A DuckDB SQL formatter and linter. One static binary with no runtime
dependencies: no database engine, no catalog, no network. Give it a
[catalog snapshot](#catalog) and it also checks table and column names,
checks queries against what the catalog says of each table, and completes
names in your editor.

The parser is generated from DuckDB's own PEG grammar, vendored verbatim, so
grebe parses the dialect DuckDB parses instead of approximating it. The
grammar is DuckDB 2.0.0's. Measured on 62,253 distinct statements (every
statement in DuckDB's own SQL test suite at v2.0.0, valid and deliberately
invalid alike, plus the TPC-H and TPC-DS queries):

- **DuckDB 2.0.0:** grebe agrees with the engine's parser on 99.5% of
  statements and accepts every statement the engine accepts. The rest are
  statements the engine refuses in a check that runs after its grammar has
  matched.
- **DuckDB 1.5.5:** 97.3%. Of the 1,651 statements grebe accepts and 1.5.5
  does not, 1,373 are syntax that is new in 2.0; two statements 1.5.5
  accepts are rejected, both recorded with the reasoning.

## Performance

Wall-clock time on a 4-core Linux machine, median of repeated runs. grebe
parallelises across files; DuckDB dialect selected for every tool.

| Input                   | grebe check | grebe format --check | sqruff lint | sqlfmt --check | SQLFluff lint (4 procs) |
|-------------------------|------------:|---------------------:|------------:|---------------:|------------------------:|
| TPC-H + TPC-DS, 121 files |  0.22 s |               0.12 s |      0.51 s |         0.62 s |                  14.4 s |
| 1,000 model files       |      0.19 s |               0.14 s |      0.48 s |         0.94 s |                  20.0 s |

Versions: sqruff 0.40.0, sqlfmt 0.32.0, SQLFluff 4.3.0. The tools differ in
rule sets and formatting style, so this compares the time to check a
project, not identical work. The grebe columns above are 0.6.4's.

Since 0.6.4 the parser builds its tree in a second pass, reuses its memo
table, and skips grammar alternatives that cannot start with the token at
hand. Measured before and after on one 4-core Linux machine (a different
one from the table above), median of 3 runs, wall time and peak memory:

| Input                                    | check before | check after | format --check before | format --check after |
|------------------------------------------|-------------:|------------:|----------------------:|---------------------:|
| TPC-H + TPC-DS, 121 files                | 0.19 s, 64 MB | 0.12 s, 21 MB | 0.13 s, 52 MB | 0.11 s, 21 MB |
| 1,000 model files                        | 1.02 s, 84 MB | 0.61 s, 31 MB | 0.68 s, 86 MB | 0.48 s, 35 MB |
| DuckDB's SQL tests, 5,516 files, 24 MB   | 13.1 s, 2.8 GB | 4.1 s, 192 MB | 9.6 s, 2.9 GB | 3.1 s, 228 MB |
| One statement, 400 nested parentheses    | 0.44 s, 603 MB | 0.04 s, 14 MB | | |
| One statement, an `IN` list of 50,000    | 1.84 s, 860 MB | 0.89 s, 222 MB | 1.76 s, 860 MB | 1.30 s, 318 MB |

## Install

```sh
cargo install grebe
```

Prebuilt binaries for Linux, macOS and Windows are attached to each
[release](https://github.com/ncrzw9/grebe/releases).

For VS Code and its forks, install the **grebe** extension (`ncrzw9.grebe`)
from the Marketplace. It brings its own binary on macOS, Linux and Windows
x64, and adds running SQL with your own `duckdb` CLI, a results grid, a
catalog of your database, and Parquet and CSV files opened in the grid.

## Usage

```
grebe format PATHS...    format in place; --check only reports;
                          `-` formats stdin to stdout
grebe check  PATHS...    lint; --select CODE,... runs only those MOD
                          rules, including opt-in ones;
                          --fix applies safe fixes (--unsafe widens it);
                          --json prints one JSON document;
                          --catalog FILE also checks names against a
                          catalog snapshot
grebe rules              list every rule
grebe catalog [DATABASE] print a catalog snapshot of a DuckDB database
                          (runs your duckdb CLI; --duckdb PATH picks it)
grebe lsp                language server on stdio
```

Exit codes: `0` clean, `1` an error-severity finding (or `format --check`
found work), `2` usage or config error, or a file that could not be read
(missing, or not UTF-8) or written.

### Friendly SQL

`--select FRIENDLY` names the rules that rewrite conventional SQL into
DuckDB's friendly SQL, all with a safe fix, so this turns a file into that
style without changing what its queries return:

```sh
grebe check --fix --select FRIENDLY query.sql && grebe format query.sql
```

```sql
SELECT region, count(*) AS orders FROM sales WHERE year = 2024 GROUP BY region;
-- becomes
FROM sales SELECT region, orders: count() WHERE year = 2024 GROUP BY ALL;
```

The rules: FROM before SELECT (MOD041), a bare `SELECT *` dropped (MOD022),
`GROUP BY ALL` (MOD003), `ORDER BY ALL` (MOD013), `count()` (MOD001),
`count_if` (MOD030) and prefix aliases (MOD023). `FRIENDLY` can stand among
other codes (`--select FRIENDLY,MOD006`) and in a config file's `select`.
In the editor the same rewrite is the command **grebe: Rewrite as Friendly
SQL**.

## Configuration

`grebe.toml`, or `[tool.grebe]` in `pyproject.toml`, found upward from the
first path (`--config PATH` reads a specific file, `--no-config` reads none):

```toml
include = ["**/*.sql"]
exclude = ["generated/**"]
# select = ["MOD001", "MOD010"]  # run only these MOD rules (opt-in ones included)
# select = ["ALL"]               # every MOD rule, default-on and opt-in alike
# select = ["FRIENDLY"]          # the safe rewrites into DuckDB's friendly SQL
# catalog = "catalog.json"       # a catalog snapshot: turns on the CAT rules
# large-table-rows = 1_000_000   # the row estimate at which CAT003 calls a table large

[severity]
MOD001 = "warning"   # error | warning | info | off; a level turns an opt-in rule on

[format]
indent_size          = 4           # spaces per indent level, 1 to 16
inline_threshold     = 100         # what fits in this many columns stays on one line
keyword_case         = "upper"     # or "lower"
function_paren_space = false       # true: count (x), coalesce (a, b)
keyword_paren_space  = true        # false: IN(1, 2), AS(...), OVER(...)
semicolon            = "attached"  # "spaced": n ;  "newline": ; on its own line
```

The values shown are the defaults. `function_paren_space` covers every call:
functions, aggregates, window and table functions, methods, a macro's
parameter list, and the keyword forms that read as calls (`CAST`,
`COALESCE`, `EXTRACT`, `ROLLUP` and the like). A type's modifiers always hug
(`decimal(18, 2)`). `keyword_paren_space` covers every other parenthesis that
follows a keyword (`IN`, `VALUES`, `OVER`, `AS`, `EXISTS`, `USING`, `FILTER`,
`FROM`, `ON`, ...); a parenthesis after a name, as in `INSERT INTO t (a, b)`,
is not affected by either.

Silence one statement with `-- grebe: ignore[CODE]`, or every code with
`-- grebe: ignore`.

## Catalog

grebe needs no catalog. Given one, it checks names against it:

- **CAT001 unknown-table**: a table or view a statement reads or writes
  that the catalog does not have.
- **CAT002 unknown-column**: `alias.column` where the alias is a table in
  the catalog and the table has no such column.

Both are warnings, both name the nearest match when there is one
(`CAT001 unknown-table (did you mean orders?)`), and neither runs without a
catalog. The suggestion is a quick fix in the editor; `--fix` never applies
it.

A catalog is a snapshot file: a JSON array with one object per column.

```json
[{"database": "shop", "schema": "main", "table": "orders", "column": "id", "type": "BIGINT"}]
```

`grebe catalog --with-metadata` writes an object instead, which is read
wherever the array is: the same rows under `"columns"`, each with
`"nullable"`, and under `"tables"` what DuckDB knows of each table (its
`kind`, estimated `rows`, `primary_key`, `unique` constraints and
`indexes`). CAT003 to CAT010 read those; see
[What a catalog with metadata adds](#what-a-catalog-with-metadata-adds).

`grebe catalog` writes one by running your own `duckdb` CLI, read-only, on
a database file:

```sh
grebe catalog shop.duckdb > catalog.json     # --duckdb PATH if duckdb is not on PATH
grebe check --catalog catalog.json models/
```

or name it in `grebe.toml` (`catalog = "catalog.json"`, relative to that
file) and both `grebe check` and the language server use it. The snapshot is
exactly what DuckDB prints in `-json` mode for this query, so anything that
can run it can write the file (an attached PostgreSQL database included):

```sql
SELECT database_name AS database, schema_name AS schema, table_name AS "table",
       column_name AS "column", data_type AS type
FROM duckdb_columns() WHERE NOT internal;
```

The language server completes from the same catalog: columns after
`alias.`, tables after `FROM`, what is inside a schema or database after
`name.`, the columns of the statement's tables wherever a column goes, on
statements that do not parse yet. With the metadata `grebe catalog
--with-metadata` writes (types, NOT NULL, keys, indexes, row estimates) it
also shows them in completion and on hover over a table, an alias or a
column, offers the join condition the keys imply right after `ON`, and has
a code action, **Expand \* to columns** (`refactor.rewrite`), that writes a
`SELECT *` out as its columns when every table is in the catalog. An editor
with a database open can send its catalog directly (the VS Code extension
sends the session's); any other LSP client gets the `grebe.toml` one.

An editor sends it with the `grebe/catalog` notification: the whole
snapshot (`{ columns: [...] }`), or one database at a time
(`{ database: "pg", columns: [...] }` replaces that database's part and
leaves the others, `{ database: "pg", columns: null }` drops it), so a
large attached database is sent once and the small local ones after every
run. The VS Code extension does that: it reads a PostgreSQL, SQLite or
MySQL attachment once, keeps it between windows, and leaves out, alone, a
database of more than 500,000 columns. Looking a name up costs the same in
a catalog of a million columns as in a small one.

These rules match names; they do not bind. They report only what cannot be
right, and say nothing when a statement or a file does something they do
not model:

- A CTE, and a table or view the file itself creates, alters or drops,
  exists, with columns the catalog cannot vouch for.
- An unqualified name matches a table of that name in any schema or
  database of the catalog, so `USE` and `search_path` never cause a
  finding. A database the catalog does not cover (one the file attaches,
  `information_schema`) is not checked.
- File names (`'data/*.parquet'`), table functions, subqueries, `PIVOT`
  output and renamed columns (`t AS a(x, y)`) are relations of their own.
- Unqualified columns are not checked.

Measured against DuckDB 1.5.5 and 2.0.0 themselves: on the TPC-H and TPC-DS
queries with their schemas as the catalog, no findings; with each of the 32
tables dropped in turn, every statement DuckDB then rejects is reported
(560 of 560) and nothing else; with each of the 486 columns dropped in
turn, 257 of the 1,887 statements DuckDB rejects are reported (the
qualified ones) and nothing else.

### What a catalog with metadata adds

With `grebe catalog --with-metadata` (or a snapshot with the same
`"tables"` and `"nullable"`), these rules run too. Each needs a particular
fact (a row estimate, a key, an index, a nullability, a type) and says
nothing about a table the snapshot gives no such fact for, so a snapshot of
columns alone behaves as before. Each fires only where a column can be
pinned to one catalog table: the query's FROM holds catalog tables and
nothing else (no subquery, CTE, table function or table the file creates),
and exactly one of them has the column. The finding names the evidence:
`CAT003 unbounded-scan (orders holds about 2,400,000 rows)`.

- **CAT003 unbounded-scan** (info): the statement's result reads every row
  of a table estimated at `large-table-rows` or more (default 1,000,000):
  no WHERE, QUALIFY, LIMIT, SAMPLE, join, GROUP BY, DISTINCT or aggregate.
  `SELECT count(*) FROM big` is left alone on purpose: it reads everything
  because the answer needs it, and returns one row. So are CTEs and
  subqueries (whatever reads them filters them), `CREATE TABLE AS`,
  `INSERT ... SELECT`, `COPY` and views.

  ```sql
  SELECT * FROM orders;      -- orders holds about 2,400,000 rows
  ```

- **CAT004 index-defeating-filter** (info): a WHERE or ON condition wraps
  the first column of the primary key, a unique key or an index in
  `lower`/`upper`/`trim`/`strftime`/`length`, casts it to text (or a
  fraction to an integer), or matches it with a `LIKE`/`ILIKE` pattern that
  starts with a wildcard, and compares it with a constant. Run against
  DuckDB 1.5 and 2.0, each of these filters row by row instead of on the
  column. What DuckDB rewrites into a filter on the column is not flagged:
  arithmetic with a constant, widening casts, `date_trunc`, a timestamp
  cast to DATE, `LIKE 'abc%'`. Where it fires on a predicate MOD035 also
  flags, it reports in MOD035's place.

  ```sql
  SELECT id FROM users WHERE lower(email) = 'a@example.com';   -- users.email leads a unique key
  ```

- **CAT005 nullable-not-in** (warning): `x NOT IN (SELECT c FROM t ...)`
  where `t` is the subquery's only table and `c` may be NULL. It replaces
  MOD028 there, with the column named; where the catalog says `c` is NOT
  NULL, MOD028 is not reported at all. (A NULL `x` drops only its own row,
  which is not what MOD028 warns about.)

  ```sql
  SELECT id FROM orders WHERE id NOT IN (SELECT order_id FROM payments);   -- payments.order_id may be NULL
  ```

- **CAT006 ambiguous-column** (warning): an unqualified column that two or
  more tables of the same FROM have, which DuckDB rejects. A name in a
  `USING (...)`, a NATURAL, SEMI or ANTI join, a name only an outer query's
  tables have, and ORDER BY (which prefers select-list names) are left
  alone. An ON sees only the tables joined before it.

  ```sql
  SELECT id FROM orders o JOIN customers c ON c.id = o.customer_id;   -- id is in o and c
  ```

- **CAT007 insert-mismatch** (warning): `INSERT ... VALUES` that DuckDB
  rejects: a column list naming a column the table lacks, a row whose width
  is not the list's, or, without a list, a row wider than the table. A row
  narrower than the table is not flagged, since a snapshot does not say
  which columns are generated; `BY NAME`, `DEFAULT VALUES` and `INSERT ...
  SELECT` are not checked.

  ```sql
  INSERT INTO customers (id, name) VALUES (1, 'a', DATE '2024-01-01');   -- 2 columns named, 3 values given
  ```

- **CAT008 type-mismatch** (info): a comparison of two columns whose types
  are of different kinds (text, number, date or time, boolean). DuckDB
  either rejects it or casts one side. Literals are never flagged.

  ```sql
  SELECT 1 FROM users u JOIN teams t ON t.name = u.team_id;   -- teams.name is VARCHAR and users.team_id is BIGINT
  ```

- **CAT009 fan-out-join** (info): a join of exactly two tables, both with a
  key in the catalog, on column equalities (or `USING`) that cover a key of
  neither: rows can multiply on both sides.

  ```sql
  SELECT * FROM payments p JOIN refunds r ON r.customer_id = p.customer_id;
  ```

- **CAT010 redundant-distinct** (info): `SELECT DISTINCT` over one table
  whose select list (bare columns, or `*`) holds the primary key or a
  unique key with no nullable column.

  ```sql
  SELECT DISTINCT id, total FROM orders;   -- id is the primary key of orders
  ```

Turn any of them off with `[severity]` (`CAT003 = "off"`); a MOD028 or
MOD035 finding a CAT rule took over comes back when that rule is off.

## Scope

- DuckDB only. Statements from other dialects are detected and skipped.
- No binding, and no catalog needed: every `MOD` rule reads the SQL alone.
  The `CAT` rules look names up in a [catalog snapshot](#catalog) when you
  give them one, and stay silent when in doubt.
- Templated SQL (dbt/Jinja) is detected and skipped.
- Formatting is never a lint finding.

## License

MIT. Includes DuckDB's PEG grammar (MIT); see `NOTICE`.
