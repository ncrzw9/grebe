//! `PRS` and `SRC` detectors — the categories that fire on statements the
//! rest of the pipeline never gets a CST for.
//!
//! Unlike every `MOD` rule, these do not take a `Tree`: `PRS001` fires
//! precisely when the matcher rejects, and the `SRC` rules classify *why* a
//! statement was not analyzed. They therefore run on source text and the
//! parse verdict, not on a tree that does not exist.
//!
//! # The three classifications this module produces — and the one it does not
//!
//! `registry.rs` lists four codes in the `Prs`/`Src` categories: `PRS001`,
//! `SRC002`, `SRC003`, `SRC004`. Only the first three are decided here.
//!
//! `SRC004 unchecked-statement`'s registry message reads *"Statement type is
//! parsed but not yet linted by any MOD rule"* — a statement that has a
//! `Tree` and simply has no detector for its shape (e.g. a DDL form no MOD
//! rule inspects yet). That is a post-parse classification: it belongs
//! wherever the pipeline dispatches a successfully-parsed statement to its
//! `MOD` detectors and finds none apply, not here. [`classify_unparsed`]
//! only ever sees statements the matcher rejected, so it never emits
//! `SRC004`. `MERGE` has its own grammar production
//! (`grebe-syntax/vendor/grammar/statements/merge_into.gram`) and parses like anything
//! else.
//!
//! # Dialect scope
//!
//! grebe lints DuckDB SQL and nothing else. SQL repositories often mix
//! dialects (Postgres schema files, row-level-security policies), and
//! reporting every valid foreign statement as a syntax error would make the
//! tool unusable on them, so a foreign statement is labeled and skipped
//! instead. Path `include`/`exclude` globs are the primary way to keep known
//! foreign directories out; head detection is the safety net for what the
//! globs miss.
//!
//! Detection needs both conditions — our parser rejects the statement *and*
//! its head is a statement form DuckDB lacks. Heads alone would match valid
//! DuckDB, and dialect-flavoured type and function names (`jsonb`,
//! `timestamptz`, `now()`) are no evidence at all, since DuckDB accepts them.
//! The head table must never classify real DuckDB as foreign: a head DuckDB's
//! grammar accepts is left out even at the cost of coverage, because a
//! wrongly-skipped statement hides a real error while `PRS001` on a foreign
//! statement is merely less informative.
//!
//! # How a rejected statement is classified
//!
//! In order, first match wins:
//!
//! 1. **`SRC002` unrenderable-template** — the statement contains template
//!    delimiters (`{{ }}`, `{% %}`, `{# #}`). Detected, never rendered
//!    (dbt/Jinja is out of scope).
//! 2. **`SRC003` foreign-dialect** — the statement's leading keywords match
//!    a head DuckDB's grammar does not have at all (our parse failure ∧ a
//!    foreign head, no second engine in the loop).
//! 3. **`PRS001` syntax-error** — the fallback: a plain reject with no
//!    template markup and no foreign head. The one `Error`-severity rule in
//!    the registry. There is no separate "unclassified parse error" code:
//!    our matcher's reject *is* `PRS001`.

use grebe_syntax::Span;
use grebe_syntax::matcher::parse_check;
use grebe_syntax::token::{self, Token, TokenKind};

use crate::detect::Finding;

/// Jinja/dbt delimiter pairs. Both the open and close marker must appear
/// somewhere in the statement — a lone `{` or `}` is ordinary DuckDB struct-
/// literal syntax (`{'a': 1}`), and requiring the pair keeps that from
/// tripping this check. This only ever runs on statements that already
/// failed our parser (see [`classify_unparsed`]'s accept/reject gate), so a
/// struct literal — which parses — never reaches here in the first place;
/// the pair requirement is a second, independent line of defense.
const TEMPLATE_DELIMS: &[(&str, &str)] = &[("{{", "}}"), ("{%", "%}"), ("{#", "#}")];

fn has_template_markup(stmt: &str) -> bool {
    TEMPLATE_DELIMS
        .iter()
        .any(|(open, close)| stmt.contains(open) && stmt.contains(close))
}

