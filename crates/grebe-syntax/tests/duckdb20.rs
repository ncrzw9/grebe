//! DuckDB 2.0 feature coverage.
//!
//! The vendored grammar IS a 2.0 grammar (`ce512b8`), but the dogfood corpus
//! (TPC-H/TPC-DS queries and examples mined from DuckDB's 1.x-era
//! documentation) exercises no 2.0-only syntax. These samples are transcribed
//! verbatim from the 2.0 release highlights post and close that gap.
//!
//! Three of them (`CREATE EXTENSION REPOSITORY`, `LOAD repo/ext`) are announced
//! but not shipped: engine v1.6.0-dev12831 rejects them with ParserException and
//! the grammar has no `REPOSITORY` production. We agree with the engine by
//! rejecting them, and the fixture marks them `-- expect: REJECT`. If they start
//! parsing, either the grammar was refreshed (fine, update the marker) or
//! something is over-permissive (not fine).

use grebe_syntax::matcher::parse_check;

/// Split the fixture into statements. `-- expect: REJECT` applies to the single
/// statement that follows it, not to the rest of the section.
fn statements() -> Vec<(String, bool)> {
    let src = include_str!("fixtures/duckdb20.sql");
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut expect_ok = true;
    for line in src.lines() {
        let t = line.trim();
        if t == "-- expect: REJECT" {
            expect_ok = false;
            continue;
        }
        if t.starts_with("--") && buf.trim().is_empty() {
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
        if t.ends_with(';') {
            if !buf.trim().is_empty() {
                out.push((buf.clone(), expect_ok));
            }
            buf.clear();
            expect_ok = true; // the marker covers one statement only
        }
    }
    out
}

#[test]
fn duckdb_20_samples_parse() {
    let stmts = statements();
    assert!(stmts.len() > 40, "fixture did not split: {}", stmts.len());

    let mut failed = Vec::new();
    for (sql, expect_ok) in &stmts {
        let (ok, _) = parse_check(sql);
        if ok != *expect_ok {
            let head = sql
                .lines()
                .find(|l| !l.trim().starts_with("--"))
                .unwrap_or("");
            failed.push(format!(
                "expected {}, got {}: {}",
                if *expect_ok { "parse" } else { "reject" },
                if ok { "parse" } else { "reject" },
                head.trim()
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "2.0 coverage regressions:\n  {}",
        failed.join("\n  ")
    );
}
