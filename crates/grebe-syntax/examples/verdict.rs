//! Differential helper: reads statements separated by NUL from stdin and
//! prints one line per statement, `1` if the matcher accepts it, else `0`.
//!
//! Pair it with an engine verdict per statement (DuckDB's Python
//! `extract_statements`, which only parses) to measure agreement after a
//! grammar refresh.
use std::io::{Read, Write};

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let mut out = String::new();
    for stmt in input.split('\0') {
        if stmt.is_empty() {
            continue;
        }
        let (ok, _) = grebe_syntax::matcher::parse_check(stmt);
        out.push_str(if ok { "1\n" } else { "0\n" });
    }
    std::io::stdout().write_all(out.as_bytes()).unwrap();
}
