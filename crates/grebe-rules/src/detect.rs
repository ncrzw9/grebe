//! CST detectors for the MOD rules.
//!
//! Each detector reads the lossless CST directly.

use std::collections::HashSet;
use std::sync::OnceLock;

use grebe_syntax::Span;
use grebe_syntax::cst::{NodeId, Tree};

/// One finding: a rule code and a byte span.
///
/// Structured values only. Nothing downstream ever matches on message text,
/// which is why no message is carried here at all — the registry owns it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finding {
    pub code: &'static str,
    pub span: Span,
    /// The edit this finding's rule proposes, when it has one. Whether it is
    /// applied is decided by the rule's registry `fix_safety`, not here.
    pub fix: Option<Fix>,
}

/// A byte-range edit on the original buffer.
///
/// `replacement` is owned. Several fixes derive their text from the source
/// (reordering a comparison's operands, rewriting an alias), which cannot be
/// `&'static str` without leaking a string per finding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Fix {
    pub span: Span,
    pub replacement: String,
}

fn text<'s>(tree: &Tree, id: NodeId, src: &'s str) -> &'s str {
    tree.text(id, src)
}

/// The single child of `id` produced by `rule`, if there is exactly one.
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

/// Strip one layer of quoting from an identifier and lowercase it.
///
/// Comparison for MOD009 is case-insensitive and quote-insensitive on both
/// sides, so `"Foo"` and `foo` are the same identifier.
fn ident(raw: &str) -> String {
    let t = raw.trim();
    let unquoted = if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1].replace("\"\"", "\"")
    } else {
        t.to_string()
    };
    unquoted.to_ascii_lowercase()
}

/// The `id`'s nearest descendant produced by `rule` whose span exactly
/// covers `id` itself, if any — i.e. `id` *is* (through however many
/// single-child pass-through layers the expression grammar interposes) a
/// bare node of that kind, not merely one containing it.
///
/// Generalizes the span-equality trick `self_alias` uses to tell `a` from
/// `a + 1`: a wrapper chain (`Expression` → `LambdaArrowExpression` → ... →
/// `ColumnReference`) never changes span, but stepping into a real child
/// (the `a` inside `a + 1`) always shrinks it.
fn bare(tree: &Tree, id: NodeId, rule: &str) -> Option<NodeId> {
    let span = tree.node(id).span;
    tree.descendants(id)
        .into_iter()
        .find(|&d| tree.rule_name(d) == rule && tree.node(d).span == span)
}

/// Lowercase each dot-separated component of a (possibly qualified,
/// possibly quoted) reference and rejoin. `T.A` and `t.a` compare equal;
/// `"Foo".bar` and `foo.bar` do too.
fn dotted_ident(raw: &str) -> String {
    raw.trim()
        .split('.')
        .map(ident)
        .collect::<Vec<_>>()
        .join(".")
}

/// The `SelectClause` belonging directly to `simple_select` — never one
/// reached by wandering into a nested subquery's own `SimpleSelect`.
///
/// `SimpleSelect <- SelectFrom WhereClause? GroupByClause? ...` and
/// `SelectFrom <- SelectFromClause / FromSelectClause`, both of which carry
/// `SelectClause` as a direct child (`FromSelectClause`'s FROM-first form
/// makes it optional). Walking only direct children at each step, rather
/// than searching `descendants`, is what keeps a `FROM (subquery)`'s inner
/// `SelectClause` from being mistaken for this scope's own.
fn select_clause_of(tree: &Tree, simple_select: NodeId) -> Option<NodeId> {
    let from = child(tree, simple_select, "SelectFrom")?;
    let alt = *tree.children(from).first()?;
    child(tree, alt, "SelectClause")
}

/// Built-in aggregate function names, matched case-insensitively.
///
/// grebe does not embed the DuckDB engine, so there is no live
/// `duckdb_functions()` query to build this set at runtime. This is a
/// static, vendored replacement, in the same spirit as
/// `grebe-syntax/vendor/grammar/keywords`: checked in, not queried,
/// regenerated from a DuckDB build.
static AGGREGATES: &str = include_str!("../vendor/aggregates.list");

fn is_aggregate(lowercase_name: &str) -> bool {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    let set = SET.get_or_init(|| {
        AGGREGATES
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect()
    });
    set.contains(lowercase_name)
}