/// Foreign statement heads, matched as leading-keyword sequences over our
/// own tokenizer. Each row is a contiguous sequence of leading keyword tokens.
///
/// # Excluded, on purpose
///
/// Three heads that look foreign (Postgres-style) are, per the vendored
/// grammar, real (if differently-shaped) DuckDB statements; `grebe parse`
/// accepts each example below:
///
/// - **`CREATE FUNCTION`** — `create_macro.gram`'s `MacroOrFunction` treats
///   `FUNCTION` as a synonym for `MACRO`. `CREATE FUNCTION add1(x) AS (x +
///   1)` parses. A broken DuckDB macro written with
///   the `FUNCTION` spelling would be misfiled as foreign-dialect instead of
///   `PRS001` if this head were kept.
/// - **`CREATE TRIGGER`** — `create_trigger.gram` gives DuckDB its own
///   (Postgres-incompatible) trigger grammar. `CREATE TRIGGER trg BEFORE
///   INSERT ON t INSERT INTO log VALUES (1)` parses. The head alone cannot
///   distinguish a native DuckDB trigger from a Postgres one.
/// - **`COMMENT ON FUNCTION`** — `comment.gram`'s `CommentOnType` lists
///   `CommentFunction` explicitly (`COMMENT ON FUNCTION f() IS 'x'`
///   parses). `COMMENT ON POLICY` / `COMMENT ON TRIGGER` are kept
///   below — `comment.gram` has no such variants.
///
/// Also excluded: the `CREATE INDEX` partial-index (`WHERE ...`) and
/// GIN/GiST/BRIN (`USING gin/gist/brin`) signals.
/// `create_index.gram` already carries an optional `WhereClause` and a
/// generic `IndexType <- 'USING' Identifier`, and both `CREATE INDEX idx ON
/// t USING gin (x)` and `CREATE INDEX idx ON t (x) WHERE y > 0` parse —
/// the same ambiguity as above, and unlike the three heads
/// above there is no narrower sub-signal to fall back to, so the whole
/// `CREATE INDEX` case is left as plain `PRS001` on reject.
///
/// The `ALTER TABLE` compound checks below (`ROW LEVEL SECURITY` / `OWNER
/// TO`) are kept: neither `OWNER` nor `SECURITY` appears anywhere in
/// `grebe-syntax/vendor/grammar`, and `ALTER TABLE t ENABLE ROW LEVEL
/// SECURITY` and `ALTER TABLE t OWNER TO app_role` both fail to parse.
/// `ALTER TABLE ... ADD CONSTRAINT ... CHECK` is *not* a foreign signal —
/// `create_table.gram`'s `TopCheckConstraint` makes `ALTER TABLE t ADD
/// CONSTRAINT ck CHECK (x > 0)` valid DuckDB.
const SIMPLE_FOREIGN_HEADS: &[&[&str]] = &[
    &["GRANT"],
    &["REVOKE"],
    &["CREATE", "POLICY"],
    &["ALTER", "POLICY"],
    &["DROP", "POLICY"],
    &["CREATE", "PROCEDURE"],
    &["CREATE", "OR", "REPLACE", "PROCEDURE"],
    &["CREATE", "EXTENSION"],
    &["CREATE", "ROLE"],
    &["CREATE", "PUBLICATION"],
    &["CREATE", "DOMAIN"],
    &["CREATE", "RULE"],
    &["ALTER", "DEFAULT", "PRIVILEGES"],
    &["DO"],
    &["NOTIFY"],
    &["LISTEN"],
    &["CLUSTER"],
    &["REINDEX"],
];

/// How many leading keyword tokens are worth collecting: the longest
/// built-in head (`CREATE OR REPLACE PROCEDURE`, four tokens). The compound
/// `COMMENT ON POLICY/TRIGGER` (three) and `ALTER TABLE` (two) checks fit
/// within it.
const MAX_HEAD_LEN: usize = 4;

fn token_text<'s>(stmt: &'s str, t: &Token) -> &'s str {
    &stmt[t.span.start as usize..t.span.end as usize]
}

/// The statement's leading run of `Word`-kind tokens, uppercased, up to
/// `max`. Stops at the first non-`Word` token (punctuation, a string, a
/// number) — every head pattern above is keywords only, never an
/// identifier, so a run of plain words is exactly what a head match needs.
fn leading_keywords(stmt: &str, max: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(max);
    for t in token::tokenize(stmt) {
        if t.kind.is_trivia() {
            continue;
        }
        if t.kind != TokenKind::Word {
            break;
        }
        out.push(token_text(stmt, &t).to_ascii_uppercase());
        if out.len() == max {
            break;
        }
    }
    out
}

