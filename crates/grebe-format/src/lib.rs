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
//! And for houses that already have a style guide, the choices such guides
//! most often make differently. Every default is the canonical style, so a
//! project that sets none of them gets exactly what it got before they
//! existed:
//!
//! | knob | default |
//! |---|---|
//! | `comma` | trailing: `a,` |
//! | `logical_operator` | leading: `AND b = 2` starts its line |
//! | `function_case` | lower: `count(x)` |
//! | `type_case` | lower: `x::integer` |
//! | `indent_style` | space |
//! | `blank_lines_between_statements` | 1 |
//! | `max_blank_lines_in_statement` | 0: the author's blank lines go |
//! | `list_layout` | auto: a list that fits stays on one line |
//! | `case_layout` | auto: a `CASE` that fits stays on one line |
//!
//! [`Style`] names a bundle of them. Nobody has to configure any of this:
//! the point of one canonical style is that it is there when nothing is
//! set.
//!
//! # Specification
//!
//! Black/ruff-format posture: one canonical style, and knobs only where
//! established guides disagree with it. The rules live in `lower`; in
//! summary, for the defaults:
//!
//! - **Case:** keywords follow `keyword_case`; function names follow
//!   `function_case` and type names `type_case`, both lower by default;
//!   identifiers are never re-cased, because their case is meaning.
//! - **Statements:** a statement that fits the threshold stays on one line;
//!   otherwise every clause (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, `LIMIT`,
//!   ...) starts a line, flush left within its query level. There is no
//!   middle ground.
//! - **Lists:** inline until they do not fit, then one item per line,
//!   indented one level; commas trail. A trailing comma the
//!   source had is kept in a broken list and dropped in a flat one; one is
//!   never added.
//! - **Indentation:** spaces, `indent_size` per level; brackets and
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
//! # Switching the formatter off
//!
//! A line comment on a line of its own, `-- grebe: format off` (or
//! `-- fmt: off`), switches the formatter off up to the next
//! `-- grebe: format on` (or `-- fmt: on`), or to the end of the file. The
//! markers are matched whatever their letter case. Everything from the
//! `off` line to the `on` line, both included, is emitted byte for byte.
//!
//! The unit is the statement: a statement any part of which lies in such a
//! region -- one that holds a marker between its tokens, too -- is left
//! exactly as written, all of it, from its first token to its `;` and a
//! `--` comment after that. Laying out half a statement would mean
//! deciding line breaks around text nobody is allowed to move.
//!
//! What is outside is formatted as ever. The blank lines the author left
//! between a region and the code next to it are kept, up to
//! `blank_lines_between_statements`.
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
mod verbatim;

/// How keywords are cased. Identifiers are never re-cased; function and type
/// names have [`NameCase`] options of their own and do not follow this.
/// Upper is the default because it is DuckDB's own documentation style.
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

/// Where the comma of a list that breaks over lines goes. A list on one
/// line is `a, b` either way.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Comma {
    /// At the end of its item's line: `a,` then `b`.
    #[default]
    Trailing,
    /// At the start of the next item's line: `a` then `, b`.
    Leading,
}

/// Where `AND` and `OR` go when a condition breaks over lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LogicalOperator {
    /// At the start of the line of the operand it joins: `AND b = 2`.
    #[default]
    Leading,
    /// At the end of the line before: `a = 1 AND`.
    Trailing,
}

/// How a function name or a type name is cased.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum NameCase {
    #[default]
    Lower,
    Upper,
    /// As written.
    Preserve,
}

/// What an indent level is made of.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum IndentStyle {
    /// `indent_size` spaces.
    #[default]
    Space,
    /// One tab, counted as `indent_size` columns towards the width.
    Tab,
}

/// When a select list, a `GROUP BY` list or a query's `ORDER BY` list goes
/// one item per line.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ListLayout {
    /// When it does not fit on one line.
    #[default]
    Auto,
    /// Whenever it has more than one item.
    Expanded,
}

/// When a `CASE` expression goes one arm per line.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CaseLayout {
    /// When it does not fit on one line.
    #[default]
    Auto,
    /// Whenever it has more than one `WHEN`.
    Expanded,
}

/// A named bundle of options: see [`Options::for_style`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Style {
    /// The canonical style: [`Options::default`].
    #[default]
    Default,
    /// In the spirit of the dbt style guide.
    Dbt,
    /// In the spirit of Matt Mazur's SQL style guide.
    Mazur,
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
    /// Where the comma goes in a list that breaks: select lists,
    /// `GROUP BY` and `ORDER BY` lists, column definitions, `VALUES` rows,
    /// arguments, `IN` lists, CTEs -- every comma list there is. With
    /// `Leading`, a trailing comma the source had is dropped rather than
    /// kept, since it has no line to lead, and a line comment after an
    /// item stays at the end of that item's line, before the line the
    /// comma starts.
    pub comma: Comma,
    /// Where `AND` and `OR` go in a condition that breaks. Other operator
    /// chains (`||`, `+`) always lead. With `Trailing`, a line comment
    /// after an operand moves behind the operator that follows it.
    pub logical_operator: LogicalOperator,
    /// The case of a function name in a call: a scalar function, an
    /// aggregate, a window function, a table function, with or without a
    /// schema in front (the schema is an identifier and keeps its case).
    /// `x.lower()` is such a call to the grammar, `lower` in schema `x`,
    /// so its name is covered. Not covered, by any value: quoted names;
    /// the name of a method called on anything but a name (`f(x).g()`,
    /// `'a'.upper()`), which the grammar reads as a label; the name a
    /// macro is created under; and the keyword forms that read as calls
    /// (`CAST`, `COALESCE`, `EXTRACT`, `TRIM`, `ROW`, ...), which are
    /// keywords and follow `keyword_case`.
    pub function_case: NameCase,
    /// The case of a type, wherever one stands (`CAST(x AS ...)`,
    /// `x::...`, a column definition): its name, a user-defined one
    /// included, and the keywords that are part of it (`timestamp with
    /// time zone`, `struct`, `double precision`). A word that could be a
    /// type name and stands where a column or table name does is an
    /// identifier and keeps its case.
    pub type_case: NameCase,
    pub indent_style: IndentStyle,
    /// Blank lines between one statement and the next.
    pub blank_lines_between_statements: usize,
    /// How many of the blank lines the author left in a row inside a
    /// statement are kept. They are kept only where the layout breaks the
    /// line anyway -- above an item, a clause, a CTE, a comment -- and a
    /// line is never broken to keep one. A statement's leading comments
    /// count as inside it, except that nothing is kept above the first.
    pub max_blank_lines_in_statement: usize,
    pub list_layout: ListLayout,
    pub case_layout: CaseLayout,
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
            comma: Comma::Trailing,
            logical_operator: LogicalOperator::Leading,
            function_case: NameCase::Lower,
            type_case: NameCase::Lower,
            indent_style: IndentStyle::Space,
            blank_lines_between_statements: 1,
            max_blank_lines_in_statement: 0,
            list_layout: ListLayout::Auto,
            case_layout: CaseLayout::Auto,
        }
    }
}