/// MOD001 count-star: `count(*)` → `count()`.
///
/// DuckDB's friendly SQL defines `count()` as the same count, so the
/// delete-the-star fix is safe.
///
/// The CST enforces most of the guard structurally: exactly
/// one positional argument, that argument a bare `StarExpression`. A
/// qualified star (`t.*`) carries a non-empty `StarQualifierList` and so
/// cannot match. `DISTINCT`/`ALL`, though, is a `DistinctOrAll` sibling of
/// the argument list inside `FunctionExpressionArgumentList` — it does not
/// touch the argument count or shape, so it must be checked explicitly.
/// `count(DISTINCT *)` counts distinct rows, not all rows, so it must not
/// fire (and must not get the delete-the-star fix, which would silently
/// rewrite it to `count(DISTINCT )`).
pub fn count_star(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for f in tree.find("FunctionExpression") {
        let Some(id) = child(tree, f, "FunctionIdentifier") else {
            continue;
        };
        if ident(text(tree, id, src)) != "count" {
            continue;
        }
        let Some(args) = child(tree, f, "FunctionExpressionArguments") else {
            continue;
        };
        let Some(arg_list) = child(tree, args, "FunctionExpressionArgumentList") else {
            continue;
        };
        if child(tree, arg_list, "DistinctOrAll").is_some() {
            continue;
        }
        // Exactly one argument, and it must be a bare star.
        let list: Vec<_> = tree
            .descendants(args)
            .into_iter()
            .filter(|&n| tree.rule_name(n) == "FunctionArgument")
            .collect();
        if list.len() != 1 {
            continue;
        }
        let stars: Vec<_> = tree
            .descendants(args)
            .into_iter()
            .filter(|&n| tree.rule_name(n) == "StarExpression")
            .collect();
        if stars.len() != 1 {
            continue;
        }
        // A bare star only: no qualifier, no EXCLUDE/REPLACE/RENAME.
        let star = stars[0];
        if ident(text(tree, star, src)) != "*" {
            continue;
        }
        // The star must BE the argument, not merely sit somewhere inside it.
        // `descendants` reaches any depth, so `count(COLUMNS(*))` -- one
        // argument, one StarExpression -- passes every check above, and
        // deleting its star would produce the invalid `count(COLUMNS())`.
        // That `*` belongs to DuckDB's `COLUMNS(*)` star-expression, not to
        // `count`. Span equality is the structural way to say "the whole
        // argument is the star": for `count(*)` the two nodes cover the same
        // bytes, for any wrapped star they cannot.
        if tree.node(star).span != tree.node(list[0]).span {
            continue;
        }
        let sp = tree.node(star).span;
        out.push(Finding {
            code: "MOD001",
            span: sp,
            // Smallest correct edit: delete the star. Collapsing the resulting
            // `count( )` is the formatter's job, never the linter's.
            fix: Some(Fix {
                span: sp,
                replacement: String::new(),
            }),
        });
    }
    out
}

/// MOD006 ifnull-coalesce: `IFNULL`/`NVL` → `COALESCE`.
///
/// Both are two-argument aliases for the standard `COALESCE`, so renaming
/// the function is a safe fix.
///
/// An identifier not followed by `(` parses as a column reference, never a
/// `FunctionExpression`, so no `(` lookahead guard is needed.
pub fn ifnull_coalesce(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for f in tree.find("FunctionExpression") {
        let Some(id) = child(tree, f, "FunctionIdentifier") else {
            continue;
        };
        let name = ident(text(tree, id, src));
        if name == "ifnull" || name == "nvl" {
            let sp = tree.node(id).span;
            out.push(Finding {
                code: "MOD006",
                span: sp,
                fix: Some(Fix {
                    span: sp,
                    replacement: "coalesce".to_string(),
                }),
            });
        }
    }
    out
}

/// MOD007 natural-join: `NATURAL JOIN` re-pairs silently when schemas change.
///
/// `NaturalJoinPrefix` only matches in join-prefix position, so a column or
/// alias named `natural` never reaches this node. No fix: spelling out the
/// join keys needs both schemas, which this tool does not have.
pub fn natural_join(tree: &Tree, _src: &str) -> Vec<Finding> {
    tree.find("NaturalJoinPrefix")
        .into_iter()
        .map(|n| Finding {
            code: "MOD007",
            span: tree.node(n).span,
            fix: None,
        })
        .collect()
}

