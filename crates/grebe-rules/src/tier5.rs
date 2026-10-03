//! Tier 5 CST detectors: MOD035 (wrapped-date-filter), MOD036
//! (row-at-a-time-insert), MOD037 (order-by-random-sample), MOD038
//! (sample-before-where), MOD039 (delete-then-insert), MOD040
//! (csv-full-sniff).
//!
//! This tier is about SQL that returns the right rows the slow way: a
//! spelling that stops DuckDB from using what it already knows about the
//! data. Every rule here is measured, not assumed -- DuckDB's optimizer
//! rewrites some slow-looking spellings into fast ones, and a rule that
//! flags those is noise.
//!
//! Production names are those of the vendored DuckDB grammar; `grebe tree`
//! prints them for any statement.

use std::collections::HashSet;
use std::sync::LazyLock;

use grebe_syntax::cst::{NodeId, Tree};

use crate::detect::Finding;

/// The single child of `id` produced by `rule`, if there is exactly one.
///
/// A private copy of the identical helper in the other tier modules: each
/// keeps its helpers private, a small deliberate duplication rather than a
/// shared import.
fn child(tree: &Tree, id: NodeId, rule: &str) -> Option<NodeId> {
    let mut found = None;
    for &c in tree.children(id) {
        if tree.rule_name(c) == rule {
            if found.is_some() {
                return None;
            }
            found = Some(c);
        }
    }
    found
}

/// Walk a strict single-child chain from `n` to a node produced by
/// `target`; `None` at the first fork.
fn descend_single(tree: &Tree, mut n: NodeId, target: &str) -> Option<NodeId> {
    loop {
        if tree.rule_name(n) == target {
            return Some(n);
        }
        let children = tree.children(n);
        if children.len() != 1 {
            return None;
        }
        n = children[0];
    }
}