impl Options {
    /// The options a named style stands for. `Dbt` and `Mazur` are in the
    /// spirit of those guides, not a claim of conformance: each sets
    /// exactly `keyword_case` lower, `function_case` lower, `type_case`
    /// lower, `comma` trailing, `indent_style` space, `indent_size` 4 and
    /// `list_layout` expanded, and leaves everything else at its default.
    /// The two are the same bundle today.
    #[must_use]
    pub fn for_style(style: Style) -> Self {
        match style {
            Style::Default => Self::default(),
            Style::Dbt | Style::Mazur => Self {
                keyword_case: KeywordCase::Lower,
                function_case: NameCase::Lower,
                type_case: NameCase::Lower,
                comma: Comma::Trailing,
                indent_style: IndentStyle::Space,
                indent_size: 4,
                list_layout: ListLayout::Expanded,
                ..Self::default()
            },
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
    match verbatim::regions(src) {
        Some(regions) => format_around(src, &regions, opts),
        None => format_code(src, opts),
    }
}

/// [`format`] for a file, or a stretch of one, in which the formatter is
/// not switched off anywhere.
fn format_code(src: &str, opts: &Options) -> String {
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
    finish(pieces.join(&"\n".repeat(opts.blank_lines_between_statements + 1)))
}

/// A file with regions the formatter is switched off for: the stretches
/// between them formatted, each on its own, and the regions as written.
///
/// A region is whole lines, or starts or ends where a statement does, so
/// the stretches around it are whole statements and their comments. The
/// blank lines between a region and its neighbours are the author's, up to
/// the number that separates statements.
fn format_around(src: &str, regions: &[(usize, usize)], opts: &Options) -> String {
    let newlines = |s: &str| s.bytes().filter(|b| *b == b'\n').count();
    let keep = |blank: usize| "\n".repeat(blank.min(opts.blank_lines_between_statements));
    let mut out = String::with_capacity(src.len());
    let mut cursor = 0usize;
    // The stretch before a region, or after the last one.
    let code = |out: &mut String, text: &str, region_follows: bool| {
        let body = text.trim();
        if body.is_empty() {
            if !out.is_empty() && region_follows {
                out.push_str(&keep(newlines(text)));
            }
            return;
        }
        let formatted = format_code(text, opts);
        if formatted.is_empty() {
            return; // nothing but a bare `;`
        }
        if !out.is_empty() {
            let lead = &text[..text.len() - text.trim_start().len()];
            out.push_str(&keep(newlines(lead)));
        }
        out.push_str(&formatted);
        if region_follows {
            let trail = &text[text.trim_end().len()..];
            out.push_str(&keep(newlines(trail).saturating_sub(1)));
        }
    };
    for &(start, end) in regions {
        code(&mut out, &src[cursor..start], true);
        out.push_str(&src[start..end]);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        // A region that ends with its line ends before the line break.
        cursor = end + usize::from(src[end..].starts_with('\n'));
    }
    code(&mut out, &src[cursor..], false);
    out
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
                                ..Options::default()
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

    // ----- the style options -------------------------------------------------

    /// The options with one or more of them changed.
    fn opts(change: impl FnOnce(&mut Options)) -> Options {
        let mut o = Options::default();
        change(&mut o);
        o
    }

    /// Formats, and checks on the way that a second run changes nothing.
    fn fmt(sql: &str, o: &Options) -> String {
        let once = format(sql, o);
        assert_eq!(format(&once, o), once, "not idempotent: {sql:?}\n{o:?}");
        once
    }

    #[test]
    fn the_defaults_are_the_canonical_style() {
        let o = Options::default();
        assert_eq!(o.comma, Comma::Trailing);
        assert_eq!(o.logical_operator, LogicalOperator::Leading);
        assert_eq!(o.function_case, NameCase::Lower);
        assert_eq!(o.type_case, NameCase::Lower);
        assert_eq!(o.indent_style, IndentStyle::Space);
        assert_eq!(o.blank_lines_between_statements, 1);
        assert_eq!(o.max_blank_lines_in_statement, 0);
        assert_eq!(o.list_layout, ListLayout::Auto);
        assert_eq!(o.case_layout, CaseLayout::Auto);
        assert_eq!(Options::for_style(Style::Default), o);
    }

    #[test]
    fn leading_commas_start_the_next_line_in_every_list_that_breaks() {
        let o = opts(|o| {
            o.comma = Comma::Leading;
            o.inline_threshold = 30;
        });
        // A list on one line is the same either way.
        assert_eq!(
            fmt("select a, b from t where x in (1, 2)", &o),
            "SELECT a, b\nFROM t\nWHERE x IN (1, 2);\n"
        );
        assert_eq!(
            fmt(
                "select alpha_column, beta_column, gamma_column from t group by alpha_column, beta_column, gamma_column order by alpha_column, beta_column, gamma_column",
                &o
            ),
            "SELECT\n    alpha_column\n    , beta_column\n    , gamma_column\nFROM t\nGROUP BY\n    alpha_column\n    , beta_column\n    , gamma_column\nORDER BY\n    alpha_column\n    , beta_column\n    , gamma_column;\n"
        );
        assert_eq!(
            fmt(
                "create table t (id integer primary key, name varchar not null)",
                &o
            ),
            "CREATE TABLE t (\n    id integer PRIMARY KEY\n    , name varchar NOT NULL\n);\n"
        );
        assert_eq!(
            fmt(
                "insert into t values (1, 'one'), (2, 'two'), (3, 'three')",
                &o
            ),
            "INSERT INTO t\nVALUES\n    (1, 'one')\n    , (2, 'two')\n    , (3, 'three');\n"
        );
        assert_eq!(
            fmt(
                "select coalesce(alpha_column, beta_column, gamma_column) from t where x in ('alpha', 'beta', 'gamma', 'delta')",
                &o
            ),
            "SELECT\n    COALESCE(\n        alpha_column\n        , beta_column\n        , gamma_column\n    )\nFROM t\nWHERE x IN (\n    'alpha'\n    , 'beta'\n    , 'gamma'\n    , 'delta'\n);\n"
        );
        assert_eq!(
            fmt(
                "with a as (select 1 as x from t), b as (select x from a) select * from b",
                &o
            ),
            "WITH a AS (\n    SELECT 1 AS x\n    FROM t\n)\n, b AS (\n    SELECT x\n    FROM a\n)\n\nSELECT *\nFROM b;\n"
        );
    }

    /// A list of leading commas has no place for a trailing one, so a
    /// trailing comma the source had goes; the comma that makes a tuple
    /// stays.
    #[test]
    fn leading_commas_drop_a_trailing_comma_and_keep_a_tuples() {
        let o = opts(|o| {
            o.comma = Comma::Leading;
            o.inline_threshold = 30;
        });
        assert_eq!(
            fmt("select alpha_column, beta_column, gamma_column, from t", &o),
            "SELECT\n    alpha_column\n    , beta_column\n    , gamma_column\nFROM t;\n"
        );
        assert_eq!(
            fmt("select (42,), (1, 2,), f(1,)", &o),
            "SELECT (42,), (1, 2), f(1);\n"
        );
    }

    /// The comma never ends up inside a line comment: a comment after an
    /// item, or after its comma, ends the item's line, and the comma starts
    /// the next one. Comments on lines of their own stay above the comma;
    /// a block comment between the comma and the item stays there.
    #[test]
    fn a_leading_comma_goes_around_the_comments_between_two_items() {
        let o = opts(|o| o.comma = Comma::Leading);
        for (src, want) in [
            (
                "select a, -- first\n b from t",
                "SELECT\n    a -- first\n    , b\nFROM t;\n",
            ),
            (
                "select a -- first\n , b from t",
                "SELECT\n    a -- first\n    , b\nFROM t;\n",
            ),
            (
                "select a,\n -- about b\n b from t",
                "SELECT\n    a\n    -- about b\n    , b\nFROM t;\n",
            ),
            (
                "select a\n -- about b\n , b from t",
                "SELECT\n    a\n    -- about b\n    , b\nFROM t;\n",
            ),
            (
                "select a, /* about b */ b from t",
                "SELECT a, /* about b */ b FROM t;\n",
            ),
            (
                "select a /* about a */, b from t",
                "SELECT a /* about a */, b FROM t;\n",
            ),
            (
                "select a, -- first\n /* x */ b, c -- last\n from t",
                "SELECT\n    a -- first\n    , /* x */ b\n    , c -- last\nFROM t;\n",
            ),
            (
                "select a, -- first\n /* x */\n -- y\n /* z */ b from t",
                "SELECT\n    a -- first\n    /* x */\n    -- y\n    , /* z */ b\nFROM t;\n",
            ),
            (
                "with a as (select 1), -- first\n b as (select 2) select 3",
                "WITH a AS (\n    SELECT 1\n) -- first\n, b AS (\n    SELECT 2\n)\n\nSELECT 3;\n",
            ),
        ] {
            assert_eq!(fmt(src, &o), want, "{src:?}");
        }
    }

    #[test]
    fn trailing_logical_operators_end_the_line_they_join() {
        let o = opts(|o| {
            o.logical_operator = LogicalOperator::Trailing;
            o.inline_threshold = 30;
        });
        assert_eq!(
            fmt(
                "select * from t where alpha = 1 and beta = 2 and gamma = 3",
                &o
            ),
            "SELECT *\nFROM t\nWHERE alpha = 1 AND\n    beta = 2 AND\n    gamma = 3;\n"
        );
        assert_eq!(
            fmt(
                "select * from t where alpha = 1 or (beta = 2 and gamma_column = 3 and delta = 4)",
                &o
            ),
            "SELECT *\nFROM t\nWHERE alpha = 1 OR\n    (\n        beta = 2 AND\n            gamma_column = 3 AND\n            delta = 4\n    );\n"
        );
        // One that fits is the same either way, and other operators lead
        // as ever.
        assert_eq!(
            fmt("select 1 where a and b", &o),
            "SELECT 1 WHERE a AND b;\n"
        );
        assert_eq!(
            fmt("select 'aaaa' || 'bbbb' || 'cccc' || 'dddd' from t", &o),
            "SELECT\n    'aaaa'\n        || 'bbbb'\n        || 'cccc'\n        || 'dddd'\nFROM t;\n"
        );
        // A join condition too.
        assert_eq!(
            fmt(
                "select * from a join b on a.alpha = b.alpha and a.beta = b.beta",
                &o
            ),
            "SELECT *\nFROM a\nJOIN b\n    ON a.alpha = b.alpha AND\n        a.beta = b.beta;\n"
        );
    }

    /// A line comment after an operand would swallow the operator that
    /// comes after it: it moves behind the operator.
    #[test]
    fn a_trailing_logical_operator_goes_before_the_operands_line_comment() {
        let o = opts(|o| o.logical_operator = LogicalOperator::Trailing);
        for (src, want) in [
            (
                "select 1 where a = 1 -- first\n and b = 2 -- second\n and c = 3",
                "SELECT 1\nWHERE a = 1 AND -- first\n    b = 2 AND -- second\n    c = 3;\n",
            ),
            (
                "select 1 where a = 1 and -- first\n b = 2",
                "SELECT 1\nWHERE a = 1 AND -- first\n    b = 2;\n",
            ),
            (
                "select 1 where a = 1\n -- about b\n and b = 2",
                "SELECT 1\nWHERE a = 1\n    -- about b\n    AND\n    b = 2;\n",
            ),
            (
                "select 1 where (a = 1 -- inner\n or b = 2) -- outer\n and c = 3",
                "SELECT 1\nWHERE (\n    a = 1 OR -- inner\n        b = 2\n) AND -- outer\n    c = 3;\n",
            ),
        ] {
            assert_eq!(fmt(src, &o), want, "{src:?}");
        }
        // The default leaves the comment where it was.
        assert_eq!(
            fmt(
                "select 1 where a = 1 -- first\n and b = 2",
                &Options::default()
            ),
            "SELECT 1\nWHERE a = 1 -- first\n    AND b = 2;\n"
        );
    }

    const NAMES: &str = "Select Count(*), Main.My_Func(X), x.Lower(), F(x).Upper(), Coalesce(a, B), Cast(x As Integer), y::VarChar, z::My_Type, d::Timestamp With Time Zone, Date, \"Count\"(1), Date '2020-01-01' From Read_CSV('f.csv') T Where Sum = 1";

    #[test]
    fn function_case_covers_function_names_and_nothing_else() {
        let wide = |o: &mut Options| o.inline_threshold = 400;
        // Lower is the default and what the formatter always did.
        assert_eq!(
            fmt(NAMES, &opts(wide)),
            "SELECT count(*), Main.my_func(X), x.lower(), f(x).Upper(), COALESCE(a, B), CAST(x AS integer), y::varchar, z::my_type, d::timestamp with time zone, Date, \"Count\"(1), date '2020-01-01' FROM read_csv('f.csv') T WHERE Sum = 1;\n"
        );
        // Upper: the names of calls, but not the keyword forms (which
        // follow `keyword_case`), not a quoted name, not a schema, not a
        // column called like a function, and no type.
        assert_eq!(
            fmt(
                NAMES,
                &opts(|o| {
                    wide(o);
                    o.function_case = NameCase::Upper;
                    o.keyword_case = KeywordCase::Lower;
                })
            ),
            "select COUNT(*), Main.MY_FUNC(X), x.LOWER(), F(x).Upper(), coalesce(a, B), cast(x as integer), y::varchar, z::my_type, d::timestamp with time zone, Date, \"Count\"(1), date '2020-01-01' from READ_CSV('f.csv') T where Sum = 1;\n"
        );
        assert_eq!(
            fmt(
                NAMES,
                &opts(|o| {
                    wide(o);
                    o.function_case = NameCase::Preserve;
                })
            ),
            "SELECT Count(*), Main.My_Func(X), x.Lower(), F(x).Upper(), COALESCE(a, B), CAST(x AS integer), y::varchar, z::my_type, d::timestamp with time zone, Date, \"Count\"(1), date '2020-01-01' FROM Read_CSV('f.csv') T WHERE Sum = 1;\n"
        );
    }

    #[test]
    fn type_case_covers_types_and_nothing_else() {
        let wide = |o: &mut Options| o.inline_threshold = 400;
        // Upper: type names and the keywords of a type, whatever
        // `keyword_case` says; a column called like a type is a column.
        assert_eq!(
            fmt(
                NAMES,
                &opts(|o| {
                    wide(o);
                    o.type_case = NameCase::Upper;
                    o.keyword_case = KeywordCase::Lower;
                })
            ),
            "select count(*), Main.my_func(X), x.lower(), f(x).Upper(), coalesce(a, B), cast(x as INTEGER), y::VARCHAR, z::MY_TYPE, d::TIMESTAMP WITH TIME ZONE, Date, \"Count\"(1), DATE '2020-01-01' from read_csv('f.csv') T where Sum = 1;\n"
        );
        assert_eq!(
            fmt(
                NAMES,
                &opts(|o| {
                    wide(o);
                    o.type_case = NameCase::Preserve;
                })
            ),
            "SELECT count(*), Main.my_func(X), x.lower(), f(x).Upper(), COALESCE(a, B), CAST(x AS Integer), y::VarChar, z::My_Type, d::Timestamp With Time Zone, Date, \"Count\"(1), Date '2020-01-01' FROM read_csv('f.csv') T WHERE Sum = 1;\n"
        );
        // Column definitions, nested types and their field names.
        let ddl = "create table T (Id Integer primary key, Amount Decimal(18, 2), Tags VarChar[], S Struct(A Int, B Map(VarChar, Double Precision)))";
        assert_eq!(
            fmt(
                ddl,
                &opts(|o| {
                    wide(o);
                    o.type_case = NameCase::Upper;
                })
            ),
            "CREATE TABLE T (Id INTEGER PRIMARY KEY, Amount DECIMAL(18, 2), Tags VARCHAR[], S STRUCT(A INT, B MAP(VARCHAR, DOUBLE PRECISION)));\n"
        );
        assert_eq!(
            fmt(ddl, &opts(wide)),
            "CREATE TABLE T (Id integer PRIMARY KEY, Amount decimal(18, 2), Tags varchar[], S struct(A int, B map(varchar, double precision)));\n"
        );
    }

    #[test]
    fn tabs_indent_one_per_level_and_count_as_indent_size_columns() {
        let sql = "select alpha_column, beta_column from t where x in (select y from u where zeta > 1 and omega > 2)";
        let o = opts(|o| {
            o.indent_style = IndentStyle::Tab;
            o.inline_threshold = 30;
        });
        let tabs = fmt(sql, &o);
        assert_eq!(
            tabs,
            "SELECT\n\talpha_column,\n\tbeta_column\nFROM t\nWHERE x IN (\n\tSELECT y\n\tFROM u\n\tWHERE zeta > 1\n\t\tAND omega > 2\n);\n"
        );
        // The same lines as with spaces, at every indent size: a tab is as
        // wide as the indent it stands for.
        for indent_size in [2, 4, 8] {
            for inline_threshold in [20, 30, 40, 60] {
                let spaces = opts(|o| {
                    o.indent_size = indent_size;
                    o.inline_threshold = inline_threshold;
                });
                let tabs = Options {
                    indent_style: IndentStyle::Tab,
                    ..spaces.clone()
                };
                assert_eq!(
                    fmt(sql, &tabs).replace('\t', &" ".repeat(indent_size)),
                    fmt(sql, &spaces),
                    "{indent_size} {inline_threshold}"
                );
            }
        }
        // No tab is left on a blank line.
        let o = opts(|o| {
            o.indent_style = IndentStyle::Tab;
            o.inline_threshold = 20;
        });
        assert_eq!(
            fmt("select (with a as (select 1) select * from a)", &o),
            "SELECT\n\t(\n\t\tWITH a AS (\n\t\t\tSELECT 1\n\t\t)\n\n\t\tSELECT *\n\t\tFROM a\n\t);\n"
        );
    }

    #[test]
    fn statements_are_separated_by_as_many_blank_lines_as_asked() {
        let sql = "select 1;\n\n\n\n-- two\nselect 2; select 3";
        for (n, want) in [
            (0, "SELECT 1;\n-- two\nSELECT 2;\nSELECT 3;\n"),
            (1, "SELECT 1;\n\n-- two\nSELECT 2;\n\nSELECT 3;\n"),
            (2, "SELECT 1;\n\n\n-- two\nSELECT 2;\n\n\nSELECT 3;\n"),
        ] {
            let o = opts(|o| o.blank_lines_between_statements = n);
            assert_eq!(fmt(sql, &o), want, "{n}");
        }
        // The path a file with an unparseable statement takes agrees.
        let sql = "select 1;\nTHIS IS NOT SQL;\nselect 2";
        for (n, want) in [
            (0, "SELECT 1;\nTHIS IS NOT SQL;\nSELECT 2;\n"),
            (2, "SELECT 1;\n\n\nTHIS IS NOT SQL;\n\n\nSELECT 2;\n"),
        ] {
            let o = opts(|o| o.blank_lines_between_statements = n);
            assert_eq!(fmt(sql, &o), want, "{n}");
        }
    }

    const SPACED_OUT: &str = "-- header\n\nwith a as (select 1 as x from t),\n\n\n\nb as (select x from a)\n\nselect alpha_column,\n\n  -- about beta\n\n  beta_column,\n  gamma_column\n\nfrom b\n\n\nwhere x_column = 1\n\n  and y_column = 2;\n\n\n\nselect a,\n\n b from t;\n";

    #[test]
    fn blank_lines_in_a_statement_are_dropped_by_default() {
        assert_eq!(
            fmt(SPACED_OUT, &opts(|o| o.inline_threshold = 30)),
            "-- header\nWITH a AS (\n    SELECT 1 AS x\n    FROM t\n),\nb AS (\n    SELECT x\n    FROM a\n)\n\nSELECT\n    alpha_column,\n    -- about beta\n    beta_column,\n    gamma_column\nFROM b\nWHERE x_column = 1\n    AND y_column = 2;\n\nSELECT a, b FROM t;\n"
        );
    }

    /// Kept where a line breaks anyway, up to the number asked for, and
    /// never a reason to break one: the last statement fits on a line and
    /// stays on it.
    #[test]
    fn blank_lines_in_a_statement_are_kept_where_the_layout_breaks() {
        let o = opts(|o| {
            o.inline_threshold = 30;
            o.max_blank_lines_in_statement = 1;
        });
        assert_eq!(
            fmt(SPACED_OUT, &o),
            "-- header\n\nWITH a AS (\n    SELECT 1 AS x\n    FROM t\n),\n\nb AS (\n    SELECT x\n    FROM a\n)\n\nSELECT\n    alpha_column,\n\n    -- about beta\n\n    beta_column,\n    gamma_column\n\nFROM b\n\nWHERE x_column = 1\n\n    AND y_column = 2;\n\nSELECT a, b FROM t;\n"
        );
        let o = opts(|o| {
            o.inline_threshold = 30;
            o.max_blank_lines_in_statement = 2;
        });
        assert_eq!(
            fmt(SPACED_OUT, &o),
            "-- header\n\nWITH a AS (\n    SELECT 1 AS x\n    FROM t\n),\n\n\nb AS (\n    SELECT x\n    FROM a\n)\n\nSELECT\n    alpha_column,\n\n    -- about beta\n\n    beta_column,\n    gamma_column\n\nFROM b\n\n\nWHERE x_column = 1\n\n    AND y_column = 2;\n\nSELECT a, b FROM t;\n"
        );
        // The blank line the layout puts after the CTEs is not doubled, and
        // the space between statements is not the statement's to keep.
        let o = opts(|o| {
            o.inline_threshold = 20;
            o.max_blank_lines_in_statement = 3;
            o.blank_lines_between_statements = 0;
        });
        assert_eq!(
            fmt(
                "select 1;\n\n\n\nwith a as (select 1)\n\nselect * from a;\n\n\n-- next\n\nselect 2",
                &o
            ),
            "SELECT 1;\nWITH a AS (\n    SELECT 1\n)\n\nSELECT *\nFROM a;\n-- next\n\nSELECT 2;\n"
        );
    }

    /// A blank line stays with the item it stood above when the comma or
    /// the operator moves to the other end of the line.
    #[test]
    fn kept_blank_lines_follow_the_item_across_comma_and_operator_styles() {
        let trailing =
            "select\n    alpha_column,\n\n    beta_column\nfrom t\nwhere x = 1 and\n\n    y = 2";
        let leading =
            "select\n    alpha_column\n\n    , beta_column\nfrom t\nwhere x = 1\n\n    and y = 2";
        let keep = |o: &mut Options| {
            o.max_blank_lines_in_statement = 1;
            o.inline_threshold = 20;
        };
        let default =
            "SELECT\n    alpha_column,\n\n    beta_column\nFROM t\nWHERE x = 1\n\n    AND y = 2;\n";
        let other = "SELECT\n    alpha_column\n\n    , beta_column\nFROM t\nWHERE x = 1 AND\n\n    y = 2;\n";
        for src in [trailing, leading] {
            assert_eq!(fmt(src, &opts(keep)), default, "{src:?}");
            let o = opts(|o| {
                keep(o);
                o.comma = Comma::Leading;
                o.logical_operator = LogicalOperator::Trailing;
            });
            assert_eq!(fmt(src, &o), other, "{src:?}");
        }
    }

    #[test]
    fn expanded_lists_go_one_item_per_line_even_when_they_fit() {
        let o = opts(|o| o.list_layout = ListLayout::Expanded);
        assert_eq!(
            fmt(
                "select a, b from t where x = 1 group by a, b order by a, b limit 5",
                &o
            ),
            "SELECT\n    a,\n    b\nFROM t\nWHERE x = 1\nGROUP BY\n    a,\n    b\nORDER BY\n    a,\n    b\nLIMIT 5;\n"
        );
        // One item is not a list, and neither is `GROUP BY ALL` or the
        // arguments of `ROLLUP`.
        assert_eq!(
            fmt("select a from t group by all order by 1", &o),
            "SELECT a FROM t GROUP BY ALL ORDER BY 1;\n"
        );
        assert_eq!(
            fmt("select a from t group by rollup (a, b)", &o),
            "SELECT a FROM t GROUP BY ROLLUP(a, b);\n"
        );
        // Other lists are left to fit: arguments, `IN`, `VALUES`, a window's
        // or an aggregate's `ORDER BY`.
        assert_eq!(
            fmt(
                "select f(a, b), sum(x order by a, b), rank() over (partition by a, b order by c, d) from t where x in (1, 2)",
                &o
            ),
            "SELECT\n    f(a, b),\n    sum(x ORDER BY a, b),\n    rank() OVER (PARTITION BY a, b ORDER BY c, d)\nFROM t\nWHERE x IN (1, 2);\n"
        );
        assert_eq!(
            fmt("insert into t values (1, 2), (3, 4)", &o),
            "INSERT INTO t VALUES (1, 2), (3, 4);\n"
        );
        // A subquery with a list of its own expands, and what holds it
        // breaks with it.
        assert_eq!(
            fmt("select a from (select a, b from t) s", &o),
            "SELECT a\nFROM (\n    SELECT\n        a,\n        b\n    FROM t\n) s;\n"
        );
        // A trailing comma is kept, as in any list that is broken.
        assert_eq!(
            fmt("select a, b, from t", &o),
            "SELECT\n    a,\n    b,\nFROM t;\n"
        );
        assert_eq!(fmt("select a, from t", &o), "SELECT a FROM t;\n");
    }

    #[test]
    fn expanded_case_goes_one_arm_per_line_even_when_it_fits() {
        let o = opts(|o| o.case_layout = CaseLayout::Expanded);
        assert_eq!(
            fmt(
                "select case when a = 1 then 'one' when a = 2 then 'two' else 'many' end as n from t",
                &o
            ),
            "SELECT\n    CASE\n        WHEN a = 1 THEN 'one'\n        WHEN a = 2 THEN 'two'\n        ELSE 'many'\n    END AS n\nFROM t;\n"
        );
        assert_eq!(
            fmt("select case x when 1 then 'a' when 2 then 'b' end", &o),
            "SELECT\n    CASE x\n        WHEN 1 THEN 'a'\n        WHEN 2 THEN 'b'\n    END;\n"
        );
        // One `WHEN` is left to fit, `ELSE` or not.
        assert_eq!(
            fmt("select case when a then 1 else 2 end from t", &o),
            "SELECT CASE WHEN a THEN 1 ELSE 2 END FROM t;\n"
        );
    }

    #[test]
    fn the_named_styles_are_the_bundles_they_are_documented_to_be() {
        for style in [Style::Dbt, Style::Mazur] {
            let o = Options::for_style(style);
            let want = Options {
                keyword_case: KeywordCase::Lower,
                function_case: NameCase::Lower,
                type_case: NameCase::Lower,
                comma: Comma::Trailing,
                indent_style: IndentStyle::Space,
                indent_size: 4,
                list_layout: ListLayout::Expanded,
                ..Options::default()
            };
            assert_eq!(o, want, "{style:?}");
            assert_eq!(
                fmt(
                    "SELECT Customer_Id, COUNT(*) AS N, CAST(Total AS DECIMAL(18, 2)) FROM Orders WHERE Status = 'open' AND Total > 0 GROUP BY 1, 3 ORDER BY N DESC",
                    &o
                ),
                "select\n    Customer_Id,\n    count(*) as N,\n    cast(Total as decimal(18, 2))\nfrom Orders\nwhere Status = 'open' and Total > 0\ngroup by\n    1,\n    3\norder by N desc;\n",
                "{style:?}"
            );
        }
        // An option set next to a style overrides it: that is the caller's
        // to do, and nothing in a style stops it.
        let o = Options {
            comma: Comma::Leading,
            ..Options::for_style(Style::Dbt)
        };
        assert_eq!(
            fmt("SELECT a, b FROM t", &o),
            "select\n    a\n    , b\nfrom t;\n"
        );
    }

    // ----- switching the formatter off -----------------------------------------

    #[test]
    fn a_region_between_off_and_on_is_emitted_as_written() {
        let sql = "select   1;\n-- grebe: format off\nselect   a,b\n   from t ;  -- as is\n\n\nNOT   SQL (;\n-- grebe: format on\nselect   2;\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "SELECT 1;\n-- grebe: format off\nselect   a,b\n   from t ;  -- as is\n\n\nNOT   SQL (;\n-- grebe: format on\nSELECT 2;\n"
        );
        // The other spelling, any letter case, indented, trailing space.
        let sql = "select   1;\n\n  -- FMT: Off\n\tselect   2 ;\n  -- fmt:on  \n\nselect   3;\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "SELECT 1;\n\n  -- FMT: Off\n\tselect   2 ;\n  -- fmt:on  \n\nSELECT 3;\n"
        );
    }

    #[test]
    fn an_unterminated_off_runs_to_the_end_of_the_file() {
        let sql = "select   1;\n-- fmt: off\nselect   2 ;\n\n  select 3  \n\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "SELECT 1;\n-- fmt: off\nselect   2 ;\n\n  select 3  \n\n"
        );
        // A file that does not end with a line break gets one.
        assert_eq!(
            fmt("-- fmt: off\nselect   2 ;", &Options::default()),
            "-- fmt: off\nselect   2 ;\n"
        );
        // An `on` with no `off` before it is a comment like any other.
        assert_eq!(
            fmt("-- fmt: on\nselect   1", &Options::default()),
            "-- fmt: on\nSELECT 1;\n"
        );
    }