/// MOD009 self-alias: `expr AS x` where `x` is `expr`'s own trailing identifier.
///
/// Only fires when the expression side resolves to a column reference; its
/// trailing identifier is compared with the alias. For a qualified reference
/// that is the last dotted component.
pub fn self_alias(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for n in tree.find("ExpressionAsCollabel") {
        let Some(label) = child(tree, n, "ColLabelOrString") else {
            continue;
        };
        let alias = ident(text(tree, label, src));
        if alias.is_empty() {
            continue;
        }
        let Some(expr) = child(tree, n, "Expression") else {
            continue;
        };

        // The expression side must BE a column reference, not merely end in
        // one. `fib.a + fib.next AS next` ends in an identifier equal to the
        // alias but is a computation, and naming a computation is not a
        // redundant alias. Requiring a ColumnReference whose span covers the
        // whole `Expression` child encodes that structurally, where
        // comparing the trailing identifier text cannot. Reading the child's
        // own span (rather than slicing source text between it and the
        // label) also means a comment between the expression and `AS`, or
        // between `AS` and the alias, can't throw the comparison off: the
        // tree is lossless, so the child's span is exact regardless of what
        // sits around it.
        let expr_span = tree.node(expr).span;
        let is_bare_ref = tree
            .descendants(n)
            .into_iter()
            .any(|d| tree.rule_name(d) == "ColumnReference" && tree.node(d).span == expr_span);
        if !is_bare_ref {
            continue;
        }

        // Compare the reference's last dotted component, case- and
        // quote-insensitively.
        let expr_text = text(tree, expr, src);
        let last = expr_text.trim().rsplit('.').next().unwrap_or("");
        if !last.is_empty() && ident(last) == alias {
            out.push(Finding {
                code: "MOD009",
                span: tree.node(n).span,
                // Delete the alias clause: from the end of the expression's
                // last token through the end of the alias token, taking
                // `" AS foo"` (or similar, comments and all) with it. Safe
                // because the alias was proven to equal what the engine
                // would infer anyway.
                fix: Some(Fix {
                    span: Span::new(expr_span.end, tree.node(label).span.end),
                    replacement: String::new(),
                }),
            });
        }
    }
    out
}

/// MOD020 plain-big-number: `1000000` → `1_000_000`. Opt-in because it is
/// pure readability style; detect-only.
///
/// Fires on seven or more digits, i.e. from one million up. The
/// all-ASCII-digits check is the entire guard and does triple duty: it
/// excludes floats, hex/binary/scientific literals, and anything already
/// underscored — which makes the rule idempotent for free.
pub fn plain_big_number(tree: &Tree, src: &str) -> Vec<Finding> {
    tree.find("NumberLiteral")
        .into_iter()
        .filter(|&n| {
            let t = text(tree, n, src).trim();
            t.len() >= 7 && t.bytes().all(|b| b.is_ascii_digit())
        })
        .map(|n| Finding {
            code: "MOD020",
            span: tree.node(n).span,
            fix: None,
        })
        .collect()
}