fn matches_head(leading: &[String], pattern: &[&str]) -> bool {
    leading.len() >= pattern.len()
        && leading[..pattern.len()]
            .iter()
            .zip(pattern)
            .all(|(a, b)| a == b)
}

/// Byte span, local to `stmt`, of its first `n` leading keyword tokens —
/// the span a head-based `SRC003` finding points at.
fn head_span(stmt: &str, n: usize) -> Span {
    let mut first: Option<u32> = None;
    let mut last_end = 0u32;
    let mut seen = 0;
    for t in token::tokenize(stmt) {
        if t.kind.is_trivia() {
            continue;
        }
        if seen == n {
            break;
        }
        if first.is_none() {
            first = Some(t.span.start);
        }
        last_end = t.span.end;
        seen += 1;
    }
    Span::new(first.unwrap_or(0), last_end)
}

/// `stmt`'s non-trivia tokens, uppercased and rejoined with single spaces.
///
/// Reconstructing from tokens rather than scanning raw text means a comment
/// that happens to contain e.g. `"row level security"` cannot trip the
/// compound checks below, and it normalizes whatever whitespace/newlines
/// separate the real keywords.
fn code_only_upper(stmt: &str) -> String {
    let mut out = String::new();
    for t in token::tokenize(stmt) {
        if t.kind.is_trivia() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&token_text(stmt, &t).to_ascii_uppercase());
    }
    out
}

/// The span of the foreign head `stmt` starts (or contains, for the
/// compound `ALTER TABLE`/`COMMENT ON` cases) with, if any.
///
/// `extra` is the config's `foreign-heads` extension: each entry a
/// space-separated, uppercase keyword sequence, matched the same way as the
/// built-in table. Configuration can only add heads; the built-in ones cannot
/// be removed.
fn foreign_head(stmt: &str, extra: &[String]) -> Option<Span> {
    let longest_extra = extra
        .iter()
        .map(|h| h.split_whitespace().count())
        .max()
        .unwrap_or(0);
    let leading = leading_keywords(stmt, MAX_HEAD_LEN.max(longest_extra));

    for pattern in SIMPLE_FOREIGN_HEADS {
        if matches_head(&leading, pattern) {
            return Some(head_span(stmt, pattern.len()));
        }
    }
    for head in extra {
        let pattern: Vec<&str> = head.split_whitespace().collect();
        if !pattern.is_empty() && matches_head(&leading, &pattern) {
            return Some(head_span(stmt, pattern.len()));
        }
    }

    // ALTER TABLE ... ROW LEVEL SECURITY / OWNER TO: `ALTER TABLE` alone is
    // ordinary DuckDB DDL, so the head match needs the secondary phrase
    // elsewhere in the statement too.
    if matches_head(&leading, &["ALTER", "TABLE"]) {
        let body = code_only_upper(stmt);
        if body.contains("ROW LEVEL SECURITY") || body.contains("OWNER TO") {
            return Some(head_span(stmt, 2));
        }
    }

    // COMMENT ON POLICY / COMMENT ON TRIGGER: COMMENT ON FUNCTION (and
    // TABLE/VIEW/etc.) is real DuckDB (comment.gram), so only these two
    // object kinds count as foreign.
    if matches_head(&leading, &["COMMENT", "ON", "POLICY"])
        || matches_head(&leading, &["COMMENT", "ON", "TRIGGER"])
    {
        return Some(head_span(stmt, 3));
    }

    None
}

/// Byte span, local to `stmt`, that a `PRS001` finding points at: the
/// token our matcher had reached (`far`) when it gave up, or the tail of
/// the statement if the matcher wanted a token past the last one there is.
fn far_span(stmt: &str, far: usize) -> Span {
    let code_tokens: Vec<Token> = token::tokenize(stmt)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    match code_tokens.get(far) {
        Some(t) => t.span,
        None => match code_tokens.last() {
            Some(t) => Span::new(t.span.end, t.span.end),
            None => Span::new(0, stmt.len() as u32),
        },
    }
}