/// Functions that compute a value *from* a timestamp: a part of it, a
/// formatting of it, or a conversion of it. A filter on their result cannot
/// be checked against a row group's min/max timestamp, so every row group
/// is read. `date_trunc` is deliberately absent: DuckDB rewrites a filter
/// on it into a range of the column itself.
const DATE_DERIVING_FNS: &[&str] = &[
    "year",
    "month",
    "day",
    "dayofmonth",
    "quarter",
    "week",
    "weekofyear",
    "yearweek",
    "isoyear",
    "dayofweek",
    "isodow",
    "dayofyear",
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "date_part",
    "datepart",
    "strftime",
    "epoch",
    "epoch_ms",
    "epoch_us",
    "epoch_ns",
    "monthname",
    "dayname",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wrap {
    /// A part, formatting or conversion of the column: slow for any
    /// comparison.
    Derived,
    /// The column cast to DATE: DuckDB prunes `=` but not ranges or `IN`.
    CastToDate,
}

fn is_rule(tree: &Tree, n: NodeId, rules: &[&str]) -> bool {
    rules.contains(&tree.rule_name(n))
}

fn reads_column(tree: &Tree, n: NodeId) -> bool {
    tree.rule_name(n) == "ColumnReference"
        || tree
            .descendants(n)
            .iter()
            .any(|&d| tree.rule_name(d) == "ColumnReference")
}

/// A side made only of constants: no column, no subquery.
fn is_constant(tree: &Tree, n: NodeId) -> bool {
    !reads_column(tree, n)
        && !tree.descendants(n).iter().any(|&d| {
            let r = tree.rule_name(d);
            r.starts_with("Select") || r.contains("Subquery")
        })
}

fn is_date_type(tree: &Tree, ty: NodeId, src: &str) -> bool {
    tree.text(ty, src).trim().eq_ignore_ascii_case("DATE")
}

/// What `side` does to a single column before it is compared, if it is one
/// of the shapes this rule knows to defeat pruning.
fn wrap_of(tree: &Tree, side: NodeId, src: &str) -> Option<Wrap> {
    let columns = std::iter::once(side)
        .chain(tree.descendants(side))
        .filter(|&d| tree.rule_name(d) == "ColumnReference")
        .count();
    if columns != 1 {
        return None;
    }
    // Walk down the pass-through chain to the first node that does work.
    let mut n = side;
    loop {
        match tree.rule_name(n) {
            "ExtractExpression" => return Some(Wrap::Derived),
            "FunctionExpression" => {
                let name = child(tree, n, "FunctionIdentifier")?;
                let name = tree.text(name, src).trim().to_ascii_lowercase();
                let name = name.rsplit('.').next().unwrap_or_default();
                return DATE_DERIVING_FNS.contains(&name).then_some(Wrap::Derived);
            }
            "CastExpression" => {
                let args = child(tree, n, "CastArguments")?;
                let ty = child(tree, args, "Type")?;
                return is_date_type(tree, ty, src).then_some(Wrap::CastToDate);
            }
            _ => {}
        }
        let children = tree.children(n);
        if let [only] = children[..] {
            n = only;
            continue;
        }
        // `col::DATE`: the column followed by a cast indirection.
        let cast_to_date = tree.descendants(n).into_iter().any(|d| {
            tree.rule_name(d) == "CastOperator"
                && child(tree, d, "Type").is_some_and(|ty| is_date_type(tree, ty, src))
        });
        let plain_column = children
            .iter()
            .any(|&c| descend_single(tree, c, "ColumnReference").is_some());
        return (cast_to_date && plain_column).then_some(Wrap::CastToDate);
    }
}

/// Is `node` part of a WHERE condition of its own query, rather than of a
/// select list or a subquery's other clauses?
fn in_where(tree: &Tree, node: NodeId) -> bool {
    let mut cur = tree.parent(node);
    while let Some(n) = cur {
        match tree.rule_name(n) {
            "WhereClause" => return true,
            "SimpleSelect" | "SelectClause" | "HavingClause" | "JoinClause" => return false,
            _ => cur = tree.parent(n),
        }
    }
    false
}

/// MOD035 — a WHERE filter that wraps a timestamp column in a function
/// DuckDB cannot see through, so it reads every row group.
///
/// Measured on a 20M-row time-ordered table: `year(ts) = 2023` read all 20M
/// rows where `ts >= '2023-01-01' AND ts < '2024-01-01'` read the year's
/// 4.1M and ran 7x faster; `strftime(ts, '%Y') = '2023'` was 74x slower.
/// What DuckDB already optimizes is excluded: `date_trunc(...)` compared
/// any way, and `CAST(ts AS DATE) = constant`, prune as well as a range.
/// A cast to DATE is flagged only with a range operator or `IN`.
///
/// Only comparisons against constants count -- a comparison of two columns
/// has nothing to prune by -- and only in a WHERE. Detect-only: the right
/// range depends on the column's type and time zone, which the linter
/// cannot see.
pub fn wrapped_date_filter(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    // `a <op> b`: an OtherOperatorExpression with exactly one tail.
    for op_expr in tree.find("OtherOperatorExpression") {
        let [subject, tail] = tree.children(op_expr)[..] else {
            continue;
        };
        if tree.rule_name(tail) != "OtherOperatorTail" || !in_where(tree, op_expr) {
            continue;
        }
        let [operator, other] = tree.children(tail)[..] else {
            continue;
        };
        let op = tree.text(operator, src).trim();
        if !matches!(op, "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=") || !is_constant(tree, other) {
            continue;
        }
        let flagged = match wrap_of(tree, subject, src) {
            Some(Wrap::Derived) => true,
            Some(Wrap::CastToDate) => !matches!(op, "=" | "<>" | "!="),
            None => false,
        };
        if flagged {
            out.push(Finding {
                code: "MOD035",
                span: tree.node(op_expr).span,
                fix: None,
            });
        }
    }
    // `a BETWEEN x AND y` and `a IN (...)`.
    for between in tree.find("BetweenInLikeExpression") {
        let [subject, op] = tree.children(between)[..] else {
            continue;
        };
        if !is_rule(tree, op, &["BetweenInLikeOp"]) || !in_where(tree, between) {
            continue;
        }
        let negated = tree
            .text(op, src)
            .split_whitespace()
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("NOT"));
        let Some(kind) = tree.descendants(op).into_iter().find(|&d| {
            is_rule(tree, d, &["BetweenClause", "InClause"])
                && tree.ancestor(d, "BetweenInLikeOp") == Some(op)
        }) else {
            continue; // LIKE and friends
        };
        let operands_constant = tree.children(kind).iter().all(|&c| is_constant(tree, c));
        if negated || !operands_constant {
            continue;
        }
        if wrap_of(tree, subject, src).is_some() {
            out.push(Finding {
                code: "MOD035",
                span: tree.node(between).span,
                fix: None,
            });
        }
    }
    out
}

