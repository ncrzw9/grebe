//! The formatter: a Wadler/Prettier `Doc` IR over the CST.
//!
//! # Why we compete rather than wrap
//!
//! DuckDB ships `src/parser/peg/sql_formatter.cpp` (1,644 lines). It is a
//! **token-stream** pretty-printer — the only symbol it uses from
//! `compiled_grammar.hpp` is `MatcherToken`, and it has no tree. It emits a
//! multiline string in a first pass, then re-splits and repairs that string
//! across six more passes.
//!
//! Two reasons settle it: a line-string rewriter structurally
//! cannot do measure-based line breaking — deciding a break by whether a whole
//! subtree fits the remaining width — and the C++ is behind no SQL function and
//! no C API symbol, so a binary that does not link libduckdb cannot call it.
//!
//! What we do take is its config surface, which is the entire knob budget:
//!
//! | knob | default |
//! |---|---|
//! | `indent_size` | 4 |
//! | `inline_threshold` | 100 |
//! | `keyword_case` | UPPER |
//!
//! To those three it adds three of its own, each about one character of
//! space, for houses whose style differs from the default there and
//! nowhere else:
//!
//! | knob | default |
//! |---|---|
//! | `function_paren_space` | false: `count(x)` |
//! | `keyword_paren_space` | true: `IN (1, 2)` |
//! | `semicolon` | attached: `ORDER BY n;` |
//!
//! No other knob exists: the point of one canonical style is that nobody
//! has to configure it.
//!
//! # Specification
//!
//! Black/ruff-format posture: one canonical style, near-zero knobs. The
//! rules live in `lower`; in summary:
//!
//! - **Case:** keywords follow `keyword_case`; function and type names are
//!   lower; identifiers are never re-cased, because their case is meaning.
//! - **Statements:** a statement that fits the threshold stays on one line;
//!   otherwise every clause (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, `LIMIT`,
//!   ...) starts a line, flush left within its query level. There is no
//!   middle ground.
//! - **Lists:** inline until they do not fit, then one item per line,
//!   indented one level; commas trail, never lead. A trailing comma the
//!   source had is kept in a broken list and dropped in a flat one; one is
//!   never added.
//! - **Indentation:** spaces only, `indent_size` per level; brackets and
//!   subqueries indent one level from their opening paren, the closing paren
//!   returns to the opening line's indent.
//! - **Expressions:** single spaces around binary operators, none before the
//!   `(` of a call; `AND`/`OR` and operator chains break before the
//!   operator, one operand per line.
//! - **Joins** sit at clause level; `ON`/`USING` trails its join until it
//!   does not fit, then indents under it.
//! - **CTEs:** one per line, bodies indented, a blank line before the main
//!   query.
//! - **Blank lines:** exactly one between statements and one after the last
//!   CTE; input blank lines are not preserved. Every statement ends with `;`
//!   and the file with a single newline.
//! - **Width** is measured in characters, not display columns.
//!
//! # Invariants (tests, not aspirations)
//!
//! - **Idempotent:** `format(format(x)) == format(x)` over the whole corpus.
//! - **Only whitespace changes:** no code token is dropped, added or
//!   reordered, with three exceptions: case (keywords, function and type
//!   names), a list's trailing comma
//!   (kept or dropped, never added), and the statement terminator (added when
//!   missing). Literals, quoting, casts and friendly-SQL forms are laid out
//!   as written; rewriting them is a lint fix, not layout.
//! - **Output still parses:** when the input parses, the formatted output
//!   re-parses to a CST of the same shape. A statement that does not parse
//!   is emitted verbatim, never "repaired".
//! - **Comments survive.** Attachment is the hard part and the place
//!   losslessness earns its keep.
//! - **Never a diagnostic.** No layout rule ever appears as a lint finding.
//!
//! # Layers
//!
//! - [`doc`] — the IR and its printer. Knows nothing about SQL.
//! - [`lower`] — CST → `Doc`. Knows nothing about widths.
//! - [`format`] — the entry point: parse, lower, print, one top-level
//!   statement at a time, so a large file costs its largest statement and
//!   not its size; and when a file does not parse as a whole, do that per
//!   statement and leave what we cannot parse exactly as it was.

pub mod doc;
mod lower;

/// How keywords are cased. Identifiers are never re-cased; function and type
/// names are always lower, whatever this says. Upper is the default because
/// it is DuckDB's own documentation style.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum KeywordCase {
    #[default]
    Upper,
    Lower,
}

/// Where a statement's terminating `;` goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Semicolon {
    /// Right after the last token: `ORDER BY n;`.
    #[default]
    Attached,
    /// After a space: `ORDER BY n ;`.
    Spaced,
    /// On a line of its own, after the statement.
    Newline,
}