    /// The statement is the unit: one that holds a marker, or reaches into
    /// a region from either side, is left as written from its first token
    /// to its terminator, and its neighbours are formatted.
    #[test]
    fn a_statement_that_reaches_into_a_region_is_left_whole() {
        let sql = "select   1;\nselect a,\n-- fmt: off\n   b,   c\n-- fmt: on\n  from   t;  -- kept\nselect   2;\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "SELECT 1;\nselect a,\n-- fmt: off\n   b,   c\n-- fmt: on\n  from   t;  -- kept\nSELECT 2;\n"
        );
        // Only the `off` inside it.
        let sql = "select   1;\n\n  select a,\n-- fmt: off\n   b from t;\nselect   2;\n-- fmt: on\nselect   3;\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "SELECT 1;\n\n  select a,\n-- fmt: off\n   b from t;\nselect   2;\n-- fmt: on\nSELECT 3;\n"
        );
        // Only the `on` inside it, and code on the lines it shares.
        let sql = "select   1; select a,\n-- fmt: off\n   b\n-- fmt: on\n  from t; select   2;\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "SELECT 1;\nselect a,\n-- fmt: off\n   b\n-- fmt: on\n  from t;\nSELECT 2;\n"
        );
    }

    /// What is around a region is formatted with the options in force, and
    /// keeps the blank lines the author left next to the region, up to the
    /// number that separates statements.
    #[test]
    fn the_code_around_a_region_is_formatted_as_ever() {
        let sql = "select   a, b from t;\n\n\n\n-- fmt: off\nselect   2 ;\n-- fmt: on\n\n\n\nselect   3;\n-- fmt: off\nselect   4 ;\n-- fmt: on\nselect   5;\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "SELECT a, b FROM t;\n\n-- fmt: off\nselect   2 ;\n-- fmt: on\n\nSELECT 3;\n-- fmt: off\nselect   4 ;\n-- fmt: on\nSELECT 5;\n"
        );
        let o = opts(|o| {
            o.keyword_case = KeywordCase::Lower;
            o.blank_lines_between_statements = 0;
            o.list_layout = ListLayout::Expanded;
        });
        assert_eq!(
            fmt(sql, &o),
            "select\n    a,\n    b\nfrom t;\n-- fmt: off\nselect   2 ;\n-- fmt: on\nselect 3;\n-- fmt: off\nselect   4 ;\n-- fmt: on\nselect 5;\n"
        );
        // Comments next to a region stay where they are.
        let sql = "select   1; -- one\n-- about two\n-- fmt: off\nselect   2 ;\n-- fmt: on\n-- about three\nselect   3;\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "SELECT 1; -- one\n-- about two\n-- fmt: off\nselect   2 ;\n-- fmt: on\n-- about three\nSELECT 3;\n"
        );
    }

    /// A marker is a line comment on a line of its own. Anything else that
    /// spells one is not, and the lint suppression comments are another
    /// feature altogether.
    #[test]
    fn only_a_line_comment_on_its_own_line_switches_the_formatter_off() {
        for (src, want) in [
            (
                "select   1; -- fmt: off\nselect   2;",
                "SELECT 1; -- fmt: off\n\nSELECT 2;\n",
            ),
            ("/* fmt: off */\nselect   1;", "/* fmt: off */ SELECT 1;\n"),
            (
                "select '\n-- fmt: off\n',   1;",
                "SELECT '\n-- fmt: off\n', 1;\n",
            ),
            (
                "-- fmt: off please\nselect   1;",
                "-- fmt: off please\nSELECT 1;\n",
            ),
            (
                "-- grebe: ignore[MOD001]\nselect   count(*) from t;",
                "-- grebe: ignore[MOD001]\nSELECT count(*)\nFROM t;\n",
            ),
            (
                "-- grebe: ignore[MOD001]\n-- fmt: off\nselect   count(*) from t;\n-- fmt: on\nselect   1; -- grebe: ignore\n",
                "-- grebe: ignore[MOD001]\n-- fmt: off\nselect   count(*) from t;\n-- fmt: on\nSELECT 1; -- grebe: ignore\n",
            ),
        ] {
            assert_eq!(fmt(src, &Options::default()), want, "{src:?}");
        }
    }

    #[test]
    fn a_region_survives_a_byte_order_mark_and_windows_line_ends() {
        let sql =
            "\u{feff}select   1;\r\n-- fmt: off\r\nselect   2 ;\r\n-- fmt: on\r\nselect   3;\r\n";
        assert_eq!(
            fmt(sql, &Options::default()),
            "\u{feff}SELECT 1;\n-- fmt: off\r\nselect   2 ;\r\n-- fmt: on\r\nSELECT 3;\n"
        );
    }

    /// Every combination of the style options: formatting the output again
    /// changes nothing, only space, case, list commas and terminators differ
    /// from the input, and every comment is still there.
    #[test]
    fn every_style_option_combination_is_idempotent_and_keeps_the_tokens() {
        use grebe_syntax::token::{TokenKind, tokenize};
        // The code tokens in order, and the comments, which may change
        // places with a comma or an operator but not go.
        fn squeeze(s: &str) -> (String, Vec<String>) {
            let mut code = String::new();
            let mut comments = Vec::new();
            for t in tokenize(s) {
                let text = &s[t.span.start as usize..t.span.end as usize];
                match t.kind {
                    TokenKind::Whitespace => {}
                    TokenKind::LineComment | TokenKind::BlockComment => {
                        comments.push(text.trim_end().to_string());
                    }
                    _ if text == "," || text == ";" => {}
                    _ => code.push_str(&text.to_lowercase()),
                }
            }
            comments.sort();
            (code, comments)
        }
        let sources = [
            CALLS_AND_KEYWORDS,
            NAMES,
            SPACED_OUT,
            "with a as (select 1 as x from t), -- first\n\nb as (select x from a) select a, -- one\n b /* two */, c from b where x = 1 -- why\n and (y = 2 or z in (1, 2, 3)) group by a, b, c order by a, b",
            "select case when a = 1 then 'one' when a = 2 then 'two' else 'many' end, case x when 1 then 2 end from t; select 2 -- c\n; ;",
            "insert into t (a, b) values (1, 2), -- row\n\n (3, 4), /* last */ (5, 6) on conflict do nothing returning (a)",
            "create table T (Id Integer primary key, -- key\n Amount Decimal(18, 2) check (amount > 0),\n\n -- tags\n Tags VarChar[],)",
            "select a\n -- above the comma\n , b,\n /* block */ c, d -- last\n from t where a -- first\n and b\n -- above the operator\n or c",
            "select 1;\n-- fmt: off\nselect   2 ;\n-- fmt: on\nTHIS IS NOT SQL;\nselect count(*) from t where a in (1) -- c\n",
            "update t set a = 1, b = 2 where id = 3 and x = 4; delete from t where a and b or c",
        ];
        let cases = [NameCase::Lower, NameCase::Upper, NameCase::Preserve];
        let mut combinations = 0;
        for comma in [Comma::Trailing, Comma::Leading] {
            for logical_operator in [LogicalOperator::Leading, LogicalOperator::Trailing] {
                for (k, function_case) in cases.into_iter().enumerate() {
                    let type_case = cases[(k + 1) % 3];
                    for indent_style in [IndentStyle::Space, IndentStyle::Tab] {
                        for blank_lines in [(1, 0), (0, 2), (2, 1)] {
                            for list_layout in [ListLayout::Auto, ListLayout::Expanded] {
                                for case_layout in [CaseLayout::Auto, CaseLayout::Expanded] {
                                    for inline_threshold in [20, 100] {
                                        let opts = Options {
                                            inline_threshold,
                                            comma,
                                            logical_operator,
                                            function_case,
                                            type_case,
                                            indent_style,
                                            blank_lines_between_statements: blank_lines.0,
                                            max_blank_lines_in_statement: blank_lines.1,
                                            list_layout,
                                            case_layout,
                                            ..Options::default()
                                        };
                                        combinations += 1;
                                        for sql in sources {
                                            let once = format(sql, &opts);
                                            assert_eq!(
                                                format(&once, &opts),
                                                once,
                                                "{opts:?}\n{sql}"
                                            );
                                            assert_eq!(
                                                squeeze(&once),
                                                squeeze(sql),
                                                "{opts:?}\n{sql}"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(combinations, 576);
    }
}
