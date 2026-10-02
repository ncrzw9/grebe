//! Suppression comments — `-- grebe: ignore[CODE, ...]`.
//!
//! Applies to every rule: a linter meant to run on real code needs an escape
//! hatch. Opinionated rules in particular need an opt-out at the site, not
//! only the project-wide `[severity] CODE = "off"` switch in config.
//!
//! The rule: a comment matching `-- grebe: ignore[CODE, ...]`
//! (bare `ignore` = all codes) **preceding the statement's first code token, or
//! on its first line**, suppresses those codes for that statement.
//!
//! This is general machinery, not MOD-specific — it suppresses `PRS001` on an
//! unparseable statement exactly as readily as `MOD001` on a `count(*)`, which
//! is the point: the statements a user most needs to silence are often the ones
//! we could not parse.
//!
//! It lives here, in `grebe-rules`, for the same reason `source::classify_unparsed`
//! does: both front ends must agree about what a file contains, and the way to
//! guarantee that is to give them one implementation rather than two that look
//! alike. `Suppressions::for_source` is computed once per analyzed buffer and
//! answers per finding.
//!
//! # Comment ownership
//!
//! The one genuinely ambiguous case is a comment sitting between two
//! statements. Given
//!
//! ```sql
//! SELECT count(*) FROM t; -- grebe: ignore[MOD001]
//! SELECT count(*) FROM u;
//! ```
//!
//! the comment precedes statement two's first code token, so a literal reading
//! of the rule above would suppress both. It is plainly a trailing comment on statement
//! one. The rule used here: **a comment that starts on the same line a previous
//! statement ended on belongs to that previous statement**, and is not
//! considered leading for the next one.

use grebe_syntax::Span;
use grebe_syntax::token::{TokenKind, split_statements, tokenize};

/// What a marker suppresses for one statement.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Mark {
    /// Bare `ignore` — every code.
    All,
    /// `ignore[...]` — these codes, uppercased.
    Codes(Vec<String>),
}

/// Every suppression in one source buffer, resolved to the statement it covers.
///
/// Build once per buffer with [`Suppressions::for_source`], then ask
/// [`Suppressions::is_suppressed`] per finding.
#[derive(Debug, Default)]
pub struct Suppressions {
    /// `(statement span, what that statement suppresses)`, in source order.
    entries: Vec<(Span, Mark)>,
}

impl Suppressions {
    /// Scan `src` for suppression comments and attach each to its statement.
    pub fn for_source(src: &str) -> Self {
        let toks = tokenize(src);
        let stmts = split_statements(src);
        if stmts.is_empty() {
            return Self::default();
        }

        let line_of = LineIndex::new(src);
        let mut entries: Vec<(Span, Mark)> = Vec::new();

        for (i, stmt) in stmts.iter().enumerate() {
            let first_line = line_of.line(stmt.start);
            // The line the previous statement ended on. A comment starting
            // there is that statement's trailing comment, never this one's
            // leading comment. `None` for the first statement -- nothing
            // precedes it to take ownership.
            let prev_end_line = i.checked_sub(1).map(|p| line_of.line(stmts[p].end));

            let mut marks: Vec<Mark> = Vec::new();
            for t in &toks {
                if !matches!(t.kind, TokenKind::LineComment | TokenKind::BlockComment) {
                    continue;
                }
                let cline = line_of.line(t.span.start);
                let applies = if cline == first_line {
                    // "on its first line" -- inside the statement or trailing
                    // it, as long as it shares the first code token's line.
                    true
                } else {
                    // "preceding the statement's first code token", on a line
                    // of its own that the previous statement does not own.
                    t.span.start < stmt.start && prev_end_line.is_none_or(|p| cline > p)
                };
                if !applies {
                    continue;
                }
                let text = &src[t.span.start as usize..t.span.end as usize];
                if let Some(m) = parse_marker(text) {
                    marks.push(m);
                }
            }

            for m in marks {
                entries.push((*stmt, m));
            }
        }

        Self { entries }
    }

    /// Is `code` suppressed for the statement containing byte `offset`?
    ///
    /// `offset` is a file-absolute finding start. Both front ends offset their
    /// per-statement spans back into file coordinates before reporting, so the
    /// same call works on the whole-file and per-statement paths alike.
    pub fn is_suppressed(&self, code: &str, offset: u32) -> bool {
        self.entries.iter().any(|(span, mark)| {
            if offset < span.start || offset >= span.end {
                return false;
            }
            match mark {
                Mark::All => true,
                Mark::Codes(codes) => codes.iter().any(|c| c.eq_ignore_ascii_case(code)),
            }
        })
    }

    /// True when the buffer carries no suppressions at all — the common case,
    /// and worth checking before looping over findings.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Parse one comment's text into a marker, if it is one.
///
/// Accepts both comment syntaxes and is liberal about internal whitespace
/// (`--grebe:ignore[X]` and `-- grebe: ignore [X]` both parse). That is
/// tolerance in a text format, not a second spelling of the interface: there is
/// exactly one marker, `grebe: ignore`, and nothing else is recognised.
fn parse_marker(text: &str) -> Option<Mark> {
    let body = match text.strip_prefix("--") {
        Some(rest) => rest,
        None => {
            let rest = text.strip_prefix("/*")?;
            rest.strip_suffix("*/").unwrap_or(rest)
        }
    };

    let body = body.trim();
    let rest = strip_prefix_ci(body, "grebe")?.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = strip_prefix_ci(rest, "ignore")?.trim_start();

    // Bare `ignore` -- every code.
    if rest.is_empty() {
        return Some(Mark::All);
    }

    let inner = rest.strip_prefix('[')?;
    // An unterminated `ignore[...` is a typo, not a suppression. Saying nothing
    // would silence rules the author never listed.
    let inner = inner.split_once(']')?.0;

    let codes: Vec<String> = inner
        .split(',')
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| !c.is_empty())
        .collect();

