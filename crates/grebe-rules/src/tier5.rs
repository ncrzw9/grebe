//! Tier 5 CST detectors: MOD035 (wrapped-date-filter).
//!
//! This tier is about SQL that returns the right rows the slow way: a
//! spelling that stops DuckDB from using what it already knows about the
//! data. Every rule here is measured, not assumed -- DuckDB's optimizer
//! rewrites some slow-looking spellings into fast ones, and a rule that
//! flags those is noise.
//!
//! Production names are those of the vendored DuckDB grammar; `grebe tree`
//! prints them for any statement.

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

/// Every tier-5 detector; findings are sorted by position, then code.
pub fn analyze_tier5(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = wrapped_date_filter(tree, src);
    out.sort_by_key(|f| (f.span.start, f.code));
    out
}