/// MOD003 group-by-all: an explicit `GROUP BY` list that names exactly the
/// non-aggregate plain-column select items → `GROUP BY ALL`.
///
/// Walks every `SimpleSelect` in the tree (top-level and every nested one —
/// `find` reaches into subqueries, and each arm of a set-operation chain is
/// its own `SimpleSelect` with its own optional `GroupByClause`, so this
/// alone is "every scope" for a rule whose clauses live at that grain).
///
/// Four independently load-bearing guards: (1) a
/// `GROUP BY ALL` already in place is skipped, not re-flagged; (2) any
/// window expression anywhere in the select list aborts the rule for this
/// scope entirely; (3) any select item that is neither a bare column nor a
/// known aggregate call — a computed expression like `a + 1` — aborts the
/// scope entirely, not just excludes that item; (4) the `GROUP BY` list
/// itself must reduce entirely to bare column references (this also
/// absorbs `CUBE`/`ROLLUP`/`GROUPING SETS`/an empty grouping item, which
/// use a different `GroupByExpression` alternative and so never satisfy
/// it). Guards (2) and (3) are "abort the whole rule for that statement",
/// not "skip this item" — get that wrong and the rule fires on a `GROUP
/// BY ALL`-equivalent list while ignoring a window function or computed
/// column sitting right next to it.
pub fn group_by_all(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for simple_select in tree.find("SimpleSelect") {
        let Some(group_by) = child(tree, simple_select, "GroupByClause") else {
            continue;
        };
        let Some(group_exprs) = child(tree, group_by, "GroupByExpressions") else {
            continue;
        };
        let Some(&alt) = tree.children(group_exprs).first() else {
            continue;
        };
        // Guard (1): `GroupByExpressions <- GroupByList / GroupByAll` --
        // already `ALL` is not something to re-flag.
        if tree.rule_name(alt) != "GroupByList" {
            continue;
        }

        // Guard (4): every item must reduce to a bare column reference.
        // `GroupByExpression <- EmptyGroupingItem / CubeOrRollupClause /
        // GroupingSetsClause / GroupByBaseExpression` -- anything but the
        // last alternative, or a `GroupByBaseExpression` that isn't itself
        // a bare `ColumnReference` (e.g. `GROUP BY upper(a)`), aborts.
        let mut group_names = Vec::new();
        let mut group_list_ok = true;
        for &item in tree
            .children(alt)
            .iter()
            .filter(|&&n| tree.rule_name(n) == "GroupByExpression")
        {
            let Some(&inner) = tree.children(item).first() else {
                group_list_ok = false;
                break;
            };
            let Some(expr) = (tree.rule_name(inner) == "GroupByBaseExpression")
                .then(|| child(tree, inner, "Expression"))
                .flatten()
            else {
                group_list_ok = false;
                break;
            };
            let Some(col) = bare(tree, expr, "ColumnReference") else {
                group_list_ok = false;
                break;
            };
            group_names.push(dotted_ident(text(tree, col, src)));
        }
        if !group_list_ok || group_names.is_empty() {
            continue;
        }

        let Some(select_clause) = select_clause_of(tree, simple_select) else {
            continue;
        };
        let Some(target_list) = child(tree, select_clause, "TargetList") else {
            continue;
        };

        // Guards (2) and (3): walk the select list once, aborting the
        // whole scope (not just skipping the offending item) on the first
        // window expression or computed non-aggregate item.
        let mut select_names = Vec::new();
        let mut aborted = false;
        for &item in tree
            .children(target_list)
            .iter()
            .filter(|&&n| tree.rule_name(n) == "AliasedExpression")
        {
            let Some(&item_alt) = tree.children(item).first() else {
                aborted = true;
                break;
            };
            let Some(expr) = child(tree, item_alt, "Expression") else {
                aborted = true;
                break;
            };
            if tree
                .descendants(expr)
                .into_iter()
                .any(|d| tree.rule_name(d) == "OverClause")
            {
                aborted = true;
                break;
            }
            if let Some(func) = bare(tree, expr, "FunctionExpression") {
                let Some(fid) = child(tree, func, "FunctionIdentifier") else {
                    aborted = true;
                    break;
                };
                if is_aggregate(&ident(text(tree, fid, src))) {
                    continue;
                }
                aborted = true;
                break;
            }
            if let Some(col) = bare(tree, expr, "ColumnReference") {
                select_names.push(dotted_ident(text(tree, col, src)));
                continue;
            }
            aborted = true;
            break;
        }
        if aborted {
            continue;
        }

        group_names.sort();
        select_names.sort();
        if group_names != select_names {
            continue;
        }

        let sp = tree.node(group_by).span;
        out.push(Finding {
            code: "MOD003",
            span: sp,
            // The explicit list was just proven set-equal to what `ALL`
            // would infer, so the rewrite is safe. `GroupByClause`'s own
            // span already runs `GROUP` through the last list token --
            // diagnostic span and fix span coincide here, unlike MOD013.
            fix: Some(Fix {
                span: sp,
                replacement: "GROUP BY ALL".to_string(),
            }),
        });
    }
    out
}