/// Aggregate function names, from the same vendored list the other tiers
/// read.
static AGGREGATE_FUNCTIONS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    include_str!("../vendor/aggregates.list")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect()
});

/// How many consecutive single-row INSERTs into one table make a run worth
/// a finding. Each statement costs about half a millisecond of overhead, so
/// short runs are harmless; the corpus never has more than 3, while a load
/// script has dozens to thousands.
const INSERT_RUN_THRESHOLD: usize = 10;

/// The target of a top-level `INSERT ... VALUES` with exactly one row.
fn single_row_insert_target(tree: &Tree, statement: NodeId, src: &str) -> Option<String> {
    let insert = descend_single(tree, statement, "InsertStatement")?;
    let values = tree.descendants(insert).into_iter().find(|&d| {
        tree.rule_name(d) == "ValuesClause" && tree.ancestor(d, "InsertStatement") == Some(insert)
    })?;
    let rows = tree
        .children(values)
        .iter()
        .filter(|&&c| tree.rule_name(c) == "ValuesExpressions")
        .count();
    if rows != 1 {
        return None;
    }
    let target = child(tree, insert, "InsertTarget")?;
    Some(
        tree.text(target, src)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase(),
    )
}

/// MOD036 — a run of consecutive single-row `INSERT ... VALUES` statements
/// into one table.
///
/// Executed: loading 10,000 rows took 5.1 s as 10,000 single-row INSERTs,
/// 0.10 s as one multi-row INSERT (50x) and 2.8 ms as `INSERT ... SELECT`
/// from a generated source (1,800x). The finding sits on the run's first
/// statement. Detect-only: separate statements succeed or fail one at a
/// time, one merged statement all at once, so merging is the author's call.
pub fn row_at_a_time_insert(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for program in tree.find("Program") {
        let mut run: Option<(String, NodeId, usize)> = None;
        let mut close = |run: &mut Option<(String, NodeId, usize)>| {
            if let Some((_, first, len)) = run.take() {
                if len >= INSERT_RUN_THRESHOLD {
                    out.push(Finding {
                        code: "MOD036",
                        span: tree.node(first).span,
                        fix: None,
                    });
                }
            }
        };
        for &statement in tree.children(program) {
            if tree.rule_name(statement) != "TopLevelStatement" {
                continue;
            }
            match single_row_insert_target(tree, statement, src) {
                Some(target) if run.as_ref().is_some_and(|(t, _, _)| *t == target) => {
                    if let Some((_, _, len)) = run.as_mut() {
                        *len += 1;
                    }
                }
                Some(target) => {
                    close(&mut run);
                    run = Some((target, statement, 1));
                }
                None => close(&mut run),
            }
        }
        close(&mut run);
    }
    out
}