    // `ignore[]` suppresses nothing, which is almost certainly not what was
    // meant -- but inventing "all" from an empty list would be worse.
    if codes.is_empty() {
        return None;
    }
    Some(Mark::Codes(codes))
}

/// `strip_prefix`, case-insensitively, for ASCII keywords.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Byte offset -> 0-based line, precomputed so attaching N comments to M
/// statements does not rescan the buffer N*M times.
struct LineIndex {
    /// Start offset of each line.
    starts: Vec<u32>,
}

impl LineIndex {
    fn new(src: &str) -> Self {
        let mut starts = vec![0u32];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i as u32 + 1);
            }
        }
        Self { starts }
    }

    fn line(&self, offset: u32) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(src: &str) -> Suppressions {
        Suppressions::for_source(src)
    }

    #[test]
    fn parses_a_code_list() {
        assert_eq!(
            parse_marker("-- grebe: ignore[MOD001, MOD002]"),
            Some(Mark::Codes(vec!["MOD001".into(), "MOD002".into()]))
        );
    }

    #[test]
    fn bare_ignore_is_all_codes() {
        assert_eq!(parse_marker("-- grebe: ignore"), Some(Mark::All));
    }

    #[test]
    fn tolerates_whitespace_and_case_and_block_comments() {
        for t in [
            "--grebe:ignore[mod001]",
            "--   GREBE :  Ignore [ mod001 ]",
            "/* grebe: ignore[MOD001] */",
        ] {
            assert_eq!(
                parse_marker(t),
                Some(Mark::Codes(vec!["MOD001".into()])),
                "failed on {t}"
            );
        }
    }

    #[test]
    fn ordinary_comments_are_not_markers() {
        for t in [
            "-- just a note",
            "-- grebe is a linter",
            "-- grebe: check this later",
            "-- otherlint: ignore[MOD001]", // another tool's marker
            "-- grebe: ignore[MOD001",      // unterminated
            "-- grebe: ignore[]",
        ] {
            assert_eq!(parse_marker(t), None, "wrongly parsed {t}");
        }
    }

    #[test]
    fn leading_comment_suppresses_its_statement() {
        let src = "-- grebe: ignore[MOD001]\nSELECT count(*) FROM t;";
        let s = marks(src);
        let at = src.find("count").unwrap() as u32;
        assert!(s.is_suppressed("MOD001", at));
        assert!(!s.is_suppressed("MOD002", at));
    }

    #[test]
    fn first_line_comment_suppresses_its_statement() {
        let src = "SELECT count(*) FROM t; -- grebe: ignore[MOD001]";
        let s = marks(src);
        assert!(s.is_suppressed("MOD001", src.find("count").unwrap() as u32));
    }

    #[test]
    fn bare_ignore_covers_every_code_including_prs001() {
        let src = "-- grebe: ignore\nTHIS IS NOT SQL (((;";
        let s = marks(src);
        assert!(s.is_suppressed("PRS001", src.find("THIS").unwrap() as u32));
    }

    #[test]
    fn a_trailing_comment_does_not_leak_into_the_next_statement() {
        // The whole reason comment ownership needs a rule: this marker is
        // trailing on statement one, not leading on statement two.
        let src = "SELECT count(*) FROM t; -- grebe: ignore[MOD001]\nSELECT count(*) FROM u;";
        let s = marks(src);
        let first = src.find("count").unwrap() as u32;
        let second = src.rfind("count").unwrap() as u32;
        assert!(s.is_suppressed("MOD001", first));
        assert!(!s.is_suppressed("MOD001", second));
    }

    #[test]
    fn a_comment_on_its_own_line_does_lead_the_next_statement() {
        let src = "SELECT count(*) FROM t;\n-- grebe: ignore[MOD001]\nSELECT count(*) FROM u;";
        let s = marks(src);
        assert!(!s.is_suppressed("MOD001", src.find("count").unwrap() as u32));
        assert!(s.is_suppressed("MOD001", src.rfind("count").unwrap() as u32));
    }

    #[test]
    fn suppression_does_not_reach_beyond_its_own_statement() {
        let src = "-- grebe: ignore[MOD001]\nSELECT count(*) FROM t;\nSELECT count(*) FROM u;";
        let s = marks(src);
        assert!(s.is_suppressed("MOD001", src.find("count").unwrap() as u32));
        assert!(!s.is_suppressed("MOD001", src.rfind("count").unwrap() as u32));
    }

    #[test]
    fn a_semicolon_inside_a_comment_does_not_split_statements() {
        let src = "-- grebe: ignore[MOD001] ; not a split\nSELECT count(*) FROM t;";
        let s = marks(src);
        assert!(s.is_suppressed("MOD001", src.find("count").unwrap() as u32));
    }

    #[test]
    fn no_comments_means_empty() {
        assert!(marks("SELECT count(*) FROM t;").is_empty());
    }

    #[test]
    fn line_index_maps_offsets_to_lines() {
        let idx = LineIndex::new("a\nbb\n\nccc");
        assert_eq!(idx.line(0), 0);
        assert_eq!(idx.line(2), 1);
        assert_eq!(idx.line(5), 2);
        assert_eq!(idx.line(6), 3);
    }
}