/// MOD004 subquery-order-by: `ORDER BY` inside a subquery with no
/// `LIMIT`/`OFFSET` in that same subquery — the order is not guaranteed to
/// survive the outer query.
///
/// The scope-node list is the whole rule: `SubqueryReference` (covers both
/// table subqueries and scalar/EXISTS subqueries — they share the
/// production), `InSelectStatement` (`x IN (SELECT ...)`), and
/// `CTESelectBody` (a CTE's body). Deliberately **not** the top-level
/// `SelectStatementInternal` (its own `ORDER BY` legitimately controls
/// final output order) and **not** `SelectParens` (a set-operation arm's
/// `ORDER BY`/`LIMIT` lives in `ResultModifiers` shared across the whole
/// chain, not a genuinely nested scope — including it would misfire on an
/// arm whose sibling supplies the trailing `LIMIT`).
pub fn subquery_order_by(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for rule in ["SubqueryReference", "InSelectStatement", "CTESelectBody"] {
        for scope in tree.find(rule) {
            let Some(inner) = child(tree, scope, "SelectStatementInternal") else {
                continue;
            };
            let Some(modifiers) = child(tree, inner, "ResultModifiers") else {
                continue;
            };
            let Some(order_by) = child(tree, modifiers, "OrderByClause") else {
                continue;
            };
            if child(tree, modifiers, "LimitOffset").is_some() {
                continue;
            }
            out.push(Finding {
                code: "MOD004",
                span: tree.node(order_by).span,
                // No safe mechanical edit: the ORDER BY might be
                // intentional documentation of expected order even though
                // it isn't guaranteed to survive the outer query.
                fix: None,
            });
        }
    }
    out
}

/// MOD005 distinct-group-by: `SELECT DISTINCT ...` combined with `GROUP BY`
/// in the same query — one of the two is redundant, or the combination is
/// masking a bug. Deliberately no fix: genuinely ambiguous which is wrong.
///
/// `DistinctClause <- DistinctOn / DistinctAll`, and confusingly,
/// `DistinctAll <- 'ALL'` — that alternative is `SELECT ALL` (DISTINCT's
/// explicit opposite), not "plain DISTINCT". A bare `DISTINCT` with no
/// target list is `DistinctOn <- 'DISTINCT' DistinctOnTargets?` with
/// `DistinctOnTargets` absent; `DISTINCT ON (a)` is the same node with
/// `DistinctOnTargets` present. Because this production reads unusually,
/// the tests below cover all four of `SELECT DISTINCT`, `SELECT DISTINCT ON
/// (a)`, `SELECT ALL`, and plain `SELECT`; `grebe tree` shows each shape.
/// `DISTINCT ON` must not fire — it has order-dependent semantics, not a
/// redundant restatement of `GROUP BY`.
pub fn distinct_group_by(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for simple_select in tree.find("SimpleSelect") {
        if child(tree, simple_select, "GroupByClause").is_none() {
            continue;
        }
        let Some(select_clause) = select_clause_of(tree, simple_select) else {
            continue;
        };
        let Some(distinct_clause) = child(tree, select_clause, "DistinctClause") else {
            continue;
        };
        let Some(distinct_on) = child(tree, distinct_clause, "DistinctOn") else {
            // `DistinctAll` (`SELECT ALL`) -- the opposite of DISTINCT.
            continue;
        };
        if child(tree, distinct_on, "DistinctOnTargets").is_some() {
            continue;
        }
        out.push(Finding {
            code: "MOD005",
            span: tree.node(distinct_on).span,
            fix: None,
        });
    }
    out
}

/// Every detector in this module.
fn analyze_tier1(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(count_star(tree, src));
    out.extend(ifnull_coalesce(tree, src));
    out.extend(natural_join(tree, src));
    out.extend(self_alias(tree, src));
    out.extend(plain_big_number(tree, src));
    out
}

