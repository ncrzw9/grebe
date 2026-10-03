//! Tier 3 CST detectors: MOD021 (qualify-rewrite), MOD025 (select-star).
//!
//! Production names are those of the vendored DuckDB grammar; `grebe tree`
//! prints them for any statement.

use std::collections::HashSet;

use grebe_syntax::cst::{NodeId, Tree};

use crate::detect::Finding;

/// The single child of `id` produced by `rule`, if there is exactly one.
///
/// [`crate::detect`] and [`crate::tier2b`] each carry an identical private
/// copy of this. Each tier module keeps its helpers private, so this is a
/// small, deliberate duplication rather than a shared import.
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

/// Node names that introduce a nested query scope. A "top-level only, no
/// subquery recursion" walk must stop at these rather than cross into
/// them. A copy of the identical constant in [`crate::tier2b`], kept
/// private like the other tier helpers.
const SCOPE_BOUNDARIES: &[&str] = &[
    "SubqueryReference",
    "InSelectStatement",
    "CTESelectBody",
    "SelectParens",
];

/// True if none of `n`'s ancestors, up to the tree root, is a
/// [`SCOPE_BOUNDARIES`] node -- i.e. `n` belongs to its enclosing
/// statement's own top-level scope, not a nested subquery/CTE/set-op arm
/// reached by crossing one of those boundaries on the way down from the
/// root.
fn is_top_level(tree: &Tree, mut n: NodeId) -> bool {
    while let Some(p) = tree.parent(n) {
        if SCOPE_BOUNDARIES.contains(&tree.rule_name(p)) {
            return false;
        }
        n = p;
    }
    true
}

/// Every node produced by `rule` reachable from `root` without crossing a
/// [`SCOPE_BOUNDARIES`] node -- i.e. belonging to `root`'s own query, not
/// a nested one. Pre-order. A copy of the identical helper in
/// [`crate::tier2b`].
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

/// Walk the single-child expression-wrapper chain from `n` down to a bare
/// `StarExpression`, or `None` if the chain forks (more than one child at
/// some level, meaning `n` is not itself a star). A copy of the identical
/// helper in [`crate::tier2b`] -- see there for the rationale
/// (`count(*)`'s star sits inside a `FunctionExpression` argument, not at
/// the head of its own target-list item's chain, and must not match).
fn reduces_to_star(tree: &Tree, mut n: NodeId) -> Option<NodeId> {
    loop {
        if tree.rule_name(n) == "StarExpression" {
            return Some(n);
        }
        let children = tree.children(n);
        if children.len() != 1 {
            return None;
        }
        n = children[0];
    }
}

// ---------------------------------------------------------------------
// MOD025 -- select-star
// ---------------------------------------------------------------------