/// Classify a statement the matcher rejected.
///
/// `stmt` is the statement's own text (e.g. one span from
/// `grebe_syntax::token::split_statements`); `offset` is that span's start,
/// the byte offset of `stmt` within the file it came from. Every span on
/// the returned finding is already in file coordinates.
///
/// Returns the single finding that explains why the statement was not
/// linted: `SRC002` for template markup, `SRC003` for a foreign-dialect
/// head, or `PRS001` for a plain syntax error. If `stmt` in fact parses —
/// this function re-checks rather than trusting the caller — it returns an
/// empty `Vec`: an accepted statement is `MOD` territory, not this
/// module's.
#[must_use]
pub fn classify_unparsed(stmt: &str, offset: u32) -> Vec<Finding> {
    classify_unparsed_with(stmt, offset, &[])
}

/// [`classify_unparsed`] with the config's extra `foreign-heads`.
#[must_use]
pub fn classify_unparsed_with(stmt: &str, offset: u32, extra_heads: &[String]) -> Vec<Finding> {
    let (ok, far) = parse_check(stmt);
    if ok {
        return Vec::new();
    }

    if has_template_markup(stmt) {
        return vec![Finding {
            code: "SRC002",
            span: Span::new(offset, offset + stmt.len() as u32),
            fix: None,
        }];
    }

    if let Some(local) = foreign_head(stmt, extra_heads) {
        return vec![Finding {
            code: "SRC003",
            span: Span::new(offset + local.start, offset + local.end),
            fix: None,
        }];
    }

    let local = far_span(stmt, far);
    vec![Finding {
        code: "PRS001",
        span: Span::new(offset + local.start, offset + local.end),
        fix: None,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that `findings` is exactly one finding, with `code`.
    fn assert_single(findings: &[Finding], code: &'static str) {
        assert_eq!(
            findings.len(),
            1,
            "expected exactly one finding, got {findings:?}"
        );
        assert_eq!(findings[0].code, code);
    }

    // --- PRS001: the fallback ---

    #[test]
    fn plain_syntax_error_is_prs001() {
        let findings = classify_unparsed("SELECT 1 FROM WHERE", 0);
        assert_single(&findings, "PRS001");
    }

    #[test]
    fn prs001_span_is_offset_into_file_coordinates() {
        let stmt = "SELEKT 1";
        let offset = 100;
        let findings = classify_unparsed(stmt, offset);
        assert_single(&findings, "PRS001");
        let span = findings[0].span;
        assert!(
            span.start >= offset,
            "span {span:?} not shifted by offset {offset}"
        );
        // The span must land inside stmt's own extent once un-shifted.
        assert!(span.end - offset <= stmt.len() as u32);
    }

    #[test]
    fn prs001_span_falls_back_to_tail_when_matcher_wants_a_token_past_the_end() {
        // "SELECT * FROM" with nothing after it: the matcher wants more
        // tokens than exist (far == token count), which must not panic or
        // index out of range.
        let stmt = "SELECT * FROM";
        let findings = classify_unparsed(stmt, 0);
        assert_single(&findings, "PRS001");
        let span = findings[0].span;
        assert!(span.start as usize <= stmt.len());
        assert!(span.end as usize <= stmt.len());
    }

    #[test]
    fn accepted_statement_yields_nothing() {
        // Defensive gate: classify_unparsed re-checks rather than trusting
        // its caller. A statement that in fact parses is never PRS/SRC.
        assert!(classify_unparsed("SELECT 1", 0).is_empty());
    }

    // --- SRC002: template markup, detect-only ---

    #[test]
    fn jinja_expression_markup_is_src002() {
        let findings = classify_unparsed("SELECT * FROM {{ ref('foo') }}", 0);
        assert_single(&findings, "SRC002");
    }

    #[test]
    fn jinja_statement_markup_is_src002() {
        let findings = classify_unparsed("{% for x in range(5) %} SELECT {{ x }} {% endfor %}", 0);
        assert_single(&findings, "SRC002");
    }

    #[test]
    fn src002_never_looks_like_it_rendered_anything() {
        // The finding must not depend on evaluating the template in any
        // way -- an unresolvable Jinja expression (an undefined variable
        // and filter) still classifies cleanly without ever trying to
        // resolve `undefined_var` or `some_filter`.
        let findings = classify_unparsed("SELECT {{ undefined_var | some_filter }}", 0);
        assert_single(&findings, "SRC002");
    }

    #[test]
    fn lone_brace_is_not_template_markup() {
        // A single '{'/'}' pair is DuckDB struct-literal syntax, not
        // template markup, and must not be misfiled as SRC002. Force a
        // reject via a trailing syntax error so this exercises the
        // classifier rather than the accept-gate.
        let findings = classify_unparsed("SELECT {'a': 1} FROM WHERE", 0);
        assert_single(&findings, "PRS001");
    }

    // --- SRC003: foreign dialect, both conditions required ---

    #[test]
    fn postgres_grant_is_src003() {
        let findings = classify_unparsed("GRANT SELECT ON t TO app_role", 0);
        assert_single(&findings, "SRC003");
    }

    #[test]
    fn postgres_create_policy_is_src003() {
        let findings = classify_unparsed("CREATE POLICY p ON t USING (true)", 0);
        assert_single(&findings, "SRC003");
    }

    #[test]
    fn postgres_do_block_is_src003() {
        let findings = classify_unparsed("DO $$ BEGIN NULL; END $$", 0);
        assert_single(&findings, "SRC003");
    }

    #[test]
    fn alter_table_row_level_security_is_src003() {
        let findings = classify_unparsed("ALTER TABLE t ENABLE ROW LEVEL SECURITY", 0);
        assert_single(&findings, "SRC003");
    }

    #[test]
    fn alter_table_owner_to_is_src003() {
        let findings = classify_unparsed("ALTER TABLE t OWNER TO app_role", 0);
        assert_single(&findings, "SRC003");
    }

    #[test]
    fn comment_on_policy_is_src003() {
        let findings = classify_unparsed("COMMENT ON POLICY p ON t IS 'x'", 0);
        assert_single(&findings, "SRC003");
    }

    /// A foreign-looking head that in fact parses must produce nothing, not
    /// SRC003. `CREATE TRIGGER` looks Postgres-only, but the vendored
    /// grammar gives it a real (if differently-shaped) DuckDB meaning.
    #[test]
    fn foreign_looking_head_that_parses_fine_yields_nothing() {
        let stmt = "CREATE TRIGGER trg BEFORE INSERT ON t INSERT INTO log VALUES (1)";
        assert!(classify_unparsed(stmt, 0).is_empty());
    }

    /// `CREATE FUNCTION` is the second such head — real DuckDB (`MACRO`
    /// synonym) when it parses, so this module must never treat it as
    /// foreign even when it fails to parse (a broken macro is PRS001, not
    /// SRC003 — there is no way to know which dialect a broken one was
    /// aiming for, so the safe default wins).
    #[test]
    fn broken_create_function_is_prs001_not_src003() {
        let stmt = "CREATE FUNCTION add1(x) AS (x +";
        let findings = classify_unparsed(stmt, 0);
        assert_single(&findings, "PRS001");
    }

    /// `ALTER TABLE ... ADD CONSTRAINT ... CHECK` is real DuckDB DDL
    /// (`TopCheckConstraint`) even though it looks like a foreign
    /// signal; `ALTER TABLE` alone must not fire SRC003 just because a
    /// later, unrelated part of the statement fails to parse for some
    /// other reason.
    #[test]
    fn alter_table_add_constraint_check_is_not_src003() {
        // Malformed past the CHECK (missing closing paren) so it still
        // reaches the classifier as a reject; the point is it must not be
        // routed to SRC003 by the ADD CONSTRAINT / CHECK text alone.
        let stmt = "ALTER TABLE t ADD CONSTRAINT ck CHECK (x > 0";
        let findings = classify_unparsed(stmt, 0);
        assert_single(&findings, "PRS001");
    }

    #[test]
    fn comment_in_a_foreign_looking_statement_does_not_spoof_the_match() {
        // "row level security" only inside a comment must not trip the
        // ALTER TABLE compound check; code_only_upper drops comments.
        let stmt =
            "ALTER TABLE t -- not row level security, just a note\n ADD COLUMN x INT bogus bogus";
        let findings = classify_unparsed(stmt, 0);
        assert_single(&findings, "PRS001");
    }

    // --- classification is mutually exclusive: exactly one code, ever ---

    #[test]
    fn template_markup_wins_over_a_foreign_looking_head() {
        // A templated GRANT: SRC002 must win, since nothing about a
        // templated statement should be evaluated as if it were SQL text.
        let findings = classify_unparsed("GRANT {{ priv }} ON t TO app_role", 0);
        assert_single(&findings, "SRC002");
    }
}