/// The whole knob budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Spaces per indent level.
    pub indent_size: usize,
    /// A construct that fits within this many columns stays on one line.
    pub inline_threshold: usize,
    pub keyword_case: KeywordCase,
    /// A space between a call's name and its `(`: `count (x)`. A call is a
    /// function, aggregate, window, table function or method call, a macro
    /// definition's parameter list, and the keyword forms that read as
    /// calls (`CAST`, `TRY_CAST`, `COALESCE`, `NULLIF`, `EXTRACT`,
    /// `POSITION`, `SUBSTRING`, `TRIM`, `OVERLAY`, `ROW`, `GROUPING`,
    /// `COLUMNS`, `UNPACK`, `TRY`, `CUBE`, `ROLLUP`). Type modifiers
    /// (`decimal(18, 2)`) are not calls and always hug.
    pub function_paren_space: bool,
    /// A space between a keyword and a `(` that follows it: `IN (1, 2)`,
    /// `VALUES (1)`, `OVER (...)`, `AS (...)`, `EXISTS (...)`, `FROM (...)`.
    /// Off, every such parenthesis hugs its keyword. The keyword forms
    /// that read as calls belong to `function_paren_space`, not here.
    pub keyword_paren_space: bool,
    pub semicolon: Semicolon,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            indent_size: 4,
            inline_threshold: 100,
            keyword_case: KeywordCase::Upper,
            function_paren_space: false,
            keyword_paren_space: true,
            semicolon: Semicolon::Attached,
        }
    }
}

/// Format a whole file. The result ends with exactly one newline, or is
/// empty when the input holds no code and no comments.
///
/// A file that does not parse as a whole is formatted statement by
/// statement; a statement the matcher rejects is emitted exactly as written
/// (trimmed), so one bad statement never costs the rest of the file its
/// layout, and never gets "fixed" into something else.
#[must_use]
pub fn format(src: &str, opts: &Options) -> String {
    // A byte order mark stays where it is: it says how the file is encoded,
    // not anything about the SQL.
    if let Some(rest) = src.strip_prefix('\u{feff}') {
        return format!("\u{feff}{}", format(rest, opts));
    }
    if let Some(whole) = lower::format_statements(src, opts) {
        return finish(whole.text);
    }
    let spans = grebe_syntax::token::split_statements(src);
    let mut pieces: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    // A piece that holds no statement -- a bare `;` with comments around it
    // -- is carried into the next piece rather than formatted alone, so its
    // comments land where the whole-file path (and re-formatting the output)
    // would put them: leading the statement that follows.
    let mut carry = 0usize;
    for sp in &spans {
        // A piece runs from the end of the previous one to the end of this
        // statement's line: the trivia before it, so its leading comments
        // format with it, and the rest of the `;` line, so a comment
        // trailing the terminator stays with the statement it trails.
        let end = end_of_line(src, sp.end as usize);
        let piece = &src[carry..end];
        cursor = end;
        match lower::format_statements(piece, opts) {
            Some(formatted) => {
                if !formatted.has_statement {
                    continue; // carry forward; `carry` stays put
                }
                if !formatted.text.is_empty() {
                    pieces.push(formatted.text);
                }
            }
            None => {
                let raw = piece.trim();
                if !raw.is_empty() {
                    pieces.push(raw.to_string());
                }
            }
        }
        carry = end;
    }
    if carry < cursor {
        // Trailing statement-less pieces: comments after the last statement.
        let piece = &src[carry..cursor];
        if let Some(formatted) = lower::format_statements(piece, opts) {
            if !formatted.text.is_empty() {
                pieces.push(formatted.text);
            }
        } else if !piece.trim().is_empty() {
            pieces.push(piece.trim().to_string());
        }
    }
    let tail = src[cursor..].trim();
    if !tail.is_empty() {
        pieces.push(tail.to_string());
    }
    finish(pieces.join("\n\n"))
}

/// The offset just past the newline that ends the line containing `at`, or
/// the end of the buffer.
fn end_of_line(src: &str, at: usize) -> usize {
    src[at..].find('\n').map_or(src.len(), |i| at + i + 1)
}

