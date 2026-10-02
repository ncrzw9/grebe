# grebe

A DuckDB SQL formatter and linter. One static binary with no runtime
dependencies: no database engine, no catalog, no network.

The parser is generated from DuckDB's own PEG grammar, vendored verbatim, so
grebe parses the dialect DuckDB parses instead of approximating it. Measured
against DuckDB 1.5.5 on 17,880 statements (TPC-H and TPC-DS queries,
examples from DuckDB's documentation, and hand-written edge cases), grebe agrees with the engine's
parser on 99.2% of statements and accepts every statement the engine
accepts.

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

## Usage

```
grebe format PATHS...    format in place; --check only reports;
                          `-` formats stdin to stdout
grebe check  PATHS...    lint; --select CODE,... runs only those MOD
                          rules, including opt-in ones;
                          --fix applies safe fixes (--unsafe widens it);
                          --json prints one JSON document
grebe rules              list every rule
grebe lsp                language server on stdio
```

Exit codes: `0` clean, `1` an error-severity finding (or `format --check`
found work), `2` usage or config error.

## Configuration

`grebe.toml`, or `[tool.grebe]` in `pyproject.toml`, found upward from the
first path (`--config PATH` reads a specific file, `--no-config` reads none):

```toml
include = ["**/*.sql"]
exclude = ["generated/**"]
# select = ["MOD001", "MOD010"]  # run only these MOD rules (opt-in ones included)
# select = ["ALL"]               # every MOD rule, default-on and opt-in alike

[severity]
MOD001 = "warning"   # error | warning | info | off; a level turns an opt-in rule on

[format]
indent_size      = 4
inline_threshold = 100
keyword_case     = "upper"
```

Silence one statement with `-- grebe: ignore[CODE]`, or every code with
`-- grebe: ignore`.

## Scope

- DuckDB only. Statements from other dialects are detected and skipped.
- No binding: grebe never resolves names against a catalog.
- Templated SQL (dbt/Jinja) is detected and skipped.
- Formatting is never a lint finding.

## License

MIT. Includes DuckDB's PEG grammar (MIT); see `NOTICE`.
