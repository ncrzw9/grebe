//! Tier 4 CST detectors: MOD026 (limit-no-orderby), MOD027
//! (window-no-orderby), MOD028 (not-in-subquery), MOD029
//! (filter-defeats-outer-join).
//!
//! Where tiers 1-3 are mostly about idiom and readability, this tier is
//! about rows coming back wrong or in whatever order the engine felt like
//! -- plan changes, parallelism, a new DuckDB version -- with nothing in
//! the query's own text saying that was expected. Every rule here is
//! detect-only: the fix is a design choice (which order? which rows?),
//! never a mechanical rewrite.
//!
//! Production names are those of the vendored DuckDB grammar; `grebe tree`
//! prints them for any statement.

use grebe_syntax::cst::{NodeId, Tree};

use crate::detect::Finding;

/// The single child of `id` produced by `rule`, if there is exactly one.
///
/// A private copy of the identical helper in [`crate::tier3`] and the other
/// tier modules -- see there for why it stays duplicated rather than shared.
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

/// `SCOPE_BOUNDARIES` / `is_top_level` / `top_level_find`: a private copy of
/// the identical trio in [`crate::tier3`] (itself a copy of
/// [`crate::tier2b`]'s) -- the walk that stays inside one statement's own
/// clauses, never crossing into a nested subquery/CTE/set-op arm.
const SCOPE_BOUNDARIES: &[&str] = &[
    "SubqueryReference",
    "InSelectStatement",
    "CTESelectBody",
    "SelectParens",
];

