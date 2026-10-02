# grebe-syntax

A DuckDB SQL parser built from DuckDB's own PEG grammar: tokenizer, packrat
matcher and a lossless concrete syntax tree. No libduckdb, no database, no
external dependencies, no `unsafe`.

The grammar is vendored verbatim from DuckDB and checked statement by statement
against a real DuckDB build, with zero tolerance for SQL that DuckDB parses and
this crate rejects. It is the parser behind the
[grebe](https://crates.io/crates/grebe) formatter and linter.

```rust
use grebe_syntax::matcher::parse;

let src = "SELECT name, count(*) FROM birds GROUP BY ALL";
let tree = parse(src).expect("valid DuckDB SQL");

let tables: Vec<&str> = tree.find("TableName").iter().map(|&id| tree.text(id, src)).collect();
assert_eq!(tables, ["birds"]);
assert_eq!(tree.text(tree.root(), src), src);
```

`parse` returns `None` for SQL DuckDB would reject. The tree keeps every
token, comments and whitespace included, so `tree.text(tree.root(), src)` is
the original input byte for byte.

License: MIT