/// MOD037 — `ORDER BY random() LIMIT n` to draw a random sample.
///
/// Executed on 20M rows: 148 ms, against 36-38 ms for `USING SAMPLE n ROWS`,
/// which returns the same kind of sample (uniform, exactly n rows). The
/// rewrite is equivalent only where nothing runs between the FROM and the
/// sample: `USING SAMPLE` samples before `WHERE` filters (executed: 9 rows
/// back, not 1,000), and before grouping, `DISTINCT`, aggregates and window
/// functions. Any of those, an `OFFSET`, a set operation, or another
/// ordering key, and this does not fire. Detect-only: the sample is random,
/// so there is no result to compare a rewrite against.
pub fn order_by_random_sample(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for modifiers in tree.find("ResultModifiers") {
        let (Some(order), Some(limit)) = (
            child(tree, modifiers, "OrderByClause"),
            child(tree, modifiers, "LimitOffset"),
        ) else {
            continue;
        };
        let keys: Vec<NodeId> = tree
            .descendants(order)
            .into_iter()
            .filter(|&d| tree.rule_name(d) == "OrderByExpression")
            .collect();
        let [key] = keys[..] else {
            continue;
        };
        let key_text = tree
            .text(key, src)
            .split_whitespace()
            .collect::<String>()
            .to_ascii_lowercase();
        if !matches!(
            key_text.as_str(),
            "random()" | "random()asc" | "random()desc"
        ) {
            continue;
        }
        if tree
            .text(limit, src)
            .split_whitespace()
            .any(|w| w.eq_ignore_ascii_case("OFFSET"))
        {
            continue;
        }
        // The query the modifiers apply to is their sibling; a set operation
        // forks on the way down and is skipped.
        let Some(select) = tree.parent(modifiers).and_then(|p| {
            tree.children(p)
                .iter()
                .copied()
                .filter(|&c| c != modifiers)
                .find_map(|c| descend_single(tree, c, "SimpleSelect"))
        }) else {
            continue;
        };
        let filters_or_groups = tree.children(select).iter().any(|&c| {
            matches!(
                tree.rule_name(c),
                "WhereClause"
                    | "GroupByClause"
                    | "HavingClause"
                    | "QualifyClause"
                    | "WindowClause"
                    | "SampleClause"
            )
        });
        let reshapes_rows = tree
            .descendants(select)
            .iter()
            .any(|&d| match tree.rule_name(d) {
                "DistinctClause" | "OverClause" => true,
                "FunctionExpression" => child(tree, d, "FunctionIdentifier").is_some_and(|id| {
                    AGGREGATE_FUNCTIONS
                        .contains(tree.text(id, src).trim().to_ascii_lowercase().as_str())
                }),
                _ => false,
            });
        let has_from = tree
            .descendants(select)
            .iter()
            .any(|&d| tree.rule_name(d) == "FromClause");
        if !filters_or_groups && !reshapes_rows && has_from {
            out.push(Finding {
                code: "MOD037",
                span: tree.node(modifiers).span,
                fix: None,
            });
        }
    }
    out
}

/// MOD038 — `WHERE ... USING SAMPLE n ROWS`: the sample is taken before the
/// WHERE filters, so far fewer than n rows come back.
///
/// Executed on 1M rows where 1% match: `WHERE v = 1 USING SAMPLE 1000 ROWS`
/// returned 9 rows. Only a fixed row count is flagged; a percentage keeps
/// the same fraction whether taken before or after filtering, and is the
/// usual way to approximate an aggregate. `TABLESAMPLE` on a table is not
/// flagged: written on the table, it reads as what it does.
pub fn sample_before_where(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for sample in tree.find("SampleClause") {
        let has_where = tree
            .parent(sample)
            .is_some_and(|select| child(tree, select, "WhereClause").is_some());
        let counts_rows = tree.descendants(sample).iter().any(|&d| {
            tree.rule_name(d) == "SampleUnit"
                && tree
                    .text(d, src)
                    .trim()
                    .to_ascii_uppercase()
                    .starts_with("ROW")
        });
        if has_where && counts_rows {
            out.push(Finding {
                code: "MOD038",
                span: tree.node(sample).span,
                fix: None,
            });
        }
    }
    out
}

/// Lowercased, whitespace-normalized text of the first table name under
/// `node`.
fn table_name(tree: &Tree, node: NodeId, src: &str) -> Option<String> {
    let name = std::iter::once(node)
        .chain(tree.descendants(node))
        .find(|&d| tree.rule_name(d) == "BaseTableName")?;
    Some(
        tree.text(name, src)
            .split_whitespace()
            .collect::<String>()
            .to_ascii_lowercase(),
    )
}