fn top_level_find(tree: &Tree, root: NodeId, rule: &str) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if tree.rule_name(n) == rule {
            out.push(n);
            continue;
        }
        if SCOPE_BOUNDARIES.contains(&tree.rule_name(n)) {
            continue;
        }
        for &c in tree.children(n).iter().rev() {
            stack.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------
// MOD026 -- limit-no-orderby
// ---------------------------------------------------------------------

/// MOD026 limit-no-orderby: a `LIMIT`/`OFFSET`/`FETCH` with no `ORDER BY`
/// at the same result-modifier scope.
///
/// Without an `ORDER BY`, SQL makes no promise about which rows a query
/// returns in which order -- a plan change, parallelism, or a point
/// release can silently change which rows a `LIMIT` keeps, or which a
/// correlated `LIMIT 1` picks as "the" row. `ResultModifiers <-
/// OrderByClause? LimitOffset?` (`statements/select.gram`) puts both
/// directly under the same node, one check per statement's own result
/// modifiers -- a CTE's or a set operand's own `ResultModifiers` is a
/// separate node, checked on its own.
///
/// No fix: whether the missing order is a bug or a deliberate "any 5
/// rows will do" sample is a judgement call the query's own comments
/// would have to settle, not something a byte-span edit can.
pub fn limit_without_order_by(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for rm in tree.find("ResultModifiers") {
        let Some(limit) = child(tree, rm, "LimitOffset") else {
            continue;
        };
        if child(tree, rm, "OrderByClause").is_some() {
            continue;
        }
        out.push(Finding {
            code: "MOD026",
            span: tree.node(limit).span,
            fix: None,
        });
    }
    out
}

// ---------------------------------------------------------------------
// MOD027 -- window-no-orderby
// ---------------------------------------------------------------------

/// Functions whose result is position-within-partition, not a value
/// computed over rows -- an `OVER` clause with no `ORDER BY` leaves the
/// partition in whatever order the scan happened to produce it in, so
/// "row 1" (or "the previous row", for `lag`) is as arbitrary as an
/// unordered `LIMIT`. Matched case-insensitively against the function's
/// own name, never the vendored keyword lists -- these are plain
/// functions, not grammar keywords.
const ORDER_SENSITIVE_WINDOW_FNS: &[&str] = &[
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
];

/// The bare, lowercased name of the function `func_expr` (a
/// `FunctionExpression` node) calls -- the trailing component, so a
/// schema-qualified call (`main.row_number()`) still matches on
/// `row_number`.
fn function_name(tree: &Tree, func_expr: NodeId, src: &str) -> Option<String> {
    let &identifier = tree.children(func_expr).first()?;
    tree.descendants(identifier)
        .into_iter()
        .find(|&d| tree.rule_name(d) == "FunctionName")
        .map(|n| tree.text(n, src).trim().to_ascii_lowercase())
}

/// MOD027 window-no-orderby: `row_number()`, `rank()`, `lag()` and the
/// other functions in `ORDER_SENSITIVE_WINDOW_FNS`, called with an
/// `OVER` clause that has no `ORDER BY`.
///
/// `WindowFrameContents <- WindowPartition? OrderByClause? FrameClause?`
/// (`statements/expression.gram`) -- a plain inline window definition
/// with no `OrderByClause` child. `OVER window_name` and `OVER (window_name
/// ...)`, which reference a named `WINDOW` clause definition this detector
/// does not resolve, are left alone rather than guessed at -- a real miss
/// is better than a wrong one.
///
/// No fix: the right order is the query's own business logic, not
/// something inferable from the columns already in scope.
pub fn window_without_order_by(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for func_expr in tree.find("FunctionExpression") {
        let Some(name) = function_name(tree, func_expr, src) else {
            continue;
        };
        if !ORDER_SENSITIVE_WINDOW_FNS.contains(&name.as_str()) {
            continue;
        }
        let Some(over) = child(tree, func_expr, "OverClause") else {
            continue;
        };
        let Some(contents) = window_frame_contents(tree, over) else {
            continue;
        };
        if child(tree, contents, "OrderByClause").is_some() {
            continue;
        }
        out.push(Finding {
            code: "MOD027",
            span: tree.node(func_expr).span,
            fix: None,
        });
    }
    out
}

/// The `WindowFrameContents` an `OverClause` defines inline, if it defines
/// one at all: `OverClause <- 'OVER' WindowFrame`, `WindowFrame <-
/// ParensIdentifier / WindowFrameDefinition / IdentifierWindowFrame`. The
/// two `Identifier` variants are bare references to a named `WINDOW`
/// clause (`OVER w`, `OVER (w)`) -- out of scope, that window's own
/// definition would need resolving first.
///
/// `WindowFrameDefinition <- WindowFrameNameContentsParens /
/// WindowFrameContentsParens`, tried in that order: the first
/// alternative's `BaseWindowName?` is optional, so it also matches
/// `OVER ()` (nothing to name) and, once `PARTITION`/`ORDER`/the frame
/// keywords turn out not to parse as a bare identifier, backtracks into
/// matching them as the *contents* instead -- `OVER (PARTITION BY a)`
/// ends up on the second, plain alternative, while `OVER ()` and `OVER (w
/// ORDER BY ...)` (extending a named window: also out of scope, for the
/// same reason as the two bare forms) stay on the first. Both are read
/// here, since which one a given `OVER (...)` lands on is this
/// backtracking, not anything the query's author chose.
fn window_frame_contents(tree: &Tree, over: NodeId) -> Option<NodeId> {
    let frame = child(tree, over, "WindowFrame")?;
    let definition = child(tree, frame, "WindowFrameDefinition")?;
    if let Some(parens) = child(tree, definition, "WindowFrameContentsParens") {
        return child(tree, parens, "WindowFrameContents");
    }
    let name_parens = child(tree, definition, "WindowFrameNameContentsParens")?;
    let name_contents = child(tree, name_parens, "WindowFrameNameContents")?;
    if child(tree, name_contents, "BaseWindowName").is_some() {
        return None;
    }
    child(tree, name_contents, "WindowFrameContents")
}

// ---------------------------------------------------------------------
// MOD028 -- not-in-subquery
// ---------------------------------------------------------------------

/// MOD028 not-in-subquery: `x NOT IN (SELECT ...)`.
///
/// If the subquery's result ever contains a `NULL`, `x NOT IN (that set)`
/// is `UNKNOWN` for *every* `x` -- not an error, not an empty intent, just
/// silently zero rows, for every row, including the ones that plainly
/// should have matched. `NOT EXISTS` (or filtering the subquery's own
/// column for `IS NOT NULL`) does not have this trap. Plain `IN (SELECT
/// ...)` is fine either way -- a `NULL` in the set makes unmatched rows
/// `UNKNOWN` rather than `FALSE`, which `WHERE` treats the same as `FALSE`;
/// it is specifically the negation that turns a `NULL` into "reject
/// everything".
///
/// `BetweenInLikeOp <- 'NOT'? BetweenInLikeOpExpression` leaves `'NOT'` as
/// a bare token, not a node of its own, so its presence is read off the
/// node's own source text rather than a child.
///
/// No fix: grebe has no catalog, so it cannot know whether the subquery's
/// column is actually nullable -- only that the shape is one `NULL` away
/// from silently wrong.
pub fn not_in_subquery(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for op in tree.find("BetweenInLikeOp") {
        if !tree
            .text(op, src)
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("NOT")
        {
            continue;
        }
        let Some(wrapper) = child(tree, op, "BetweenInLikeOpExpression") else {
            continue;
        };
        let Some(in_clause) = child(tree, wrapper, "InClause") else {
            continue;
        };
        let has_subquery = tree
            .descendants(in_clause)
            .into_iter()
            .any(|n| tree.rule_name(n) == "InSelectStatement");
        if has_subquery {
            out.push(Finding {
                code: "MOD028",
                span: tree.node(op).span,
                fix: None,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------
// MOD029 -- filter-defeats-outer-join
// ---------------------------------------------------------------------

/// MOD029 filter-defeats-outer-join: a top-level `WHERE` predicate that
/// rejects a `NULL` on a `LEFT JOIN`'s right-hand side, which turns the
/// join into an `INNER JOIN` in every row that actually needed the
/// "left" in `LEFT JOIN` -- the unmatched rows the join was written to
/// keep are exactly the rows this `WHERE` throws away. The fix is usually
/// to move the predicate into the join's own `ON`, not to touch `WHERE`.
///
/// Deliberately narrow, to keep this at zero known false positives
/// rather than wide and occasionally wrong:
///
/// - only `RegularJoinClause` (`LEFT [OUTER] JOIN t ON/USING ...`) is
///   read for its `JoinType`; `NATURAL`/`ASOF`/`POSITIONAL` joins and
///   `RIGHT`/`FULL` joins are out of scope (a `RIGHT JOIN`'s nullable
///   side is its *left* input, which is harder to name from the join
///   node alone, and worth its own rule rather than a guess here).
/// - the whole top-level `WHERE` is skipped the moment it has a
///   top-level `OR` (`LogicalOrExpression` with any
///   `LogicalOrExpressionTail`) -- `WHERE b.x = 1 OR b.x IS NULL` is the
///   standard way to keep an outer join's unmatched rows, and `OR` makes
///   which operand decides a row's fate too hard to read off the tree
///   alone.
/// - a top-level `AND` operand with an explicit `NOT`, or that is itself
///   an `IS [NOT] NULL` / `IS [NOT] DISTINCT FROM` test
///   (`LogicalNotExpression`/`IsExpression` with any `IsTest`), is
///   skipped outright: `b.id IS NULL` is the idiomatic way to find
///   *unmatched* rows, and getting the negated forms wrong (`IS NOT
///   NULL`, `NOTNULL`) right would need the same careful reading `OR`
///   already doesn't get. Real, but rarer, misses.
/// - an operand containing a `ParensExpression` anywhere is skipped --
///   parenthesised sub-conditions can carry their own `OR` or their own
///   `IS NULL` guard, and this does not parse back into them.
/// - a reference wrapped in `coalesce(...)` is not counted: defaulting a
///   `NULL` before comparing it is exactly how someone keeps an outer
///   join's unmatched rows on purpose.
///
/// One finding per offending `AND` operand, named after the first
/// defeated join found in it. No fix: whether the predicate belongs in
/// `ON` or the join should simply be an `INNER JOIN` is a question about
/// what the query is for.
pub fn filter_defeats_outer_join(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for simple_select in tree.find("SimpleSelect") {
        let Some(where_clause) = top_level_find(tree, simple_select, "WhereClause")
            .into_iter()
            .next()
        else {
            continue;
        };
        let targets = left_joined_targets(tree, simple_select, src);
        if targets.is_empty() {
            continue;
        }
        let Some(expr) = child(tree, where_clause, "Expression") else {
            continue;
        };
        let Some(or_expr) = descend_single(tree, expr, "LogicalOrExpression") else {
            continue;
        };
        if tree.children(or_expr).len() > 1 {
            continue; // a top-level OR: out of scope, see the doc comment.
        }
        for operand in and_chain_operands(tree, or_expr) {
            if let Some(target) = defeated_join(tree, operand, &targets, src) {
                let _ = target; // named in the message only, not matched on.
                out.push(Finding {
                    code: "MOD029",
                    span: tree.node(operand).span,
                    fix: None,
                });
            }
        }
    }
    out
}

/// The reference name (alias if aliased, else the bare table name) of
/// every table on the right-hand side of a top-level, plain `LEFT [OUTER]
/// JOIN` in `scope`'s own `FROM` clause.
fn left_joined_targets(tree: &Tree, scope: NodeId, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for join in top_level_find(tree, scope, "JoinClause") {
        let Some(regular) = child(tree, join, "RegularJoinClause") else {
            continue;
        };
        let Some(join_type) = child(tree, regular, "JoinType") else {
            continue;
        };
        if child(tree, join_type, "LeftJoin").is_none() {
            continue;
        }
        let Some(table_ref) = child(tree, regular, "TableRef") else {
            continue;
        };
        if let Some(name) = join_target_name(tree, table_ref, src) {
            out.push(name);
        }
    }
    out
}

/// `TableRef <- InnerTableRef JoinOrPivot*`, `InnerTableRef <- ValuesRef /
/// TableFunction / TableSubquery / BaseTableRef / ParensTableRef`. An
/// aliased source is named by its alias; an unaliased `BaseTableRef` is
/// named by its own bare table name (the only qualifier a reference to it
/// could use); anything else unaliased (a derived table, a table
/// function, a `VALUES` clause) has no name this can read off the tree,
/// so it is left out rather than guessed at.
fn join_target_name(tree: &Tree, table_ref: NodeId, src: &str) -> Option<String> {
    let inner = child(tree, table_ref, "InnerTableRef")?;
    let &kind = tree.children(inner).first()?;
    if let Some(alias) = table_alias_name(tree, kind, src) {
        return Some(alias);
    }
    if tree.rule_name(kind) == "BaseTableRef" {
        let name_node = child(tree, kind, "BaseTableName")?;
        return Some(last_dotted_lower(tree, name_node, src));
    }
    None
}

/// The identifier (or string literal) `node`'s own `TableAlias` child
/// spells, if it has one -- `TableAlias <- TableAliasAs /
/// TableAliasWithoutAs`, `TableAliasAs <- 'AS' IdentifierOrStringLiteral
/// ColumnAliases?`, `TableAliasWithoutAs <- Identifier ColumnAliases?`:
/// read generically, by descendant search for the identifier or string
/// leaf, rather than by picking apart which of the two variants matched.
fn table_alias_name(tree: &Tree, node: NodeId, src: &str) -> Option<String> {
    let alias = child(tree, node, "TableAlias")?;
    tree.descendants(alias)
        .into_iter()
        .find(|&d| matches!(tree.rule_name(d), "Identifier" | "StringLiteral"))
        .map(|n| last_dotted_lower(tree, n, src))
}

/// The last `.`-separated component of `id`'s own text, lowercased -- a
/// CST-shape-agnostic way to reduce a qualified name (or a bare one) to
/// its final identifier. A private copy of the identical helper in
/// [`crate::tier3`].
fn last_dotted_lower(tree: &Tree, id: NodeId, src: &str) -> String {
    tree.text(id, src)
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// Walks a strictly single-child chain from `n` down to the first node
/// named `target`, or `None` the moment a node along the way has more
/// than one child (a fork this helper does not choose a branch at) or the
/// chain runs out before reaching it.
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

/// `LogicalOrExpression <- LogicalAndExpression LogicalOrExpressionTail*`:
/// `or_expr` has already been checked to have no `LogicalOrExpressionTail`
/// siblings, so its one child is a `LogicalAndExpression`. `LogicalAndExpression
/// <- LogicalNotExpression LogicalAndExpressionTail*`, `LogicalAndExpressionTail
/// <- 'AND' LogicalNotExpression` -- the first `LogicalNotExpression` plus
/// each tail's own, in source order: the operands of the top-level `AND`
/// chain (a chain of one, with no explicit `AND` at all, is one operand).
fn and_chain_operands(tree: &Tree, or_expr: NodeId) -> Vec<NodeId> {
    let Some(&and_expr) = tree.children(or_expr).first() else {
        return Vec::new();
    };
    tree.children(and_expr)
        .iter()
        .filter_map(|&c| match tree.rule_name(c) {
            "LogicalNotExpression" => Some(c),
            "LogicalAndExpressionTail" => tree.children(c).first().copied(),
            _ => None,
        })
        .collect()
}

/// If `operand` (a top-level `AND` operand, a `LogicalNotExpression`)
/// references one of `targets` in a way [`filter_defeats_outer_join`]'s
/// doc comment calls in scope, the first such target's name; `None` if it
/// doesn't, or if its shape is one of the guards documented there.
fn defeated_join(tree: &Tree, operand: NodeId, targets: &[String], src: &str) -> Option<String> {
    // An explicit `NOT`, or any `IS`/`IS NOT`/`NOTNULL`/`ISNULL`/`IS
    // DISTINCT FROM` test, means this operand is not a plain comparison --
    // out of scope, per the doc comment.
    if tree.children(operand).len() > 1 {
        return None;
    }
    let &is_expr = tree.children(operand).first()?;
    if tree.children(is_expr).len() > 1 {
        return None;
    }
    let &is_distinct = tree.children(is_expr).first()?;
    if tree.children(is_distinct).len() > 1 {
        return None;
    }
    let &comparison = tree.children(is_distinct).first()?;
    // `ComparisonExpression <- BetweenInLikeExpression ComparisonExpressionTail*`
    // and `BetweenInLikeExpression <- OtherOperatorExpression
    // BetweenInLikeOp?` are themselves pass-through for an ordinary `=` /
    // `<` / `<>` comparison: every operator `AnyOp` lists (which is all of
    // them) is matched one level further down, inside
    // `OtherOperatorExpression <- BitwiseExpression OtherOperatorTail*`,
    // before either of these two gets a chance to apply their own,
    // narrower operator rule. A real predicate is a tail at any of these
    // three levels (plain operator, or `IN`/`LIKE`/`BETWEEN`); a bare
    // value with none of them (just `WHERE b.flag`) is left alone.
    let has_predicate = tree.children(comparison).len() > 1
        || tree.children(comparison).first().is_some_and(|&between| {
            tree.children(between).len() > 1
                || tree
                    .children(between)
                    .first()
                    .is_some_and(|&other_op| tree.children(other_op).len() > 1)
        });
    if !has_predicate {
        return None;
    }
    if tree
        .descendants(comparison)
        .iter()
        .any(|&d| tree.rule_name(d) == "ParensExpression")
    {
        return None;
    }
    for qualifier in tree.descendants(comparison) {
        if tree.rule_name(qualifier) != "TableQualification" {
            continue;
        }
        let name = tree
            .text(qualifier, src)
            .trim_end_matches('.')
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let Some(target) = targets.iter().find(|t| **t == name) else {
            continue;
        };
        if !coalesce_guards(tree, qualifier, comparison, src) {
            return Some(target.clone());
        }
    }
    None
}

/// True if a `coalesce(...)` call sits between `node` and `boundary`
/// (exclusive) -- i.e. `node` is being defaulted before anything compares
/// it, which is exactly how an outer join's unmatched rows are kept on
/// purpose.
///
/// `COALESCE` has its own dedicated grammar production, `CoalesceExpression
/// <- 'COALESCE' Parens(List(Expression))` under `SpecialFunctionExpression`
/// -- it is not a plain `FunctionExpression` call the way `coalesce` reads,
/// and `'COALESCE'` is a bare keyword, not a `FunctionName` node, so this
/// checks the rule name directly rather than going through
/// [`function_name`].
fn coalesce_guards(tree: &Tree, node: NodeId, boundary: NodeId, _src: &str) -> bool {
    let mut n = node;
    while let Some(p) = tree.parent(n) {
        if p == boundary {
            return false;
        }
        if tree.rule_name(p) == "CoalesceExpression" {
            return true;
        }
        n = p;
    }
    false
}

/// Every detector in this module.
pub fn analyze_tier4(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(limit_without_order_by(tree, src));
    out.extend(window_without_order_by(tree, src));
    out.extend(not_in_subquery(tree, src));
    out.extend(filter_defeats_outer_join(tree, src));
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
        analyze_tier4(&tree, sql)
            .into_iter()
            .map(|f| f.code)
            .collect()
    }

    fn fires(code: &str, sql: &str) -> bool {
        codes(sql).contains(&code)
    }

    // -- MOD026 limit-no-orderby -----------------------------------------

    #[test]
    fn mod026_fires_on_limit_or_offset_without_order_by() {
        for sql in [
            "SELECT a FROM t LIMIT 10",
            "SELECT a FROM t OFFSET 5",
            "SELECT a FROM t LIMIT 10 OFFSET 5",
            "SELECT a FROM t WHERE a = 1 LIMIT 1",
        ] {
            assert!(fires("MOD026", sql), "should fire: {sql}");
        }
    }

    #[test]
    fn mod026_does_not_fire_with_order_by_present() {
        for sql in [
            "SELECT a FROM t ORDER BY a LIMIT 10",
            "SELECT a FROM t ORDER BY a OFFSET 5",
            "SELECT a FROM t",
            "WITH x AS (SELECT a FROM t) SELECT a FROM x ORDER BY a LIMIT 5",
        ] {
            assert!(!fires("MOD026", sql), "should not fire: {sql}");
        }
    }

    #[test]
    fn mod026_checks_each_scope_on_its_own() {
        // The CTE's own LIMIT has no ORDER BY; the outer query's does.
        let sql = "WITH x AS (SELECT a FROM t LIMIT 5) SELECT a FROM x ORDER BY a LIMIT 10";
        assert_eq!(codes(sql), ["MOD026"]);
    }

    // -- MOD027 window-no-orderby -----------------------------------------

    #[test]
    fn mod027_fires_on_ranking_and_offset_functions_without_order_by() {
        for sql in [
            "SELECT row_number() OVER (PARTITION BY a) FROM t",
            "SELECT rank() OVER () FROM t",
            "SELECT lag(a) OVER (PARTITION BY b) FROM t",
            "SELECT ntile(4) OVER () FROM t",
        ] {
            assert!(fires("MOD027", sql), "should fire: {sql}");
        }
    }

    #[test]
    fn mod027_does_not_fire_with_order_by_or_on_aggregates() {
        for sql in [
            "SELECT row_number() OVER (ORDER BY a) FROM t",
            "SELECT rank() OVER (PARTITION BY a ORDER BY b) FROM t",
            // sum() is not in the order-sensitive list: MOD030-territory
            // (ordered-aggregate-default-frame), not this rule's job.
            "SELECT sum(a) OVER (PARTITION BY b) FROM t",
            "SELECT a FROM t",
        ] {
            assert!(!fires("MOD027", sql), "should not fire: {sql}");
        }
    }

    #[test]
    fn mod027_leaves_named_windows_alone() {
        for sql in [
            "SELECT row_number() OVER w FROM t WINDOW w AS (ORDER BY a)",
            "SELECT row_number() OVER (w) FROM t WINDOW w AS (ORDER BY a)",
        ] {
            assert!(
                !fires("MOD027", sql),
                "named window, should not fire: {sql}"
            );
        }
    }

    // -- MOD028 not-in-subquery --------------------------------------------

    #[test]
    fn mod028_fires_on_not_in_subquery_only() {
        assert!(fires(
            "MOD028",
            "SELECT * FROM a WHERE a.id NOT IN (SELECT id FROM b)"
        ));
        assert!(!fires(
            "MOD028",
            "SELECT * FROM a WHERE a.id IN (SELECT id FROM b)"
        ));
        assert!(!fires(
            "MOD028",
            "SELECT * FROM a WHERE a.id NOT IN (1, 2, 3)"
        ));
        assert!(!fires("MOD028", "SELECT * FROM a WHERE a.id IN (1, 2, 3)"));
    }

    // -- MOD029 filter-defeats-outer-join -----------------------------------

    #[test]
    fn mod029_fires_on_a_plain_predicate_against_the_joined_side() {
        for sql in [
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE b.x = 1",
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE 1 = b.x",
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE b.x IN (1, 2)",
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE b.x BETWEEN 1 AND 2",
            "SELECT * FROM a LEFT JOIN b AS bb ON a.id = bb.id WHERE bb.x = 1",
            "SELECT * FROM a LEFT OUTER JOIN b ON a.id = b.id WHERE b.x = 1",
        ] {
            assert!(fires("MOD029", sql), "should fire: {sql}");
        }
    }

    #[test]
    fn mod029_leaves_the_idiomatic_is_null_guard_alone() {
        for sql in [
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE b.id IS NULL",
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE b.x = 1 OR b.x IS NULL",
        ] {
            assert!(!fires("MOD029", sql), "should not fire: {sql}");
        }
    }

    #[test]
    fn mod029_leaves_a_coalesce_guarded_predicate_alone() {
        assert!(!fires(
            "MOD029",
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE coalesce(b.x, 1) = 1"
        ));
    }

    #[test]
    fn mod029_does_not_fire_on_inner_joins_or_the_preserved_side() {
        for sql in [
            "SELECT * FROM a JOIN b ON a.id = b.id WHERE b.x = 1",
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE a.x = 1",
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id",
        ] {
            assert!(!fires("MOD029", sql), "should not fire: {sql}");
        }
    }

    #[test]
    fn mod029_skips_an_operand_wrapped_in_extra_parens_or_not() {
        for sql in [
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE (b.x = 1)",
            "SELECT * FROM a LEFT JOIN b ON a.id = b.id WHERE NOT b.x = 1",
        ] {
            assert!(!fires("MOD029", sql), "should not fire: {sql}");
        }
    }

    #[test]
    fn mod029_names_only_the_defeated_join_among_several() {
        // Two LEFT JOINs: only the predicate on `c` should fire.
        let sql = "SELECT * FROM a LEFT JOIN b ON a.id = b.id LEFT JOIN c ON a.id = c.id \
                   WHERE c.x = 1";
        assert_eq!(codes(sql), ["MOD029"]);
    }
}