fn finish(mut s: String) -> String {
    while s.ends_with('\n') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() {
        return s;
    }
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(sql: &str) -> String {
        format(sql, &Options::default())
    }

    fn narrow(sql: &str, width: usize) -> String {
        format(
            sql,
            &Options {
                inline_threshold: width,
                ..Options::default()
            },
        )
    }

    #[test]
    fn a_short_statement_stays_on_one_line_and_gets_a_semicolon() {
        assert_eq!(
            f("select a, b from t where x = 1"),
            "SELECT a, b FROM t WHERE x = 1;\n"
        );
    }

    #[test]
    fn keywords_upper_functions_lower_identifiers_untouched() {
        assert_eq!(
            f("Select COUNT(*) As Total, MyCol from MyTable"),
            "SELECT count(*) AS Total, MyCol FROM MyTable;\n"
        );
    }

    #[test]
    fn an_unreserved_keyword_used_as_a_name_keeps_its_case() {
        assert_eq!(
            f("select data, value from Data"),
            "SELECT data, value FROM Data;\n"
        );
    }

    #[test]
    fn types_are_lower_cased() {
        assert_eq!(
            f("select cast(x as INTEGER), y::VARCHAR, z::DECIMAL(18, 2) from t"),
            "SELECT CAST(x AS integer), y::varchar, z::decimal(18, 2) FROM t;\n"
        );
    }

    #[test]
    fn clauses_start_lines_once_broken() {
        let sql =
            "select alpha, beta, gamma from some_table where alpha > 1 group by all order by 1";
        assert_eq!(
            narrow(sql, 40),
            "SELECT alpha, beta, gamma\nFROM some_table\nWHERE alpha > 1\nGROUP BY ALL\nORDER BY 1;\n"
        );
    }

    #[test]
    fn a_long_select_list_breaks_one_per_line_with_the_trailing_comma_it_had() {
        let sql = "select alpha_column, beta_column, gamma_column, from t";
        assert_eq!(
            narrow(sql, 30),
            "SELECT\n    alpha_column,\n    beta_column,\n    gamma_column,\nFROM t;\n"
        );
        // Without a trailing comma in the source, none is added.
        let sql = "select alpha_column, beta_column, gamma_column from t";
        assert_eq!(
            narrow(sql, 30),
            "SELECT\n    alpha_column,\n    beta_column,\n    gamma_column\nFROM t;\n"
        );
    }

    #[test]
    fn the_comma_of_a_one_element_tuple_is_kept() {
        // `(42,)` is a tuple, `(42)` is the number: dropping the comma
        // would change what the query returns.
        assert_eq!(
            f("select (42,), typeof((42,)), (1, 2,), f(1,)"),
            "SELECT (42,), typeof((42,)), (1, 2), f(1);\n"
        );
    }

    #[test]
    fn a_trailing_comma_is_dropped_when_the_list_collapses() {
        assert_eq!(f("select a, b, from t"), "SELECT a, b FROM t;\n");
    }

    #[test]
    fn and_chains_break_operator_leading() {
        let sql = "select * from t where alpha = 1 and beta = 2 and gamma = 3";
        assert_eq!(
            narrow(sql, 30),
            "SELECT *\nFROM t\nWHERE alpha = 1\n    AND beta = 2\n    AND gamma = 3;\n"
        );
    }

    #[test]
    fn joins_sit_at_clause_level() {
        let sql = "select * from orders o join customers c on o.cid = c.id left join x using (k) where 1 = 1";
        assert_eq!(
            narrow(sql, 40),
            "SELECT *\nFROM orders o\nJOIN customers c ON o.cid = c.id\nLEFT JOIN x USING (k)\nWHERE 1 = 1;\n"
        );
    }

    #[test]
    fn ctes_get_their_own_lines_and_a_blank_line_before_the_query() {
        let sql = "with a as (select 1 as x from t), b as (select x from a) select * from b";
        assert_eq!(
            narrow(sql, 40),
            "WITH a AS (\n    SELECT 1 AS x\n    FROM t\n),\nb AS (\n    SELECT x\n    FROM a\n)\n\nSELECT *\nFROM b;\n"
        );
    }

    #[test]
    fn subqueries_break_their_clauses_when_they_break() {
        let sql = "select * from (select alpha, beta from some_table where alpha > 1) s";
        assert_eq!(
            narrow(sql, 40),
            "SELECT *\nFROM (\n    SELECT alpha, beta\n    FROM some_table\n    WHERE alpha > 1\n) s;\n"
        );
    }

    #[test]
    fn case_breaks_one_arm_per_line() {
        let sql =
            "select case when a = 1 then 'one' when a = 2 then 'two' else 'many' end as n from t";
        assert_eq!(
            narrow(sql, 40),
            "SELECT\n    CASE\n        WHEN a = 1 THEN 'one'\n        WHEN a = 2 THEN 'two'\n        ELSE 'many'\n    END AS n\nFROM t;\n"
        );
    }

    #[test]
    fn set_operations_sit_at_clause_level() {
        let sql = "select a from t union all select b from u";
        assert_eq!(
            narrow(sql, 20),
            "SELECT a\nFROM t\nUNION ALL\nSELECT b\nFROM u;\n"
        );
    }

    /// `FROM t SELECT ...` is the same query as `SELECT ... FROM t` and gets
    /// the same layout: one line while it fits, and once it does not, a
    /// line per clause, in the order the clauses were written.
    #[test]
    fn a_from_first_query_lays_out_like_its_select_first_form() {
        let from_first = "from t select a, b where a > 1 group by all order by 1;";
        let select_first = "select a, b from t where a > 1 group by all order by 1;";
        assert_eq!(
            f(from_first),
            "FROM t SELECT a, b WHERE a > 1 GROUP BY ALL ORDER BY 1;\n"
        );
        assert_eq!(
            f(select_first),
            "SELECT a, b FROM t WHERE a > 1 GROUP BY ALL ORDER BY 1;\n"
        );
        assert_eq!(
            narrow(from_first, 40),
            "FROM t\nSELECT a, b\nWHERE a > 1\nGROUP BY ALL\nORDER BY 1;\n"
        );
        assert_eq!(
            narrow(select_first, 40),
            "SELECT a, b\nFROM t\nWHERE a > 1\nGROUP BY ALL\nORDER BY 1;\n"
        );
        assert_eq!(f("from t"), "FROM t;\n");
        assert_eq!(narrow("from t", 3), "FROM t;\n");
    }

    /// The two forms differ in where the `FROM` block stands and in nothing
    /// else: the same lines, at every width, joins and wrapped lists
    /// included.
    #[test]
    fn from_first_and_select_first_break_into_the_same_lines() {
        let from = "from orders o join customers c on o.cid = c.id left join x using (k)";
        let select = "select alpha_column, beta_column, count(*) as n";
        let rest = "where alpha_column > 1 and beta_column < 100 group by all having count(*) > 2 qualify row_number() over (partition by alpha_column order by beta_column) = 1 order by 1 limit 10";
        for (from, select) in [
            (from, select),
            ("from t", "select a"),
            ("from tbl_one, tbl_two, tbl_three, tbl_four", select),
            (
                "from (select a, b from u where a > 1) s",
                "select distinct a, b",
            ),
        ] {
            for w in [20, 30, 40, 60, 100, 400] {
                let from_first = narrow(&format!("{from} {select} {rest}"), w);
                let select_first = narrow(&format!("{select} {from} {rest}"), w);
                let lines = |s: &str| {
                    let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
                    lines.sort();
                    lines
                };
                if from_first.lines().count() > 1 {
                    assert_eq!(
                        lines(&from_first),
                        lines(&select_first),
                        "width {w}:\n{from_first}\n{select_first}"
                    );
                } else {
                    assert_eq!(
                        select_first.lines().count(),
                        1,
                        "width {w}:\n{select_first}"
                    );
                }
                assert_eq!(narrow(&from_first, w), from_first, "width {w}");
            }
        }
    }

    #[test]
    fn from_first_queries_break_wherever_a_query_can_stand() {
        // A CTE body and the main query.
        assert_eq!(
            narrow(
                "with c as (from t select a, b where a > 1) from c select a",
                30
            ),
            "WITH c AS (\n    FROM t\n    SELECT a, b\n    WHERE a > 1\n)\n\nFROM c\nSELECT a;\n"
        );
        // A subquery, in a FROM-first query and in a SELECT-first one.
        assert_eq!(
            narrow("from (from t select a, b where a > 1) select a", 30),
            "FROM (\n    FROM t\n    SELECT a, b\n    WHERE a > 1\n)\nSELECT a;\n"
        );
        assert_eq!(
            narrow(
                "select * from (from t select a, b where a > 1 order by 1) s",
                30
            ),
            "SELECT *\nFROM (\n    FROM t\n    SELECT a, b\n    WHERE a > 1\n    ORDER BY 1\n) s;\n"
        );
        // Members of a set operation.
        assert_eq!(
            narrow("from t select a union all from u select b", 20),
            "FROM t\nSELECT a\nUNION ALL\nFROM u\nSELECT b;\n"
        );
        // CREATE TABLE AS, CREATE VIEW, INSERT, COPY.
        assert_eq!(
            narrow("create table x as from t select a, b where a > 1", 30),
            "CREATE TABLE x AS\nFROM t\nSELECT a, b\nWHERE a > 1;\n"
        );
        assert_eq!(
            narrow("create view v as from t select a, b where a > 1", 30),
            "CREATE VIEW v AS\nFROM t\nSELECT a, b\nWHERE a > 1;\n"
        );
        assert_eq!(
            narrow("insert into x from t select a, b where a > 1", 30),
            "INSERT INTO x\nFROM t\nSELECT a, b\nWHERE a > 1;\n"
        );
        assert_eq!(
            narrow("copy (from t select a, b where a > 1) to 'x.csv'", 30),
            "COPY (\n    FROM t\n    SELECT a, b\n    WHERE a > 1\n) TO 'x.csv';\n"
        );
        // Joins at clause level, a wrapped select list, a broken AND chain.
        assert_eq!(
            narrow(
                "from orders o join customers c on o.cid = c.id select alpha_column, beta_column, gamma_column where alpha_column = 1 and beta_column = 2",
                34
            ),
            "FROM orders o\nJOIN customers c ON o.cid = c.id\nSELECT\n    alpha_column,\n    beta_column,\n    gamma_column\nWHERE alpha_column = 1\n    AND beta_column = 2;\n"
        );
        // No select list at all.
        assert_eq!(
            narrow("from t where alpha = 1 and beta = 2 and gamma = 3", 30),
            "FROM t\nWHERE alpha = 1\n    AND beta = 2\n    AND gamma = 3;\n"
        );
    }

    const CALLS_AND_KEYWORDS: &str = "select count(*), coalesce(a, b), cast(x as decimal(18, 2)), x.lower(), sum(a) filter (where a > 1) over (partition by b) from read_csv('f.csv') t join u using (k) where a in (1, 2) and exists (select 1) and not (a > 1) group by rollup (a, b)";

    #[test]
    fn function_paren_space_sets_calls_off_and_leaves_types_and_keywords() {
        let opts = Options {
            function_paren_space: true,
            inline_threshold: 400,
            ..Options::default()
        };
        assert_eq!(
            format(CALLS_AND_KEYWORDS, &opts),
            "SELECT count (*), COALESCE (a, b), CAST (x AS decimal(18, 2)), x.lower (), sum (a) FILTER (WHERE a > 1) OVER (PARTITION BY b) FROM read_csv ('f.csv') t JOIN u USING (k) WHERE a IN (1, 2) AND EXISTS (SELECT 1) AND NOT (a > 1) GROUP BY ROLLUP (a, b);\n"
        );
        // A macro's parameter list is its call, spelled once; a CTE's
        // column list and a column list after a table name are not calls.
        assert_eq!(
            format(
                "create macro add_one(a) as a + 1; with c(n) as (select 1) select * from c; insert into t (a) values (1)",
                &opts
            ),
            "CREATE MACRO add_one (a) AS a + 1;\n\nWITH c(n) AS (SELECT 1) SELECT * FROM c;\n\nINSERT INTO t (a) VALUES (1);\n"
        );
        // An argument list that breaks still opens on the name's line.
        let narrow = Options {
            inline_threshold: 20,
            ..opts
        };
        assert_eq!(
            format("select coalesce(alpha_column, beta_column)", &narrow),
            "SELECT\n    COALESCE (\n        alpha_column,\n        beta_column\n    );\n"
        );
    }

    #[test]
    fn keyword_paren_space_off_hugs_every_parenthesis_after_a_keyword() {
        let opts = Options {
            keyword_paren_space: false,
            inline_threshold: 400,
            ..Options::default()
        };
        assert_eq!(
            format(CALLS_AND_KEYWORDS, &opts),
            "SELECT count(*), COALESCE(a, b), CAST(x AS decimal(18, 2)), x.lower(), sum(a) FILTER(WHERE a > 1) OVER(PARTITION BY b) FROM read_csv('f.csv') t JOIN u USING(k) WHERE a IN(1, 2) AND EXISTS(SELECT 1) AND NOT(a > 1) GROUP BY ROLLUP(a, b);\n"
        );
        // The clause keywords too; a parenthesis after a name is not one
        // after a keyword, and an operator's is not either.
        assert_eq!(
            format(
                "with c(n) as (select 1) select (n + 1) * 2 from (select n from c) s where (n > 1) order by (n); insert into t (a, b) values (1, 2), (3, 4); select case (a) when (1) then (2) end, 1 + (2)",
                &opts
            ),
            "WITH c(n) AS(SELECT 1) SELECT(n + 1) * 2 FROM(SELECT n FROM c) s WHERE(n > 1) ORDER BY(n);\n\nINSERT INTO t (a, b) VALUES(1, 2), (3, 4);\n\nSELECT CASE(a) WHEN(1) THEN(2) END, 1 + (2);\n"
        );
        // A clause that breaks puts its list on the next line as ever.
        let narrow = Options {
            inline_threshold: 20,
            ..opts
        };
        assert_eq!(
            format("insert into t values (1, 'one'), (2, 'two')", &narrow),
            "INSERT INTO t\nVALUES\n    (1, 'one'),\n    (2, 'two');\n"
        );
    }

    #[test]
    fn the_semicolon_goes_where_the_option_says() {
        let with = |semicolon: Semicolon, sql: &str| {
            format(
                sql,
                &Options {
                    semicolon,
                    inline_threshold: 30,
                    ..Options::default()
                },
            )
        };
        let sql = "select a from t order by n; select 2";
        assert_eq!(
            with(Semicolon::Attached, sql),
            "SELECT a FROM t ORDER BY n;\n\nSELECT 2;\n"
        );
        assert_eq!(
            with(Semicolon::Spaced, sql),
            "SELECT a FROM t ORDER BY n ;\n\nSELECT 2 ;\n"
        );
        assert_eq!(
            with(Semicolon::Newline, sql),
            "SELECT a FROM t ORDER BY n\n;\n\nSELECT 2\n;\n"
        );
        // After a statement that broke, too.
        let sql = "select alpha, beta from some_table where alpha > 1";
        assert_eq!(
            with(Semicolon::Spaced, sql),
            "SELECT alpha, beta\nFROM some_table\nWHERE alpha > 1 ;\n"
        );
        assert_eq!(
            with(Semicolon::Newline, sql),
            "SELECT alpha, beta\nFROM some_table\nWHERE alpha > 1\n;\n"
        );
    }

    /// The semicolon is never swallowed by a line comment: a comment after
    /// the last token ends up after the `;` when the `;` stays on that
    /// line, as it always has, and stays on its line when the `;` takes
    /// the next one.
    #[test]
    fn the_semicolon_never_lands_in_a_trailing_comment() {
        let cases = [
            (
                "select 1 -- c\n;",
                "SELECT 1; -- c\n",
                "SELECT 1 ; -- c\n",
                "SELECT 1 -- c\n;\n",
            ),
            (
                "select 1 -- c",
                "SELECT 1; -- c\n",
                "SELECT 1 ; -- c\n",
                "SELECT 1 -- c\n;\n",
            ),
            (
                "select 1; -- c",
                "SELECT 1; -- c\n",
                "SELECT 1 ; -- c\n",
                "SELECT 1\n; -- c\n",
            ),
            (
                "select 1\n-- c\n;",
                "SELECT 1\n-- c\n;\n",
                "SELECT 1\n-- c\n;\n",
                "SELECT 1\n-- c\n;\n",
            ),
            (
                "select 1 /* c */;",
                "SELECT 1 /* c */;\n",
                "SELECT 1 /* c */ ;\n",
                "SELECT 1 /* c */\n;\n",
            ),
        ];
        for (sql, attached, spaced, newline) in cases {
            for (semicolon, want) in [
                (Semicolon::Attached, attached),
                (Semicolon::Spaced, spaced),
                (Semicolon::Newline, newline),
            ] {
                let opts = Options {
                    semicolon,
                    ..Options::default()
                };
                let once = format(sql, &opts);
                assert_eq!(once, want, "{semicolon:?} {sql:?}");
                assert_eq!(format(&once, &opts), once, "{semicolon:?} {sql:?} again");
            }
        }
    }

    /// Every combination of the options, in both keyword cases: formatting
    /// the output again changes nothing, and only space, case, list commas
    /// and terminators differ from the input.
    #[test]
    fn every_option_combination_is_idempotent_and_keeps_the_tokens() {
        fn squeeze(s: &str) -> String {
            s.chars()
                .filter(|c| !c.is_whitespace() && *c != ',' && *c != ';')
                .flat_map(char::to_lowercase)
                .collect()
        }
        let sources = [
            CALLS_AND_KEYWORDS,
            "with c(n) as (select 1) select (n + 1) * 2 from (select n from c) s where (n > 1) order by (n)",
            "insert into t (a, b) values (1, 2), (3, 4) on conflict do nothing returning (a)",
            "select a, -- first\n b /* second */ from t -- tail\n;\n-- footer\n",
            "select case (a) when (1) then (2) else coalesce(b, (3)) end from t; select 2 -- c\n; ;",
            "from t select try_cast(a as varchar(10)), extract(year from d), row(1, 2) where a = any (select 1)",
            "create table t (id integer primary key, amount decimal(18, 2) check (amount > 0)); create macro f(a) as a + 1",
            "select 1;\nTHIS IS NOT SQL;\nselect count(*) from t where a in (1) -- c\n",
        ];
        for function_paren_space in [false, true] {
            for keyword_paren_space in [false, true] {
                for semicolon in [Semicolon::Attached, Semicolon::Spaced, Semicolon::Newline] {
                    for keyword_case in [KeywordCase::Upper, KeywordCase::Lower] {
                        for inline_threshold in [20, 100] {
                            let opts = Options {
                                indent_size: 4,
                                inline_threshold,
                                keyword_case,
                                function_paren_space,
                                keyword_paren_space,
                                semicolon,
                            };
                            for sql in sources {
                                let once = format(sql, &opts);
                                assert_eq!(format(&once, &opts), once, "{opts:?}\n{sql}");
                                assert_eq!(squeeze(&once), squeeze(sql), "{opts:?}\n{sql}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_spacing_options_combine_with_lower_case_keywords() {
        let opts = Options {
            keyword_case: KeywordCase::Lower,
            function_paren_space: true,
            keyword_paren_space: false,
            semicolon: Semicolon::Spaced,
            inline_threshold: 400,
            ..Options::default()
        };
        assert_eq!(
            format(CALLS_AND_KEYWORDS, &opts),
            "select count (*), coalesce (a, b), cast (x as decimal(18, 2)), x.lower (), sum (a) filter(where a > 1) over(partition by b) from read_csv ('f.csv') t join u using(k) where a in(1, 2) and exists(select 1) and not(a > 1) group by rollup (a, b) ;\n"
        );
        let opts = Options {
            semicolon: Semicolon::Newline,
            inline_threshold: 30,
            ..opts
        };
        assert_eq!(
            format("SELECT COUNT(*) FROM t WHERE a IN (1, 2) -- c", &opts),
            "select count (*)\nfrom t\nwhere a in(1, 2) -- c\n;\n"
        );
    }

    #[test]
    fn statements_are_separated_by_one_blank_line() {
        assert_eq!(f("select 1;;\n\n\n\nselect 2"), "SELECT 1;\n\nSELECT 2;\n");
    }

    #[test]
    fn comments_survive_in_place() {
        let sql =
            "-- header\nselect a, -- first\n    b /* second */ from t -- tail\n;\n-- footer\n";
        assert_eq!(
            f(sql),
            "-- header\nSELECT\n    a, -- first\n    b /* second */\nFROM t; -- tail\n-- footer\n"
        );
    }

    #[test]
    fn operators_and_punctuation_space_correctly() {
        assert_eq!(
            f("select -x, a[1], a[1:2], {'k': 1}, [1,2], t.*, f(x)->y, $1, s::int[] from t"),
            "SELECT -x, a[1], a[1:2], {'k': 1}, [1, 2], t.*, f(x) -> y, $1, s::int[] FROM t;\n"
        );
    }

    #[test]
    fn prefix_alias_and_star_modifiers() {
        assert_eq!(
            f("select total: sum(x), * exclude (a, b) from t"),
            "SELECT total: sum(x), * EXCLUDE (a, b) FROM t;\n"
        );
    }

    #[test]
    fn windows_break_inside_their_parens() {
        let sql = "select row_number() over (partition by customer_id order by order_date desc) as rn from orders";
        assert_eq!(
            narrow(sql, 50),
            "SELECT\n    row_number() OVER (\n        PARTITION BY customer_id\n        ORDER BY order_date DESC\n    ) AS rn\nFROM orders;\n"
        );
    }

    #[test]
    fn dml_and_ddl_take_the_generic_layout() {
        assert_eq!(
            narrow("insert into t (a, b) values (1, 2), (3, 4)", 30),
            "INSERT INTO t (a, b)\nVALUES (1, 2), (3, 4);\n"
        );
        assert_eq!(
            narrow("update t set a = 1, b = 2 where id = 3", 20),
            "UPDATE t\nSET a = 1, b = 2\nWHERE id = 3;\n"
        );
        assert_eq!(
            narrow(
                "create table t (id integer primary key, name varchar not null)",
                40
            ),
            "CREATE TABLE t (\n    id integer PRIMARY KEY,\n    name varchar NOT NULL\n);\n"
        );
        assert_eq!(
            narrow("create or replace table x as select a from t", 30),
            "CREATE OR REPLACE TABLE x AS\nSELECT a\nFROM t;\n"
        );
    }

    #[test]
    fn an_unparseable_statement_is_left_alone_and_its_neighbours_are_not() {
        let sql = "select   1;\nTHIS IS NOT SQL;\nselect   2";
        assert_eq!(f(sql), "SELECT 1;\n\nTHIS IS NOT SQL;\n\nSELECT 2;\n");
    }

    #[test]
    fn fallback_keeps_a_comment_trailing_the_terminator_with_its_statement() {
        // The first statement parses, the second does not, so the file takes
        // the per-statement path. The comment after `;` must not migrate.
        let sql = "select a, b, from t; -- note\nNOT SQL;";
        let once = f(sql);
        assert_eq!(once, "SELECT a, b FROM t; -- note\n\nNOT SQL;\n");
        assert_eq!(f(&once), once);
    }

    #[test]
    fn a_trailing_comma_before_the_terminator_is_dropped_even_when_broken() {
        let sql = "select alpha_column, beta_column, gamma_column, -- c\n;";
        assert_eq!(
            narrow(sql, 30),
            "SELECT\n    alpha_column,\n    beta_column,\n    gamma_column; -- c\n"
        );
    }

    #[test]
    fn a_bare_semicolon_with_comments_is_dropped_the_way_the_next_run_sees_it() {
        let sql = "SELECT 1;\n\n-- note\n/* block */;\n\n-- next\nSELECT 2;\n";
        let once = f(sql);
        assert_eq!(
            once,
            "SELECT 1;\n\n-- note\n/* block */\n-- next\nSELECT 2;\n"
        );
        assert_eq!(f(&once), once);
        let sql = "SELECT 1;\n/* tail */; -- trailing\n";
        let once = f(sql);
        assert_eq!(once, "SELECT 1;\n/* tail */\n-- trailing\n");
        assert_eq!(f(&once), once);
    }

    #[test]
    fn fallback_carries_a_bare_semicolon_into_the_next_statement() {
        // The `> junk` line forces the per-statement path; the bare `;`
        // after the block comment must not become its own piece.
        let sql = "SELECT 1;\n\n-- note:\n/* x */;\n\n-- next\nSELECT 2;\n\n> junk;\n";
        let once = f(sql);
        assert_eq!(
            once,
            "SELECT 1;\n\n-- note:\n/* x */\n-- next\nSELECT 2;\n\n> junk;\n"
        );
        assert_eq!(f(&once), once);
    }

    #[test]
    fn merge_arms_and_operator_chains_break() {
        let sql = "merge into people using upserts on people.id = upserts.id when matched and people.salary < 100_000 then update set salary = upserts.salary when matched then delete when not matched then insert by name";
        assert_eq!(
            narrow(sql, 60),
            "MERGE INTO people\nUSING upserts\nON people.id = upserts.id\nWHEN MATCHED AND people.salary < 100_000 THEN UPDATE SET salary = upserts.salary\nWHEN MATCHED THEN DELETE\nWHEN NOT MATCHED THEN INSERT BY NAME;\n"
        );
        let sql = "select 'aaaa' || 'bbbb' || 'cccc' || 'dddd' from t";
        assert_eq!(
            narrow(sql, 30),
            "SELECT\n    'aaaa'\n        || 'bbbb'\n        || 'cccc'\n        || 'dddd'\nFROM t;\n"
        );
        // A lone comparison never splits.
        assert_eq!(
            narrow("select * from t where alpha_beta_gamma > 1", 20),
            "SELECT *\nFROM t\nWHERE alpha_beta_gamma > 1;\n"
        );
        assert_eq!(
            f("with recursive r (n) as (select 1) select * from r"),
            "WITH RECURSIVE r(n) AS (SELECT 1) SELECT * FROM r;\n"
        );
    }

    #[test]
    fn empty_and_comment_only_files() {
        assert_eq!(f(""), "");
        assert_eq!(f("   \n\n"), "");
        assert_eq!(f("-- just a note\n"), "-- just a note\n");
    }

    #[test]
    fn idempotent_on_the_unit_examples() {
        for sql in [
            "with a as (select 1 as x from t), b as (select x from a) select * from b",
            "select case when a = 1 then 'one' when a = 2 then 'two' else 'many' end as n from t",
            "-- header\nselect a, -- first\n    b /* second */ from t -- tail\n;\n-- footer\n",
            "select row_number() over (partition by customer_id order by order_date desc) as rn from orders",
        ] {
            for w in [20, 40, 100] {
                let once = narrow(sql, w);
                let twice = narrow(&once, w);
                assert_eq!(once, twice, "not idempotent at width {w}:\n{once}");
            }
        }
    }

    #[test]
    fn stacked_prefix_operators_keep_their_space() {
        // `--1` would start a comment; `~-1`, `-~1` and `~~1` are other
        // operators (`~~` is LIKE).
        assert_eq!(f("select - -1 as x"), "SELECT - -1 AS x;\n");
        assert_eq!(f("select 2 * - -1"), "SELECT 2 * - -1;\n");
        assert_eq!(
            f("select ~ -1, - ~1, ~ ~1, + -1"),
            "SELECT ~ -1, - ~1, ~ ~1, + -1;\n"
        );
        assert_eq!(f("select [- -1], (- -a)"), "SELECT [- -1], (- -a);\n");
        // A single prefix operator still hugs its operand.
        assert_eq!(
            f("select - 1, ~ a, - (b + 1), -(-c)"),
            "SELECT -1, ~a, -(b + 1), -(-c);\n"
        );
        assert_eq!(
            f(&f("select - -1; select 2")),
            "SELECT - -1;\n\nSELECT 2;\n"
        );
    }

    #[test]
    fn a_byte_order_mark_is_kept_and_the_sql_after_it_formatted() {
        assert_eq!(
            f("\u{feff}select   1;\nselect   2"),
            "\u{feff}SELECT 1;\n\nSELECT 2;\n"
        );
    }

    #[test]
    fn an_unclosed_paren_costs_only_its_own_statement() {
        assert_eq!(
            f("select (;\nselect   1;\nselect 2"),
            "select (;\n\nSELECT 1;\n\nSELECT 2;\n"
        );
    }

    /// A line comment never moves between runs: formatting the output again
    /// changes nothing.
    #[test]
    fn line_comments_stay_put_from_one_run_to_the_next() {
        for (src, want) in [
            ("select a from t -- c1", "SELECT a FROM t; -- c1\n"),
            ("select 1 -- eof", "SELECT 1; -- eof\n"),
            (
                "select a, b from t where -- c\n x = 1",
                "SELECT a, b\nFROM t\nWHERE -- c\nx = 1;\n",
            ),
            (
                "select\n  a -- why a\n  , b\nfrom t",
                "SELECT\n    a, -- why a\n    b\nFROM t;\n",
            ),
            (
                "select list_transform([1,2,3] -- c\n, x -> x + 1) from t",
                "SELECT\n    list_transform(\n        [1, 2, 3], -- c\n        x -> x + 1\n    )\nFROM t;\n",
            ),
        ] {
            let once = f(src);
            assert_eq!(once, want, "{src:?}");
            assert_eq!(f(&once), once, "second run moved something: {src:?}");
        }
    }
}