/// MOD025 select-star: a bare `*` in the outermost select list --
/// `SELECT *` or `WITH ... SELECT *` -- an unstable contract for anything
/// other than exploratory querying: the result's columns change whenever
/// the source's do. Opt-in because a bare `*` is idiomatic for exploration;
/// projects that want explicit column lists can turn it on.
///
/// Only the top-level `TargetList` is examined: `is_top_level` is what
/// makes "outermost" in the rule's name a structural guarantee rather
/// than a convention to remember, the same way `top_level_find` does for
/// MOD017. Within it, each item is checked for a bare `StarExpression`
/// (via `reduces_to_star`, the same technique MOD017/MOD022 use)
/// with no `StarQualifierList` present, which excludes `t.*`.
///
/// A star narrowed by `ExcludeList`/`ReplaceList`/`RenameList` does not
/// fire either (see the guard in the body).
///
/// Fires on *any* depth-0 bare star, even one mixed among other columns
/// (`SELECT a, * FROM t`): it walks *every* top-level item looking for
/// the first bare star, rather than requiring the star be the list's
/// *sole* item the way MOD017/MOD022's `sole_star` helper does.
///
/// FROM-first (`FROM t`, no `SELECT` segment at all) structurally can't
/// reach this detector: a `FromSelectClause` has no `TargetList` to walk,
/// so it does not fire.
pub fn select_star_outer(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for target_list in tree.find("TargetList") {
        if !is_top_level(tree, target_list) {
            continue;
        }
        let items: Vec<NodeId> = tree
            .children(target_list)
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "AliasedExpression")
            .collect();
        // First-hit-only: at most one finding per top-level list.
        //
        // A star carrying EXCLUDE / REPLACE / RENAME does not fire: the
        // modifiers are the acceptable way to shape a result set, and only a
        // bare star is worth flagging.
        let hit = items.iter().find_map(|&item| {
            let star = reduces_to_star(tree, item)?;
            let narrowed = tree.children(star).iter().any(|&c| {
                matches!(
                    tree.rule_name(c),
                    "StarQualifierList" | "ExcludeList" | "ReplaceList" | "RenameList"
                )
            });
            (!narrowed).then_some(star)
        });
        if let Some(star) = hit {
            out.push(Finding {
                code: "MOD025",
                span: tree.node(star).span,
                // No fix: can't mechanically know the real column names.
                fix: None,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------
// MOD021 -- qualify-rewrite
// ---------------------------------------------------------------------

/// The alias text of a `TargetList` item (an `AliasedExpression`),
/// lowercased, or `None` if it carries no alias.
///
/// `AliasedExpression`'s only child is one of two wrapper productions
/// (`grebe tree` on `a AS b` and on bare `a b` shows which):
/// `ExpressionOptIdentifier <- Expression Identifier?` (no `AS`, alias
/// optional) or `ExpressionAsCollabel <- Expression 'AS'?
/// ColLabelOrString` (used whenever `AS` is present). Either way, when an
/// alias is present the wrapper has exactly two children and the alias
/// is the second.
fn alias_text(tree: &Tree, item: NodeId, src: &str) -> Option<String> {
    let &wrapper = tree.children(item).first()?;
    let kids = tree.children(wrapper);
    let &ident = kids.get(1)?;
    Some(tree.text(ident, src).trim().to_lowercase())
}

/// The last `.`-separated component of `id`'s own text, lowercased -- a
/// CST-shape-agnostic way to reduce any `ColumnReference` (unqualified
/// `ColumnName`, qualified `TableReservedColumnName`, or any other final-
/// identifier nesting the grammar might use) to its bare column name.
/// A text operation by design, rather than one chased through the CST
/// shape.
fn last_dotted_lower(tree: &Tree, id: NodeId, src: &str) -> String {
    tree.text(id, src)
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// True if `item`'s expression subtree contains a `FunctionExpression`
/// with a present `OverClause`.
fn has_window_function(tree: &Tree, item: NodeId) -> bool {
    tree.descendants(item).into_iter().any(|n| {
        tree.rule_name(n) == "FunctionExpression"
            && tree
                .children(n)
                .iter()
                .any(|&c| tree.rule_name(c) == "OverClause")
    })
}

/// MOD021 qualify-rewrite: a derived table or CTE that computes a
/// window-function result under an alias, wrapped by an outer query whose
/// **only** purpose is to filter on that alias -- `QUALIFY` does this in
/// place, without the wrapper.
///
/// Guards:
///
/// - the top-level `WHERE` must be present;
/// - the top-level `FROM` must have **exactly one** source with **no**
///   join -- either shows up as more than one `TableRef` (a comma cross
///   join) or as a single `TableRef` with a `JoinOrPivot` sibling (an
///   explicit `JOIN`), both excluded;
/// - that sole source must be a subquery or a **by-name** CTE reference,
///   nothing deeper (not a table function, not a derived table of a
///   derived table);
/// - the inner query's top-level `TargetList` must have an item whose
///   alias is one of the outer `WHERE`'s referenced columns *and* whose
///   expression contains a window function.
///
/// First match wins -- one finding per statement, on the `WhereClause`'s
/// `WHERE` keyword token.
///
/// "Top-level" throughout means *this statement's own* clauses -- the
/// same `is_top_level`/`top_level_find` boundary every rule in this file
/// uses -- not nested inside a further subquery.
///
/// No fix: the registry's fix safety is `None` even though its message
/// names QUALIFY as the rewrite target. The actual rewrite requires
/// re-homing the inner query's `FROM`/`WHERE`/other clauses up into the
/// outer statement, removing the wrapper layer, and
/// possibly excluding the alias column from the final output -- a
/// structural statement-level transform well beyond a byte-span edit.
/// Detect-only, deliberately.
pub fn qualify_rewrite(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();

    for stmt in tree.find("SelectStatementInternal") {
        if !is_top_level(tree, stmt) {
            continue;
        }
        let Some(&where_clause) = top_level_find(tree, stmt, "WhereClause").first() else {
            continue;
        };
        let Some(&from_clause) = top_level_find(tree, stmt, "FromClause").first() else {
            continue;
        };

        let where_cols: HashSet<String> = tree
            .descendants(where_clause)
            .into_iter()
            .filter(|&n| tree.rule_name(n) == "ColumnReference")
            .map(|n| last_dotted_lower(tree, n, src))
            .collect();

        let table_refs: Vec<NodeId> = tree
            .children(from_clause)
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "TableRef")
            .collect();
        let [table_ref] = table_refs[..] else {
            // Zero sources (shouldn't happen alongside a WHERE) or more
            // than one -- a comma cross join. Either way, not a single
            // source.
            continue;
        };
        if tree
            .children(table_ref)
            .iter()
            .any(|&c| tree.rule_name(c) == "JoinOrPivot")
        {
            continue;
        }
        let Some(inner_table_ref) = child(tree, table_ref, "InnerTableRef") else {
            continue;
        };

        let inner_stmt = if let Some(table_subquery) = child(tree, inner_table_ref, "TableSubquery")
        {
            child(tree, table_subquery, "SubqueryReference")
                .and_then(|sq| child(tree, sq, "SelectStatementInternal"))
        } else if let Some(base_table_ref) = child(tree, inner_table_ref, "BaseTableRef") {
            let table_name = child(tree, base_table_ref, "BaseTableName")
                .map(|n| tree.text(n, src).trim().to_lowercase());
            let Some(table_name) = table_name else {
                continue;
            };
            child(tree, stmt, "WithClause")
                .into_iter()
                .flat_map(|with_clause| {
                    tree.children(with_clause)
                        .iter()
                        .copied()
                        .filter(|&c| tree.rule_name(c) == "WithStatement")
                        .collect::<Vec<_>>()
                })
                .find(|&ws| {
                    child(tree, ws, "ColIdOrString")
                        .is_some_and(|n| tree.text(n, src).trim().to_lowercase() == table_name)
                })
                .and_then(|ws| {
                    let cte_body = child(tree, ws, "CTEBody")?;
                    let cte_select_body = child(tree, cte_body, "CTESelectBody")?;
                    child(tree, cte_select_body, "SelectStatementInternal")
                })
        } else {
            // Something other than a subquery or a base-table-shaped
            // reference (e.g. a table function) -- not this pattern.
            None
        };
        let Some(inner_stmt) = inner_stmt else {
            continue;
        };

        let Some(&target_list) = top_level_find(tree, inner_stmt, "TargetList").first() else {
            continue;
        };
        let items: Vec<NodeId> = tree
            .children(target_list)
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "AliasedExpression")
            .collect();

        let hit = items.iter().any(|&item| {
            alias_text(tree, item, src).is_some_and(|a| where_cols.contains(&a))
                && has_window_function(tree, item)
        });
        if hit {
            let kw_span = tree
                .code_tokens(where_clause)
                .first()
                .map(|t| t.span)
                .unwrap_or_else(|| tree.node(where_clause).span);
            out.push(Finding {
                code: "MOD021",
                span: kw_span,
                fix: None,
            });
        }
    }

    out
}

/// Every tier-3 detector, in registry order (MOD021, MOD025).
pub fn analyze_tier3(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(qualify_rewrite(tree, src));
    out.extend(select_star_outer(tree, src));
    out.sort_by_key(|f| (f.span.start, f.code));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use grebe_syntax::matcher::parse;

    fn codes(sql: &str) -> Vec<&'static str> {
        let tree = parse(sql).unwrap_or_else(|| panic!("did not parse: {sql}"));
        analyze_tier3(&tree, sql)
            .into_iter()
            .map(|f| f.code)
            .collect()
    }

    fn fires(code: &str, sql: &str) -> bool {
        codes(sql).contains(&code)
    }

    fn count(code: &str, sql: &str) -> usize {
        codes(sql).into_iter().filter(|&c| c == code).count()
    }

    // -------------------------------------------------------------
    // MOD025 -- select-star
    // -------------------------------------------------------------

    #[test]
    fn mod025_select_star_outer() {
        for sql in [
            "SELECT * FROM t",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            // Fires on any depth-0 bare star, even mixed with columns.
            "SELECT a, * FROM t",
        ] {
            assert!(fires("MOD025", sql), "should fire: {sql}");
        }
    }

    #[test]
    fn mod025_must_not_fire() {
        for sql in [
            // qualified star -- StarQualifierList present
            "SELECT t.* FROM t",
            // no star at all
            "SELECT a, b FROM t",
            // FROM-first: no TargetList exists to walk at all
            "FROM t",
            // Narrowed stars: the modifiers are the acceptable way to shape
            // a result set, and flagging them buries the bare-star case that
            // is actually worth reading.
            "SELECT * EXCLUDE (a) FROM t",
            "SELECT * REPLACE (upper(a) AS a) FROM t",
            "SELECT * RENAME (a AS b) FROM t",
            "SELECT * EXCLUDE (a) REPLACE (upper(b) AS b) FROM t",
        ] {
            assert!(!fires("MOD025", sql), "should NOT fire: {sql}");
        }
        // The bare star it exists for still fires.
        assert!(fires("MOD025", "SELECT * FROM t"));
    }

    #[test]
    fn mod025_outermost_only_no_subquery_recursion() {
        // Only the outer star fires; the inner subquery's own star must
        // not produce a second finding.
        let sql = "SELECT * FROM (SELECT * FROM inner_t) outer_t";
        assert_eq!(count("MOD025", sql), 1);
    }

    // -------------------------------------------------------------
    // MOD021 -- qualify-rewrite
    // -------------------------------------------------------------

    const SUBQUERY_FIRE: &str = "SELECT * FROM (SELECT *, row_number() \
        OVER (PARTITION BY a ORDER BY b) AS rn FROM t) sub WHERE rn = 1";
    const CTE_FIRE: &str = "WITH c AS (SELECT *, row_number() OVER \
        (PARTITION BY a ORDER BY b) AS rn FROM t) SELECT * FROM c WHERE rn = 1";

    #[test]
    fn mod021_qualify_rewrite_fires() {
        for sql in [SUBQUERY_FIRE, CTE_FIRE] {
            assert!(fires("MOD021", sql), "should fire: {sql}");
        }
    }

    #[test]
    fn mod021_must_not_fire_on_join() {
        // The outer FROM has more than one source (a JOIN), which the
        // rule excludes rather than reasoning about.
        let sql = "SELECT * FROM (SELECT *, row_number() OVER (PARTITION \
            BY a ORDER BY b) AS rn FROM t) sub JOIN u ON sub.id = u.id \
            WHERE rn = 1";
        assert!(!fires("MOD021", sql));
    }

    #[test]
    fn mod021_must_not_fire_on_comma_join() {
        let sql = "SELECT * FROM (SELECT *, row_number() OVER (PARTITION BY a \
            ORDER BY b) AS rn FROM t) sub, u WHERE rn = 1";
        assert!(!fires("MOD021", sql));
    }

    #[test]
    fn mod021_must_not_fire_without_window_function() {
        // The inner aliased item has no window function anywhere in its
        // subtree.
        let sql = "SELECT * FROM (SELECT *, 1 AS rn FROM t) sub WHERE rn = 1";
        assert!(!fires("MOD021", sql));
    }

    #[test]
    fn mod021_must_not_fire_on_unrelated_where_column() {
        // The outer WHERE references a column that isn't one of the
        // inner query's window-aliased columns.
        let sql = "SELECT * FROM (SELECT *, row_number() OVER (PARTITION \
            BY a ORDER BY b) AS rn FROM t) sub WHERE other_col = 1";
        assert!(!fires("MOD021", sql));
    }

    #[test]
    fn mod021_must_not_fire_on_plain_base_table() {
        // The outer FROM references a plain base table -- neither a
        // subquery nor a by-name CTE reference.
        let sql = "SELECT * FROM t WHERE rn = 1";
        assert!(!fires("MOD021", sql));
    }

    #[test]
    fn mod021_must_not_fire_without_where() {
        let sql = "SELECT * FROM (SELECT *, row_number() OVER (PARTITION \
            BY a ORDER BY b) AS rn FROM t) sub";
        assert!(!fires("MOD021", sql));
    }

    #[test]
    fn mod021_reports_at_where_keyword_span() {
        let tree = parse(SUBQUERY_FIRE).unwrap();
        let f = analyze_tier3(&tree, SUBQUERY_FIRE)
            .into_iter()
            .find(|f| f.code == "MOD021")
            .unwrap();
        assert!(f.fix.is_none(), "MOD021 is detect-only");
        let marked = &SUBQUERY_FIRE[f.span.start as usize..f.span.end as usize];
        assert_eq!(marked, "WHERE");
    }
}