/// MOD039 — `DELETE FROM t WHERE ...` immediately followed by `INSERT INTO
/// t ... SELECT`, both reading the same source table: an upsert spelled as
/// two statements.
///
/// Executed for 1M changed rows into 10M: DELETE + INSERT took 122 ms,
/// `MERGE INTO` 56 ms (2.2x) as one atomic statement with no key required;
/// and with the INSERT failing outside a transaction, the DELETE stayed
/// committed and 500,000 rows were lost. `INSERT OR REPLACE` / `ON
/// CONFLICT` are not the suggestion: they need a PRIMARY KEY, which made
/// them 3x slower than the keyless DELETE + INSERT. Fires inside a
/// transaction too, for the speed. A DELETE without WHERE is a full reload,
/// not an upsert, and is left alone. Detect-only: the MERGE's ON condition
/// and column mapping are the author's.
pub fn delete_then_insert(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for program in tree.find("Program") {
        let statements: Vec<NodeId> = tree
            .children(program)
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "TopLevelStatement")
            .collect();
        for pair in statements.windows(2) {
            let (Some(delete), Some(insert)) = (
                descend_single(tree, pair[0], "DeleteStatement"),
                descend_single(tree, pair[1], "InsertStatement"),
            ) else {
                continue;
            };
            if child(tree, delete, "WhereClause").is_none() {
                continue;
            }
            let deleted =
                child(tree, delete, "TargetOptAlias").and_then(|t| table_name(tree, t, src));
            let inserted =
                child(tree, insert, "InsertTarget").and_then(|t| table_name(tree, t, src));
            if deleted.is_none() || deleted != inserted {
                continue;
            }
            // An upsert reads its changes from somewhere: the DELETE's
            // condition and the INSERT's SELECT must share a source table.
            // Without that link this is an unrelated delete and insert (the
            // DuckDB docs' transaction example deletes one person and adds
            // another), and a single-row VALUES insert has no source at all.
            let sources = |stmt: NodeId, own_target: Option<NodeId>| -> HashSet<String> {
                tree.descendants(stmt)
                    .into_iter()
                    .filter(|&d| tree.rule_name(d) == "BaseTableName")
                    .filter(|&d| {
                        own_target.is_none_or(|t| d != t && !tree.descendants(t).contains(&d))
                    })
                    .map(|d| {
                        tree.text(d, src)
                            .split_whitespace()
                            .collect::<String>()
                            .to_ascii_lowercase()
                    })
                    .collect()
            };
            let delete_reads = sources(delete, child(tree, delete, "TargetOptAlias"));
            let insert_reads = sources(insert, child(tree, insert, "InsertTarget"));
            if !delete_reads.is_disjoint(&insert_reads) {
                out.push(Finding {
                    code: "MOD039",
                    span: tree.node(delete).span,
                    fix: None,
                });
            }
        }
    }
    out
}

/// MOD040 — `read_csv(..., sample_size = -1)`: the sniffer reads the whole
/// file to guess column types before the query starts.
///
/// Executed on a 5M-row CSV: 3.5 s against 0.23 s with the default sample
/// (15x). The usual reason for it is a type the default sample guessed
/// wrong; declaring the types (`columns = {...}` or `types = {...}`) fixes
/// that without reading the file twice. Detect-only: the types are the
/// author's.
pub fn csv_full_sniff(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for function in tree.find("TableFunction") {
        let is_csv_reader = tree.descendants(function).into_iter().any(|d| {
            tree.rule_name(d) == "TableFunctionName"
                && matches!(
                    tree.text(d, src).trim().to_ascii_lowercase().as_str(),
                    "read_csv" | "read_csv_auto"
                )
        });
        if !is_csv_reader {
            continue;
        }
        let whole_file = tree.descendants(function).into_iter().any(|d| {
            tree.rule_name(d) == "FunctionArgument"
                && tree.ancestor(d, "TableFunction") == Some(function)
                && matches!(
                    tree.text(d, src)
                        .split_whitespace()
                        .collect::<String>()
                        .to_ascii_lowercase()
                        .as_str(),
                    "sample_size=-1" | "sample_size:=-1"
                )
        });
        if whole_file {
            out.push(Finding {
                code: "MOD040",
                span: tree.node(function).span,
                fix: None,
            });
        }
    }
    out
}

/// Every tier-5 detector; findings are sorted by position, then code.
pub fn analyze_tier5(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = wrapped_date_filter(tree, src);
    out.extend(row_at_a_time_insert(tree, src));
    out.extend(order_by_random_sample(tree, src));
    out.extend(sample_before_where(tree, src));
    out.extend(delete_then_insert(tree, src));
    out.extend(csv_full_sniff(tree, src));
    out.sort_by_key(|f| (f.span.start, f.code));
    out
}
