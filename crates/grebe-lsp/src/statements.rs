//! `grebe/statements`: which statements a "run" means.
//!
//! The editor runs SQL through the user's `duckdb` CLI one statement at a
//! time, because the CLI's JSON output carries no statement tag: results
//! only pair with statements when they are sent singly. The split has to be
//! the tokenizer's, not a client-side regex, because only the tokenizer
//! knows that a `;` inside a string, a comment or a dollar-quoted body is not
//! a boundary.
//!
//! Spans come from [`grebe_syntax::token::split_statements`]: each starts at
//! its first code token (leading comments stay behind) and includes its `;`.

use grebe_syntax::Span;
use grebe_syntax::token::{split_statements, tokenize};

/// The statements a run over `range` should execute, in source order.
///
/// - A non-empty range runs every statement it overlaps.
/// - An empty range (a cursor) runs the statement containing it; a cursor
///   in the gap after a statement runs the statement just before it, which
///   is where a cursor sits after typing `;`.
///
/// A statement that is only `;` is dropped: there is nothing to run.
#[must_use]
pub fn select(src: &str, range: Span) -> Vec<Span> {
    let all: Vec<Span> = split_statements(src)
        .into_iter()
        .filter(|&s| has_code_before_semicolon(src, s))
        .collect();
    if range.start < range.end {
        return all
            .into_iter()
            .filter(|s| s.start < range.end && range.start < s.end)
            .collect();
    }
    let at = range.start;
    if let Some(&s) = all.iter().find(|s| s.start <= at && at <= s.end) {
        return vec![s];
    }
    all.iter()
        .rev()
        .find(|s| s.end <= at)
        .or_else(|| all.first())
        .map(|&s| vec![s])
        .unwrap_or_default()
}

/// Every statement in the document.
#[must_use]
pub fn all(src: &str) -> Vec<Span> {
    let len = u32::try_from(src.len()).unwrap_or(u32::MAX);
    select(src, Span::new(0, len))
}

/// What kind of statement `stmt` is, from its parse: `select` for anything
/// that returns rows as a query (plain `SELECT`, `WITH`, FROM-first,
/// `VALUES`, `PIVOT`, `DESCRIBE`, `SUMMARIZE`, `SHOW` -- the grammar files
/// all of these under `SelectStatement`), otherwise the statement rule's
/// name without `Statement`, lowercased (`insert`, `create`, `copy`, ...).
/// `unknown` when the statement does not parse on its own.
///
/// The editor offers column types and export only for `select`: both work by
/// running the query again (`DESCRIBE ...`, `COPY (...) TO`), which is only
/// harmless for a statement that changes nothing.
#[must_use]
pub fn kind(stmt: &str) -> String {
    let Some(tree) = grebe_syntax::matcher::parse(stmt) else {
        return "unknown".into();
    };
    let rule = tree
        .find("Statement")
        .first()
        .and_then(|&s| tree.children(s).first().copied())
        .map_or("", |c| tree.rule_name(c));
    match rule {
        "" => "unknown".into(),
        "SelectStatement" => "select".into(),
        other => other
            .strip_suffix("Statement")
            .unwrap_or(other)
            .to_ascii_lowercase(),
    }
}

fn has_code_before_semicolon(src: &str, s: Span) -> bool {
    let text = &src[s.start as usize..s.end as usize];
    tokenize(text)
        .iter()
        .any(|t| !t.kind.is_trivia() && &text[t.span.start as usize..t.span.end as usize] != ";")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str, spans: &[Span]) -> Vec<String> {
        spans
            .iter()
            .map(|s| src[s.start as usize..s.end as usize].to_string())
            .collect()
    }

    const SRC: &str = "-- setup\nCREATE TABLE t AS SELECT 1 AS a;\n\
                       SELECT ';' AS s FROM t;\n;\nSELECT a FROM t";

    #[test]
    fn a_whole_document_splits_on_real_semicolons_only() {
        let got = texts(SRC, &all(SRC));
        assert_eq!(
            got,
            [
                "CREATE TABLE t AS SELECT 1 AS a;",
                "SELECT ';' AS s FROM t;",
                "SELECT a FROM t",
            ]
        );
    }

    #[test]
    fn a_cursor_runs_the_statement_it_is_in() {
        let at = SRC.find("';'").unwrap() as u32;
        assert_eq!(
            texts(SRC, &select(SRC, Span::new(at, at))),
            ["SELECT ';' AS s FROM t;"]
        );
    }

    #[test]
    fn a_cursor_after_a_semicolon_runs_the_statement_before_it() {
        let at = (SRC.find("AS a;").unwrap() + "AS a;".len()) as u32;
        assert_eq!(
            texts(SRC, &select(SRC, Span::new(at, at))),
            ["CREATE TABLE t AS SELECT 1 AS a;"]
        );
    }

    #[test]
    fn a_cursor_in_a_leading_comment_runs_the_first_statement() {
        assert_eq!(
            texts(SRC, &select(SRC, Span::new(2, 2))),
            ["CREATE TABLE t AS SELECT 1 AS a;"]
        );
    }

    #[test]
    fn a_selection_runs_every_statement_it_touches() {
        let start = SRC.find("SELECT 1").unwrap() as u32;
        let end = SRC.find("FROM t;").unwrap() as u32;
        assert_eq!(
            texts(SRC, &select(SRC, Span::new(start, end))),
            [
                "CREATE TABLE t AS SELECT 1 AS a;",
                "SELECT ';' AS s FROM t;"
            ]
        );
    }

    #[test]
    fn kinds_come_from_the_parse() {
        for (sql, want) in [
            ("SELECT 1;", "select"),
            ("WITH c AS (SELECT 1) SELECT * FROM c", "select"),
            ("FROM t", "select"),
            ("SUMMARIZE t;", "select"),
            ("PIVOT t ON a USING sum(b)", "select"),
            ("INSERT INTO t SELECT 1;", "insert"),
            ("CREATE TABLE t AS SELECT 1", "create"),
            ("COPY t TO 'x.csv'", "copy"),
            ("DELETE FROM t", "delete"),
            ("EXPLAIN SELECT 1", "explain"),
            ("SELEC 1", "unknown"),
        ] {
            assert_eq!(kind(sql), want, "{sql}");
        }
    }

    #[test]
    fn nothing_to_run_is_empty() {
        assert!(all("").is_empty());
        assert!(all("-- only a comment\n;").is_empty());
    }
}
