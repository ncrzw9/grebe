# grebe

A DuckDB SQL formatter and linter. One static binary with no runtime
dependencies: no database engine, no catalog, no network. Give it a
[catalog snapshot](#catalog) and it also checks table and column names and
completes them in your editor.

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
project, not identical work.

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

## Configuration

`grebe.toml`, or `[tool.grebe]` in `pyproject.toml`, found upward from the
first path (`--config PATH` reads a specific file, `--no-config` reads none):

```toml
include = ["**/*.sql"]
exclude = ["generated/**"]
# select = ["MOD001", "MOD010"]  # run only these MOD rules (opt-in ones included)
# select = ["ALL"]               # every MOD rule, default-on and opt-in alike
# catalog = "catalog.json"       # a catalog snapshot: turns on CAT001 and CAT002

[severity]
MOD001 = "warning"   # error | warning | info | off; a level turns an opt-in rule on

[format]
indent_size      = 4
inline_threshold = 100
keyword_case     = "upper"
```

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
`name.`, on statements that do not parse yet. An editor with a database
open can send its catalog directly (the VS Code extension sends the
session's); any other LSP client gets the `grebe.toml` one.

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

## Scope

- DuckDB only. Statements from other dialects are detected and skipped.
- No binding, and no catalog needed: every `MOD` rule reads the SQL alone.
  The `CAT` rules look names up in a [catalog snapshot](#catalog) when you
  give them one, and stay silent when in doubt.
- Templated SQL (dbt/Jinja) is detected and skipped.
- Formatting is never a lint finding.

## License

MIT. Includes DuckDB's PEG grammar (MIT); see `NOTICE`.
