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
//! - [`format`] — the entry point: parse, lower, print; and when a file does
//!   not parse as a whole, do that per statement and leave what we cannot
//!   parse exactly as it was.

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

/// The whole knob budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Spaces per indent level.
    pub indent_size: usize,
    /// A construct that fits within this many columns stays on one line.
    pub inline_threshold: usize,
    pub keyword_case: KeywordCase,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            indent_size: 4,
            inline_threshold: 100,
            keyword_case: KeywordCase::Upper,
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
    if let Some(tree) = grebe_syntax::matcher::parse(src) {
        return finish(format_tree(&tree, src, opts));
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
        match grebe_syntax::matcher::parse(piece) {
            Some(tree) => {
                if tree.find("Statement").is_empty() {
                    continue; // carry forward; `carry` stays put
                }
                let out = format_tree(&tree, piece, opts);
                if !out.is_empty() {
                    pieces.push(out);
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
        if let Some(tree) = grebe_syntax::matcher::parse(piece) {
            let out = format_tree(&tree, piece, opts);
            if !out.is_empty() {
                pieces.push(out);
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

fn format_tree(tree: &grebe_syntax::cst::Tree, src: &str, opts: &Options) -> String {
    let lowerer = lower::Lowerer::new(tree, src, opts);
    let doc = lowerer.program();
    doc::print(&doc, opts.inline_threshold, opts.indent_size)
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
}