/// Every detector in the crate, across all tiers, in source order.
///
/// Every detector runs on every parsed statement, DDL included: there is no
/// statement-type gate. A detector that does not apply to a statement's shape
/// simply finds no matching nodes.
///
/// This is the single entry point the CLI calls; a tier module that is not
/// listed here is dead code, however well tested it is on its own.
pub fn analyze(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = analyze_tier1(tree, src);
    out.extend(crate::tier2a::analyze_tier2a(tree, src));
    out.extend(crate::tier2b::analyze_tier2b(tree, src));
    out.extend(crate::tier3::analyze_tier3(tree, src));
    // MOD003/MOD004/MOD005.
    out.extend(group_by_all(tree, src));
    out.extend(subquery_order_by(tree, src));
    out.extend(distinct_group_by(tree, src));
    out.sort_by_key(|f| (f.span.start, f.code));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use grebe_syntax::matcher::parse;

    /// Codes fired for `sql`, in source order.
    fn codes(sql: &str) -> Vec<&'static str> {
        let tree = parse(sql).unwrap_or_else(|| panic!("did not parse: {sql}"));
        analyze(&tree, sql).into_iter().map(|f| f.code).collect()
    }

    fn fires(code: &str, sql: &str) -> bool {
        codes(sql).contains(&code)
    }

    #[test]
    fn mod001_count_star() {
        for sql in [
            "SELECT count(*) FROM t",
            "SELECT count( * ) FROM t",
            "SELECT a, count(*) OVER (PARTITION BY b) FROM t",
            "SELECT COUNT(*) FROM t",
        ] {
            assert!(fires("MOD001", sql), "should fire: {sql}");
        }
        for sql in [
            "SELECT count(x) FROM t",
            "SELECT count(DISTINCT x) FROM t",
            "SELECT count(t.*) FROM t",
            "SELECT count() FROM t",
            // `DISTINCT`/`ALL` sits beside the argument list, not inside
            // the argument count, so it doesn't change the CST shape the
            // way `t.*`'s `StarQualifierList` does -- it needs its own
            // guard. `count(DISTINCT *)` counts distinct rows, a different
            // value than `count(*)`, so it must not fire.
            "SELECT count(DISTINCT *) FROM t",
            // The `*` here belongs to DuckDB's `COLUMNS(*)` star-expression,
            // not to `count`. Deleting it would produce `count(COLUMNS())`,
            // which DuckDB rejects; the argument/star span check prevents it.
            "SELECT count(COLUMNS(*)) FROM t",
            "SELECT min(COLUMNS(*)), count(COLUMNS(*)) FROM numbers",
            "SELECT count(ALL *) FROM t",
        ] {
            assert!(!fires("MOD001", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod006_ifnull_coalesce() {
        for sql in ["SELECT ifnull(a, b) FROM t", "SELECT NVL(x, 0) FROM t"] {
            assert!(fires("MOD006", sql), "should fire: {sql}");
        }
        for sql in ["SELECT coalesce(a, b) FROM t", "SELECT ifnull FROM t"] {
            assert!(!fires("MOD006", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod007_natural_join() {
        for sql in [
            "SELECT * FROM a NATURAL JOIN b",
            "SELECT * FROM a NATURAL LEFT JOIN b",
            "SELECT * FROM a NATURAL INNER JOIN b",
        ] {
            assert!(fires("MOD007", sql), "should fire: {sql}");
        }
        // `natural` cannot appear as a bare identifier -- it is a reserved
        // keyword -- so the quoted form is the real guard case.
        for sql in [
            "SELECT * FROM a JOIN b ON a.i = b.i",
            "SELECT \"natural\" FROM t",
            "SELECT * FROM a CROSS JOIN b",
        ] {
            assert!(!fires("MOD007", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod009_self_alias() {
        for sql in [
            "SELECT foo AS foo FROM t",
            "SELECT t.foo AS foo FROM t",
            "SELECT \"Foo\" AS foo FROM t",
            // Case-insensitive on both sides.
            "SELECT FOO AS foo FROM t",
            // Quote-insensitive on both sides -- the alias, not just the
            // expression, may be quoted.
            "SELECT foo AS \"Foo\" FROM t",
            // A comment between the expression and `AS`, or between `AS`
            // and the alias, must not throw off the comparison -- the fix
            // must read the CST's own child spans, not slice source text.
            "SELECT foo /* c */ AS foo FROM t",
            "SELECT foo AS /* c */ foo FROM t",
        ] {
            assert!(fires("MOD009", sql), "should fire: {sql}");
        }
        for sql in [
            "SELECT foo AS bar FROM t",
            "SELECT 1 AS one FROM t",
            // A computation that merely ENDS in the alias name is not a
            // redundant alias -- the case the bare-reference guard is for.
            "SELECT fib.a + fib.next AS next FROM fib",
            "SELECT a + b AS b FROM t",
        ] {
            assert!(!fires("MOD009", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod009_fix_deletes_alias_clause() {
        let sql = "SELECT foo AS foo FROM t";
        let tree = parse(sql).unwrap();
        let findings = self_alias(&tree, sql);
        assert_eq!(findings.len(), 1);
        let fix = findings[0].fix.clone().expect("MOD009 has a safe fix");
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, "SELECT foo FROM t");
        let t2 = parse(&fixed).unwrap();
        assert!(self_alias(&t2, &fixed).is_empty(), "fix is not idempotent");
    }

    #[test]
    fn mod009_fix_deletes_alias_clause_with_comments() {
        // The fix must remove the whole clause -- comments included -- from
        // the end of the expression through the end of the alias token.
        let sql = "SELECT foo /* c */ AS foo FROM t";
        let tree = parse(sql).unwrap();
        let findings = self_alias(&tree, sql);
        assert_eq!(findings.len(), 1);
        let fix = findings[0].fix.clone().expect("MOD009 has a safe fix");
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, "SELECT foo FROM t");
        let t2 = parse(&fixed).unwrap();
        assert!(self_alias(&t2, &fixed).is_empty(), "fix is not idempotent");
    }

    #[test]
    fn mod020_plain_big_number() {
        for sql in ["SELECT 1000000", "SELECT 123456789"] {
            assert!(fires("MOD020", sql), "should fire: {sql}");
        }
        for sql in [
            "SELECT 1_000_000",   // already grouped -- makes the rule idempotent
            "SELECT 100000",      // under the 7-digit threshold
            "SELECT 1000000.0",   // float
            "SELECT 0x1000000",   // hex
            "SELECT '123456789'", // string literal, not a number
        ] {
            assert!(!fires("MOD020", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod003_group_by_all() {
        for sql in [
            "SELECT a, b, count(*) FROM t GROUP BY a, b",
            // Different order -- unlike MOD013's ORDER BY version, this
            // comparison is sorted.
            "SELECT a, b, sum(x) FROM t GROUP BY b, a",
            // Qualified/dotted references, case-insensitively.
            "SELECT t.a, t.b, count(*) FROM t GROUP BY t.a, t.b",
            "SELECT A, count(*) FROM t GROUP BY a",
        ] {
            assert!(fires("MOD003", sql), "should fire: {sql}");
        }
        for sql in [
            // Guard (1): already ALL.
            "SELECT a, b, count(*) FROM t GROUP BY ALL",
            // Guard (4), by way of a non-GroupByBaseExpression alternative:
            // GROUPING SETS / CUBE / ROLLUP are non-list forms.
            "SELECT a, b FROM t GROUP BY GROUPING SETS ((a), (b))",
            "SELECT a, b FROM t GROUP BY CUBE(a, b)",
            "SELECT a, b FROM t GROUP BY ROLLUP(a, b)",
            // Guard (2): a window expression anywhere in the select list
            // aborts the whole rule, even though `a` alone would match
            // `GROUP BY a`.
            "SELECT a, count(*) OVER (PARTITION BY b) FROM t GROUP BY a",
            // Guard (3): a computed, non-aggregate select item aborts the
            // whole rule -- `a + 1` isn't captured by comparing to the
            // plain-column GROUP BY list, even though ALL would also group
            // by it.
            "SELECT a, a + 1 FROM t GROUP BY a",
            // The GROUP BY list names a column, `x`, that isn't a plain
            // select-list item -- the sets don't match.
            "SELECT a, count(x) FROM t GROUP BY a, x",
            // Guard (4): a computed GROUP BY expression never matches a
            // plain column set, but is checked explicitly rather than
            // incidentally.
            "SELECT a, x FROM t GROUP BY upper(a), x",
        ] {
            assert!(!fires("MOD003", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod003_guards_abort_the_whole_rule_not_just_the_item() {
        // Both statements contain a select list / GROUP BY pair that would
        // otherwise match (`a, b` vs `GROUP BY a, b`) alongside a construct
        // that must abort the entire rule for the statement -- not merely
        // exclude the offending item. Neither may produce a finding.
        for sql in [
            "SELECT a, b, count(*) OVER (PARTITION BY a) AS w, count(*) FROM t GROUP BY a, b",
            "SELECT a, b, a + 1, count(*) FROM t GROUP BY a, b",
        ] {
            assert!(
                !fires("MOD003", sql),
                "guard should abort the whole rule: {sql}"
            );
        }
    }

    #[test]
    fn mod003_fix_rewrites_to_group_by_all() {
        let sql = "SELECT a, b, count(*) FROM t GROUP BY a, b";
        let tree = parse(sql).unwrap();
        let findings = group_by_all(&tree, sql);
        assert_eq!(findings.len(), 1);
        let fix = findings[0].fix.clone().expect("MOD003 has a safe fix");
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, "SELECT a, b, count(*) FROM t GROUP BY ALL");
        let t2 = parse(&fixed).unwrap();
        assert!(
            group_by_all(&t2, &fixed).is_empty(),
            "fix is not idempotent"
        );
    }

    #[test]
    fn mod004_subquery_order_by() {
        for sql in [
            "SELECT * FROM (SELECT a FROM t ORDER BY a) sub",
            "WITH c AS (SELECT a FROM t ORDER BY a) SELECT * FROM c",
            "SELECT * FROM t WHERE x IN (SELECT a FROM u ORDER BY a)",
            "SELECT (SELECT a FROM u ORDER BY a) FROM t",
        ] {
            assert!(fires("MOD004", sql), "should fire: {sql}");
        }
        for sql in [
            // LIMIT present: order is meaningful, it defines which rows.
            "SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 10) sub",
            // The outermost statement's own ORDER BY legitimately controls
            // final output order -- this rule never looks at it.
            "SELECT a FROM t ORDER BY a",
            // A set-operation arm's ORDER BY is part of ResultModifiers
            // shared across the whole chain (the trailing LIMIT belongs to
            // the chain, not this arm) -- SelectParens is not a scope.
            "(SELECT a FROM t ORDER BY a) UNION (SELECT b FROM u) LIMIT 5",
        ] {
            assert!(!fires("MOD004", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod004_scope_exclusion_does_not_suppress_a_real_nested_hit() {
        // The top-level ORDER BY here must not fire on its own, but must
        // also not swallow the genuine subquery-interior hit alongside it.
        // Checked against `subquery_order_by` directly, not `codes`/
        // `analyze` -- other rules (e.g. MOD013, MOD022) also legitimately
        // fire on this SQL and are beside the point of this test.
        let sql = "SELECT * FROM (SELECT a FROM t ORDER BY a) sub ORDER BY sub.a";
        let tree = parse(sql).unwrap();
        assert_eq!(subquery_order_by(&tree, sql).len(), 1);

        // Same for a set-operation arm's ORDER BY sitting next to a real
        // subquery hit nested inside a different arm.
        let sql2 = "(SELECT * FROM (SELECT x FROM t ORDER BY x) sub) \
                     UNION (SELECT y FROM u ORDER BY y) LIMIT 3";
        let tree2 = parse(sql2).unwrap();
        assert_eq!(subquery_order_by(&tree2, sql2).len(), 1);
    }

    #[test]
    fn mod004_has_no_fix() {
        let sql = "SELECT * FROM (SELECT a FROM t ORDER BY a) sub";
        let tree = parse(sql).unwrap();
        let findings = subquery_order_by(&tree, sql);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].fix.is_none());
    }

    #[test]
    fn mod005_distinct_group_by() {
        assert!(fires(
            "MOD005",
            "SELECT DISTINCT a, count(*) FROM t GROUP BY a"
        ));
        for sql in [
            // DISTINCT ON has order-dependent semantics -- a deliberate
            // guard, not redundant with GROUP BY the way plain DISTINCT is.
            "SELECT DISTINCT ON (a) b, c FROM t GROUP BY a",
            "SELECT a FROM t GROUP BY a",
            "SELECT DISTINCT a FROM t",
            // SELECT ALL is DISTINCT's opposite (`DistinctAll <- 'ALL'`),
            // not a spelling of plain DISTINCT.
            "SELECT ALL a FROM t GROUP BY a",
            // An aggregate's own DISTINCT modifier is not a SELECT-level
            // DISTINCT clause.
            "SELECT count(DISTINCT a) FROM t GROUP BY b",
        ] {
            assert!(!fires("MOD005", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod005_has_no_fix() {
        let sql = "SELECT DISTINCT a, count(*) FROM t GROUP BY a";
        let tree = parse(sql).unwrap();
        let findings = distinct_group_by(&tree, sql);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].fix.is_none());
    }

    #[test]
    fn fixes_are_minimal_and_correct() {
        let sql = "SELECT count(*) FROM t";
        let tree = parse(sql).unwrap();
        let f = &analyze(&tree, sql)[0];
        let fix = f.fix.clone().expect("MOD001 has a safe fix");
        // Deleting just the star yields valid, equivalent SQL.
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, "SELECT count() FROM t");
        assert!(parse(&fixed).is_some(), "fixed SQL must still parse");
        // And the rule must not fire on its own output.
        let t2 = parse(&fixed).unwrap();
        assert!(analyze(&t2, &fixed).is_empty(), "fix is not idempotent");
    }
}
