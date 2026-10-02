//! Tier 2a CST detectors: MOD002, MOD008, MOD011, MOD012, MOD013, MOD019,
//! MOD023.
//!
//! Same contract as [`crate::detect`]: each detector reads the lossless CST
//! directly and findings are structured values (a code and a byte span).
//! MOD019 and MOD023 are opt-in (off by default in the registry); that only
//! affects whether a run includes them by default, not how they detect, so
//! it does not show up below.

use crate::detect::{Finding, Fix};
use grebe_syntax::Span;
use grebe_syntax::cst::{NodeId, Tree};

fn text<'s>(tree: &Tree, id: NodeId, src: &'s str) -> &'s str {
    tree.text(id, src)
}

/// The single child of `id` produced by `rule`, if there is exactly one.
///
/// Duplicated from `detect.rs` rather than imported: that module's helpers
/// are private to it, and this file is a sibling module, not a descendant.
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

/// All direct children of `id` produced by `rule`, in source order.
fn children_of(tree: &Tree, id: NodeId, rule: &str) -> Vec<NodeId> {
    tree.children(id)
        .iter()
        .copied()
        .filter(|&c| tree.rule_name(c) == rule)
        .collect()
}

/// True if `id`'s own span is entirely covered by a descendant produced by
/// `rule` -- i.e. `id` reduces, through the expression precedence chain, to
/// exactly that shape and nothing else. Not merely "contains a `rule` node
/// somewhere deeper" (`x + 1` contains a `NumberLiteral` but does not reduce
/// to one), and not "wrapped in something that would widen the span" (`a[1]`
/// contains a `ColumnReference` spanning just `a`, narrower than the whole
/// `a[1]` -- the `IndirectionList` sibling that would need excluding
/// separately is caught for free by the span mismatch).
fn reduces_to(tree: &Tree, id: NodeId, rule: &str) -> bool {
    let want = tree.node(id).span;
    tree.descendants(id)
        .into_iter()
        .any(|d| tree.rule_name(d) == rule && tree.node(d).span == want)
}

/// MOD002 null-comparison: `x = NULL` / `x != NULL` / `x <> NULL` (either
/// operand order) always evaluates to `NULL` under three-valued logic —
/// virtually always a bug; should be `IS [NOT] NULL`.
///
/// **Guard (load-bearing):** restricted to WHERE/HAVING/QUALIFY clauses only.
/// This is what keeps `UPDATE t SET x = NULL` from firing — a `SET`
/// assignment's right-hand side is an `Expression` with no comparison
/// wrapped around it at all (no `OtherOperatorTail`), so in the CST this
/// guard is structurally redundant with the shape of the assignment
/// production itself, but it is kept explicit so the restriction does not
/// depend on that shape.
///
/// The vendored grammar routes `=`/`==`/`!=`/`<>` (and other DuckDB "other
/// operators") through `OtherOperatorExpression <- BitwiseExpression
/// OtherOperatorTail*`, `OtherOperatorTail <- OtherOperator
/// BitwiseExpression` — a flat sequence, not a `ComparisonExpression` node
/// (that node exists but only wraps the whole chain; the operator and its
/// operands live one level down, in the tail).
/// For a tail at position *i*, its left operand is whatever came right
/// before it in that flat sequence: the parent's own leading
/// `BitwiseExpression` when *i* is the first tail, else the previous tail's
/// own right-hand `BitwiseExpression`.
pub fn null_comparison(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for tail in tree.find("OtherOperatorTail") {
        let in_condition_clause = tree.ancestor(tail, "WhereClause").is_some()
            || tree.ancestor(tail, "HavingClause").is_some()
            || tree.ancestor(tail, "QualifyClause").is_some();
        if !in_condition_clause {
            continue;
        }
        let Some(parent) = tree.parent(tail) else {
            continue;
        };
        if tree.rule_name(parent) != "OtherOperatorExpression" {
            continue;
        }
        let Some(op_node) = child(tree, tail, "OtherOperator") else {
            continue;
        };
        let is_not = match text(tree, op_node, src) {
            "=" | "==" => false,
            "!=" | "<>" => true,
            _ => continue,
        };
        let Some(right) = child(tree, tail, "BitwiseExpression") else {
            continue;
        };

        let siblings = tree.children(parent);
        let Some(idx) = siblings.iter().position(|&s| s == tail) else {
            continue;
        };
        if idx == 0 {
            continue; // malformed: a tail is never the parent's first child
        }
        let prev = siblings[idx - 1];
        let left = if tree.rule_name(prev) == "BitwiseExpression" {
            prev
        } else {
            let Some(l) = child(tree, prev, "BitwiseExpression") else {
                continue;
            };
            l
        };

        let right_is_null = reduces_to_null(tree, right);
        let left_is_null = !right_is_null && reduces_to_null(tree, left);
        if !right_is_null && !left_is_null {
            continue;
        }

        let verb = if is_not { "IS NOT NULL" } else { "IS NULL" };
        if right_is_null {
            // Simple splice: operator through NULL's end becomes `IS [NOT] NULL`.
            let sp = Span::new(tree.node(op_node).span.start, tree.node(right).span.end);
            out.push(Finding {
                code: "MOD002",
                span: sp,
                fix: Some(Fix {
                    span: sp,
                    replacement: verb.to_string(),
                }),
            });
        } else {
            // `NULL = x` -- DuckDB's `IS [NOT] NULL` is postfix on the
            // non-NULL operand, so this is a reorder, not a splice: replace
            // the whole comparison span with `<right operand text> IS [NOT]
            // NULL`. Marked unsafe in the registry: even though the rewrite
            // is "even wrong -> right" (always-NULL becomes a meaningful
            // boolean), it is still a semantic change, not a preserving one.
            let sp = Span::new(tree.node(left).span.start, tree.node(right).span.end);
            let replacement = format!("{} {verb}", text(tree, right, src));
            out.push(Finding {
                code: "MOD002",
                span: sp,
                fix: Some(Fix {
                    span: sp,
                    replacement,
                }),
            });
        }
    }
    out
}

/// True if `id`'s own span is entirely covered by a `NullLiteral`
/// descendant -- i.e. the operand reduces, through the expression precedence
/// chain, to NULL and nothing else (not merely an expression containing NULL
/// somewhere inside it, like `NULL + 1`).
fn reduces_to_null(tree: &Tree, id: NodeId) -> bool {
    reduces_to(tree, id, "NullLiteral")
}

/// MOD023 prefix-alias (opt-in, off by default): `expr AS x` in a select
/// list, where DuckDB's prefix form `x: expr` says the same thing with the
/// name first. Opt-in because it is house style, not a correctness concern;
/// the fix is safe because DuckDB defines the prefix alias as the same alias.
///
/// **Guard:** node-kind match on `ExpressionAsCollabel` is the only guard
/// needed. The grammar's `AliasedExpression <- ColIdExpression /
/// ExpressionAsCollabel / ExpressionOptIdentifier` already separates the
/// three select-list-item shapes as distinct alternatives, and
/// `AliasedExpression` only ever occurs as a `TargetList` element -- so a
/// table alias (`FROM t AS u`, a `TableAlias` node, a completely different
/// production) can never be mistaken for one, and an already-prefix item
/// (`t: total`, `ColIdExpression`) never reaches this arm at all. No
/// depth-tracking needed.
pub fn prefix_alias(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for n in tree.find("ExpressionAsCollabel") {
        let Some(expr) = child(tree, n, "Expression") else {
            continue;
        };
        let Some(label) = child(tree, n, "ColLabelOrString") else {
            continue;
        };
        let sp = Span::new(tree.node(expr).span.start, tree.node(label).span.end);
        // Reorder, not splice: `<expr> AS <alias>` -> `<alias>: <expr>`.
        let replacement = format!("{}: {}", text(tree, label, src), text(tree, expr, src));
        out.push(Finding {
            code: "MOD023",
            span: sp,
            fix: Some(Fix {
                span: sp,
                replacement,
            }),
        });
    }
    out
}

/// MOD008 bare-union: `UNION` between two SELECTs without `ALL`,
/// `DISTINCT`, or `BY NAME` -- the default silently dedups, surprising
/// people who expect `UNION` to behave like an append.
///
/// **Guard:** none needed. Textually, a set-op `UNION` and the **UNION
/// type** constructor (`x::UNION(a INT, ...)`) both look like "the word
/// UNION followed by an open paren". In the grammar they are different
/// productions reachable from different contexts: `SetopClause <- SetopType
/// DistinctOrAll? ByName?` (a genuine set operation between two
/// `SelectStatementInternal`s) versus `UnionType <- 'UNION' ColIdTypeList`
/// (reachable only from `Type` context). `find("SetopClause")` structurally
/// can never see the latter; the tests include `x::UNION(a INT, b VARCHAR)`,
/// which produces no `SetopClause` at all.
///
/// `EXCEPT`/`INTERSECT` never reach this detector either: `SetopType <-
/// SetopUnion / SetopExcept` has no `INTERSECT` alternative (that operator
/// is handled by a separate `IntersectChain` production), and the
/// `SetopExcept` arm is filtered out below, matching the registry message
/// which is UNION-specific.
pub fn bare_union(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for setop in tree.find("SetopClause") {
        let Some(setop_type) = child(tree, setop, "SetopType") else {
            continue;
        };
        if child(tree, setop_type, "SetopUnion").is_none() {
            continue; // EXCEPT, not UNION
        }
        if child(tree, setop, "DistinctOrAll").is_some() || child(tree, setop, "ByName").is_some() {
            continue;
        }
        // No fix: picking ALL vs DISTINCT changes results and the tool can't
        // guess intent (registry fix safety `None`).
        out.push(Finding {
            code: "MOD008",
            span: tree.node(setop).span,
            fix: None,
        });
    }
    out
}

/// Item-count threshold for MOD019. Below this, a literal `IN` list isn't
/// worth flagging even under the (unmeasured) perf premise.
const LARGE_IN_LIST_THRESHOLD: usize = 20;

/// MOD019 large-in-list (opt-in, off by default): `IN (...)` with >= 20
/// literal items. The premise, from DuckDB's performance guide -- a `JOIN`
/// against `VALUES` scales better than a huge `IN` list -- is unmeasured
/// here, so the rule is opt-in only.
///
/// **Guards (both must hold):** every item literal-only (any single
/// non-literal item -- column ref, function call, nested expression --
/// disqualifies the whole list, no partial credit), and the item count
/// meeting `LARGE_IN_LIST_THRESHOLD`.
pub fn large_in_list(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    // `InExpressionList <- Parens(List(Expression))` is one of three
    // `InExpression` alternatives (the others are `InSelectStatement` and
    // `InContainsExpression`); matching this node kind alone settles list
    // vs. subquery from the grammar, with no inference from the token
    // after `(`.
    for list in tree.find("InExpressionList") {
        let items = children_of(tree, list, "Expression");
        if items.len() < LARGE_IN_LIST_THRESHOLD {
            continue;
        }
        if !items.iter().all(|&it| is_literal_item(tree, it)) {
            continue;
        }
        // No fix: rewriting to `JOIN (VALUES ...)` needs a synthesized table
        // alias and column name -- a human decision, not mechanical.
        out.push(Finding {
            code: "MOD019",
            span: tree.node(list).span,
            fix: None,
        });
    }
    out
}

/// True if the `Expression` at `expr_id` is, in its entirety, a single
/// string/number literal -- optionally wrapped in a unary minus.
fn is_literal_item(tree: &Tree, expr_id: NodeId) -> bool {
    let want = tree.node(expr_id).span;
    let direct = tree.descendants(expr_id).into_iter().any(|d| {
        matches!(tree.rule_name(d), "NumberLiteral" | "StringLiteral") && tree.node(d).span == want
    });
    if direct {
        return true;
    }
    // DuckDB parses a negative literal like `-5` as a unary-minus
    // `PrefixExpression <- PrefixOperator* BaseExpression` wrapping a
    // `NumberLiteral`, one level deeper than the direct case above, so a
    // leading `-` is allowed here explicitly.
    tree.descendants(expr_id).into_iter().any(|d| {
        if tree.rule_name(d) != "PrefixExpression" || tree.node(d).span != want {
            return false;
        }
        let ops = children_of(tree, d, "PrefixOperator");
        let single_minus =
            ops.len() == 1 && !children_of(tree, ops[0], "MinusPrefixOperator").is_empty();
        if !single_minus {
            return false;
        }
        let Some(base) = child(tree, d, "BaseExpression") else {
            return false;
        };
        let base_span = tree.node(base).span;
        tree.descendants(base)
            .into_iter()
            .any(|n| tree.rule_name(n) == "NumberLiteral" && tree.node(n).span == base_span)
    })
}

/// MOD011 ordinal-reference: `GROUP BY 1` / `ORDER BY 2` -- a positional
/// reference into the select list that breaks silently when the list is
/// edited.
///
/// **Guard:** the list item's `Expression` must reduce, at its own top
/// level, to exactly a bare `NumberLiteral` (`reduces_to`, shared with
/// MOD013 below) -- `x + 1` and `foo(1)` parse as different expression
/// shapes (`AdditiveExpression`/`FunctionExpression`) *wrapping* a literal
/// deeper in the tree, never as the literal itself as the item, so the
/// span-equality check rejects them with no manual depth-tracking. `GROUP BY
/// ALL` never reaches the list-item walk at all: `GroupByExpressions <-
/// GroupByList / GroupByAll` puts the `ALL` form on a different grammar
/// alternative with no `GroupByList` sibling to iterate. A signed literal
/// like `GROUP BY -1` also does not match -- DuckDB's grammar parses unary
/// minus as a `PrefixExpression` wrapping the `NumberLiteral` one level
/// deeper (as `grebe tree` shows), so the item's own top level is a
/// `PrefixExpression`, not the literal.
///
/// **Only the first ordinal per clause fires**: `ORDER BY x, 1` produces
/// one finding, not a second one for the `1`.
///
/// **Fix:** none. The registry does not mark this safe -- resolving an
/// ordinal to the select-list item it names (and possibly synthesizing an
/// alias) is a human call, not a mechanical rewrite.
pub fn ordinal_reference(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();

    for gb in tree.find("GroupByClause") {
        let Some(exprs) = child(tree, gb, "GroupByExpressions") else {
            continue;
        };
        let Some(list) = child(tree, exprs, "GroupByList") else {
            continue; // GROUP BY ALL -- no list to walk
        };
        for item in children_of(tree, list, "GroupByExpression") {
            // The other three `GroupByExpression` alternatives
            // (EmptyGroupingItem/CubeOrRollupClause/GroupingSetsClause)
            // simply have no `GroupByBaseExpression` child, so they're
            // skipped for free -- none of them can structurally be an
            // ordinal.
            let Some(base) = child(tree, item, "GroupByBaseExpression") else {
                continue;
            };
            let Some(expr) = child(tree, base, "Expression") else {
                continue;
            };
            if reduces_to(tree, expr, "NumberLiteral") {
                out.push(Finding {
                    code: "MOD011",
                    span: tree.node(expr).span,
                    fix: None,
                });
                break; // first hit per clause only
            }
        }
    }

    for ob in tree.find("OrderByClause") {
        let Some(exprs) = child(tree, ob, "OrderByExpressions") else {
            continue;
        };
        let Some(list) = child(tree, exprs, "OrderByExpressionList") else {
            continue; // ORDER BY ALL -- no list to walk
        };
        for item in children_of(tree, list, "OrderByExpression") {
            // `OrderByExpression <- Expression DescOrAsc? NullsFirstOrLast?`
            // -- the trailing modifiers are siblings of `Expression`, not
            // wrapping it, so reading the `Expression` child directly
            // already ignores them without extra unwrapping.
            let Some(expr) = child(tree, item, "Expression") else {
                continue;
            };
            if reduces_to(tree, expr, "NumberLiteral") {
                out.push(Finding {
                    code: "MOD011",
                    span: tree.node(expr).span,
                    fix: None,
                });
                break; // first hit per clause only
            }
        }
    }

    out
}

/// MOD012 view-order-by: `CREATE VIEW ... AS SELECT ... ORDER BY ...` with
/// no `LIMIT`/`OFFSET` in the view body -- same premise as MOD004, applied
/// to a view's own top-level `ORDER BY`: the order is not part of the
/// view's contract.
///
/// **Guard:** node-kind match on `CreateViewStmt` is the entire guard.
/// `create_view.gram`'s production (`CreateSecure? CreateRecursive? 'VIEW'
/// IfNotExists? QualifiedName InsertColumnList? WithList? 'AS'
/// SelectStatementInternal`) identifies a view exactly --
/// `CreateSecure?`/`CreateRecursive?` are just optional leading children of
/// the same node, so `CREATE OR REPLACE VIEW`/`CREATE TEMP VIEW` match
/// without counting tokens after `CREATE`, and a `CreateViewStmt` node can never
/// be confused with `CREATE TABLE`/`CREATE MACRO` (different alternatives of
/// `CreateStatementVariation` entirely).
///
/// **Fix:** none, same rationale as MOD004 -- the `ORDER BY` might be
/// intentional documentation of expected order even though a view doesn't
/// guarantee it; deleting it changes nothing about correctness but does
/// delete something a human wrote on purpose.
pub fn view_order_by(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for view in tree.find("CreateViewStmt") {
        let Some(body) = child(tree, view, "SelectStatementInternal") else {
            continue;
        };
        let Some(result_mods) = child(tree, body, "ResultModifiers") else {
            continue;
        };
        let Some(order_clause) = child(tree, result_mods, "OrderByClause") else {
            continue;
        };
        if child(tree, result_mods, "LimitOffset").is_some() {
            continue;
        }
        out.push(Finding {
            code: "MOD012",
            span: tree.node(order_clause).span,
            fix: None,
        });
    }
    out
}

/// The `Expression` a `TargetList` item wraps, regardless of which shape it
/// is written in.
///
/// `AliasedExpression <- ColIdExpression / ExpressionAsCollabel /
/// ExpressionOptIdentifier` -- unlike `GroupByExpression`/`OrderByExpression`
/// above, `AliasedExpression`'s `Expression` is not a direct child: the node
/// tree has one more hop through whichever of the three alternatives matched
/// (`grebe tree` on `SELECT a` shows `AliasedExpression [7..8] "a"` wrapping
/// `ExpressionOptIdentifier [7..8] "a"` wrapping `Expression [7..8] "a"`,
/// not `Expression` directly). All three alternatives carry an `Expression`
/// child of their own, so which one matched doesn't matter for MOD013 --
/// aliased or not, the wrapped `Expression` is what gets compared.
fn target_list_item_expr(tree: &Tree, item: NodeId) -> Option<NodeId> {
    for alt in [
        "ColIdExpression",
        "ExpressionAsCollabel",
        "ExpressionOptIdentifier",
    ] {
        if let Some(node) = child(tree, item, alt) {
            return child(tree, node, "Expression");
        }
    }
    None
}

/// The scope's own top-level select-list `TargetList`, if -- and only if --
/// this scope is a single, non-set-operation `SELECT ... FROM ...` with no
/// parenthesized wrapping anywhere along the way. MOD013's "the statement's
/// whole select list" premise only makes sense for that shape: a `UNION`
/// chain has one select list per arm, not one for the statement (DuckDB's
/// `json_serialize_sql` likewise gives a SETOP top node no `select_list` of
/// its own). Anything else bails out with `None`.
fn simple_target_list(tree: &Tree, scope: NodeId) -> Option<NodeId> {
    let chain = child(tree, scope, "SelectSetOpChain")?;
    if !children_of(tree, chain, "SelectSetOpChainTail").is_empty() {
        return None; // UNION/EXCEPT arm present
    }
    let ichain = child(tree, chain, "IntersectChain")?;
    if !children_of(tree, ichain, "IntersectChainTail").is_empty() {
        return None; // INTERSECT arm present
    }
    let atom = child(tree, ichain, "SelectAtom")?;
    let stype = child(tree, atom, "SelectStatementType")?; // None if SelectParens
    let parens_simple = child(tree, stype, "OptionalParensSimpleSelect")?;
    let simple = child(tree, parens_simple, "SimpleSelect")?; // None if SimpleSelectParens
    let from = child(tree, simple, "SelectFrom")?;
    let from_clause = child(tree, from, "SelectFromClause")?; // None if FromSelectClause
    let select_clause = child(tree, from_clause, "SelectClause")?;
    child(tree, select_clause, "TargetList")
}

/// MOD013 order-by-all: an explicit `ORDER BY` list that is, in order,
/// exactly the statement's whole select list -- `ORDER BY ALL` says the same
/// without repeating it.
///
/// **Comparison is positional, not sorted** -- the direct opposite of
/// MOD003's GROUP BY version. `ORDER BY a, b` and `ORDER BY b, a` sort
/// differently, so this rule must not sort either list before comparing;
/// `ORDER BY ALL` applies the select list's own order. Copying MOD003's
/// sorted-set-equality habit here would be a real bug.
///
/// **Guards (every one is all-or-nothing across the whole list, no partial
/// credit):** already-`ORDER BY ALL` skip (`OrderByExpressions`'s
/// `OrderByAll` alternative, a direct grammar fact); any `NullsFirstOrLast`
/// on any order item disqualifies (`ALL` applies uniformly, not per-column);
/// any `DescOrAsc` other than `AscendingOrder` (i.e. `DESC`/`DESCENDING`)
/// on any order item disqualifies, for the same reason; any order item or
/// select item that is not a bare `ColumnReference` (via `reduces_to`, which
/// also excludes indirection like `a[1]` for free by span mismatch)
/// disqualifies the whole statement.
///
/// **Fix:** safe, because the explicit list was proven equal, position by
/// position, to what `ORDER BY ALL` would expand to.
///
/// **Fix-span vs. diagnostic-span (they differ):**
/// the *diagnostic* span is narrow -- just the `ORDER`/`BY` keyword tokens,
/// via `code_tokens` (the two tokens `OrderByClause` owns directly; nothing
/// else is a direct token of that node since `OrderByExpressions` absorbs
/// everything after `BY` into a child node). The *fix* span is the whole
/// clause: `OrderByClause`'s own span already runs from `ORDER` through the
/// last list-item token, since nothing else follows `OrderByExpressions` in
/// the production -- so the fix reuses that whole-node span rather than
/// reconstructing it.
pub fn order_by_all(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();

    for scope in tree.find("SelectStatementInternal") {
        let Some(result_mods) = child(tree, scope, "ResultModifiers") else {
            continue;
        };
        let Some(order_clause) = child(tree, result_mods, "OrderByClause") else {
            continue;
        };
        let Some(order_exprs) = child(tree, order_clause, "OrderByExpressions") else {
            continue;
        };
        if child(tree, order_exprs, "OrderByAll").is_some() {
            continue; // already in ALL form -- must not re-fire
        }
        let Some(order_list) = child(tree, order_exprs, "OrderByExpressionList") else {
            continue;
        };
        let order_items = children_of(tree, order_list, "OrderByExpression");

        let mut onames: Vec<String> = Vec::with_capacity(order_items.len());
        let mut disqualified = false;
        for item in &order_items {
            if child(tree, *item, "NullsFirstOrLast").is_some() {
                disqualified = true;
                break;
            }
            if let Some(desc_or_asc) = child(tree, *item, "DescOrAsc") {
                if child(tree, desc_or_asc, "AscendingOrder").is_none() {
                    disqualified = true; // DESC/DESCENDING anywhere
                    break;
                }
            }
            let Some(expr) = child(tree, *item, "Expression") else {
                disqualified = true;
                break;
            };
            if !reduces_to(tree, expr, "ColumnReference") {
                disqualified = true;
                break;
            }
            onames.push(text(tree, expr, src).to_lowercase());
        }
        if disqualified {
            continue;
        }

        let Some(target_list) = simple_target_list(tree, scope) else {
            continue;
        };
        let select_items = children_of(tree, target_list, "AliasedExpression");
        let mut snames: Vec<String> = Vec::with_capacity(select_items.len());
        for item in &select_items {
            let Some(expr) = target_list_item_expr(tree, *item) else {
                break; // leaves `snames` short -- caught below
            };
            if !reduces_to(tree, expr, "ColumnReference") {
                break; // computed select item anywhere disqualifies the whole statement
            }
            snames.push(text(tree, expr, src).to_lowercase());
        }
        if snames.len() != select_items.len() {
            continue; // the early-break above fired: not all-bare-columns
        }

        if onames != snames {
            continue;
        }

        let toks = tree.code_tokens(order_clause);
        let diag_span = if toks.len() >= 2 {
            Span::new(toks[0].span.start, toks[1].span.end)
        } else {
            tree.node(order_clause).span // defensive: shouldn't happen for valid SQL
        };
        out.push(Finding {
            code: "MOD013",
            span: diag_span,
            fix: Some(Fix {
                span: tree.node(order_clause).span,
                replacement: "ORDER BY ALL".to_string(),
            }),
        });
    }

    out
}

/// Every tier-2a detector; findings are sorted by position, then code.
pub fn analyze_tier2a(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(null_comparison(tree, src));
    out.extend(ordinal_reference(tree, src));
    out.extend(view_order_by(tree, src));
    out.extend(order_by_all(tree, src));
    out.extend(prefix_alias(tree, src));
    out.extend(bare_union(tree, src));
    out.extend(large_in_list(tree, src));
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
        analyze_tier2a(&tree, sql)
            .into_iter()
            .map(|f| f.code)
            .collect()
    }

    fn fires(code: &str, sql: &str) -> bool {
        codes(sql).contains(&code)
    }

    #[test]
    fn mod002_null_comparison() {
        for sql in [
            "SELECT 1 WHERE x = NULL",
            "SELECT 1 WHERE NULL != y",
            "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) = NULL",
            // QUALIFY needs a window-function column to filter on; `rn` is a
            // `row_number()` alias.
            "SELECT a, row_number() OVER (ORDER BY a) AS rn FROM t QUALIFY rn <> NULL",
            // Same shape inside a subquery's WHERE.
            "SELECT * FROM (SELECT a FROM t WHERE a = NULL) sub",
        ] {
            assert!(fires("MOD002", sql), "should fire: {sql}");
        }
        for sql in [
            "UPDATE t SET x = NULL",
            "SELECT 1 WHERE x IS NULL",
            "SELECT 1 WHERE x = y",
        ] {
            assert!(!fires("MOD002", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod002_fix_reorders_when_null_is_left() {
        let sql = "SELECT 1 WHERE NULL != y";
        let tree = parse(sql).unwrap();
        let findings = null_comparison(&tree, sql);
        assert_eq!(findings.len(), 1);
        let fix = findings[0].fix.clone().expect("MOD002 has an unsafe fix");
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, "SELECT 1 WHERE y IS NOT NULL");
        assert!(parse(&fixed).is_some(), "fixed SQL must still parse");
    }

    #[test]
    fn mod002_fix_splices_when_null_is_right() {
        let sql = "SELECT 1 WHERE x = NULL";
        let tree = parse(sql).unwrap();
        let findings = null_comparison(&tree, sql);
        assert_eq!(findings.len(), 1);
        let fix = findings[0].fix.clone().expect("MOD002 has an unsafe fix");
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, "SELECT 1 WHERE x IS NULL");
        assert!(parse(&fixed).is_some(), "fixed SQL must still parse");
    }

    #[test]
    fn mod023_prefix_alias() {
        for sql in ["SELECT total AS t", "SELECT a + b AS sum"] {
            assert!(fires("MOD023", sql), "should fire: {sql}");
        }
        for sql in [
            "SELECT * FROM t AS u", // table alias -- different production
            "SELECT t: total",      // already prefix-form -- no AS token
        ] {
            assert!(!fires("MOD023", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod023_fix_reorders() {
        let sql = "SELECT total AS t";
        let tree = parse(sql).unwrap();
        let findings = prefix_alias(&tree, sql);
        assert_eq!(findings.len(), 1);
        let fix = findings[0].fix.clone().expect("MOD023 has a safe fix");
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, "SELECT t: total");
        let t2 = parse(&fixed).unwrap();
        assert!(
            prefix_alias(&t2, &fixed).is_empty(),
            "fix is not idempotent"
        );
    }

    #[test]
    fn mod008_bare_union() {
        assert!(fires("MOD008", "SELECT a FROM x UNION SELECT a FROM y"));
        for sql in [
            "SELECT a FROM x UNION ALL SELECT a FROM y",
            "SELECT a FROM x UNION DISTINCT SELECT a FROM y",
            "SELECT a FROM x UNION BY NAME SELECT a FROM y",
            // The UNION *type* -- a completely unrelated grammar construct.
            "SELECT x::UNION(a INT, b VARCHAR)",
        ] {
            assert!(!fires("MOD008", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod019_large_in_list() {
        let twenty = (1..=20)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT 1 WHERE id IN ({twenty})");
        assert!(fires("MOD019", &sql), "should fire: {sql}");

        let nineteen = (1..=19)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let under_threshold = format!("SELECT 1 WHERE id IN ({nineteen})");
        assert!(
            !fires("MOD019", &under_threshold),
            "should NOT fire: {under_threshold}"
        );

        assert!(!fires(
            "MOD019",
            "SELECT 1 WHERE id IN (SELECT id FROM other)"
        ));

        // One non-literal item (a column ref) disqualifies a 20-item list --
        // no partial credit.
        let mixed = format!(
            "{},a",
            (1..=19)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let mixed_sql = format!("SELECT 1 WHERE id IN ({mixed})");
        assert!(!fires("MOD019", &mixed_sql), "should NOT fire: {mixed_sql}");

        assert!(!fires("MOD019", "SELECT 1 WHERE id IN (1,2,3)"));
    }

    #[test]
    fn mod019_accepts_negative_literals() {
        let mut items: Vec<String> = vec!["-1".to_string()];
        items.extend((2..=20).map(|n| n.to_string()));
        let sql = format!("SELECT 1 WHERE id IN ({})", items.join(","));
        assert!(fires("MOD019", &sql), "should fire: {sql}");
    }

    #[test]
    fn mod011_ordinal_reference() {
        for sql in [
            "SELECT a, b FROM t GROUP BY 1, 2",
            "SELECT a, b, c FROM t ORDER BY 3 DESC",
            // Only "1" is an ordinal here ("x" isn't) -- still fires once.
            "SELECT x, a FROM t ORDER BY x, 1",
            // Scope discovery: an ordinal inside a subquery, not just the
            // top-level statement.
            "SELECT * FROM (SELECT a, b FROM t GROUP BY 1) sub",
        ] {
            assert!(fires("MOD011", sql), "should fire: {sql}");
        }
        for sql in [
            // The `1` is nested inside a larger expression, not the item
            // itself.
            "SELECT a FROM t GROUP BY x + 1",
            // The `1` is inside a function call argument.
            "SELECT a FROM t GROUP BY foo(1)",
            "SELECT a, b FROM t GROUP BY ALL",
            "SELECT a, b FROM t ORDER BY ALL",
            // No ordinals at all.
            "SELECT a, b FROM t GROUP BY a, b",
            "SELECT a, b FROM t ORDER BY a, b",
            // A signed literal parses as a `PrefixExpression` wrapping the
            // `NumberLiteral`, one level deeper than a bare ordinal.
            "SELECT a FROM t GROUP BY -1",
        ] {
            assert!(!fires("MOD011", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod011_no_fix_and_first_hit_only_per_clause() {
        let sql = "SELECT a, b FROM t GROUP BY 1, 2";
        let tree = parse(sql).unwrap();
        let findings = ordinal_reference(&tree, sql);
        assert_eq!(
            findings.len(),
            1,
            "only the first ordinal per clause should fire: {findings:?}"
        );
        assert!(findings[0].fix.is_none(), "MOD011 has no fix");
        // The finding points at the ordinal itself, not the whole clause.
        assert_eq!(
            &sql[findings[0].span.start as usize..findings[0].span.end as usize],
            "1"
        );
    }

    #[test]
    fn mod011_group_by_and_order_by_both_report() {
        // A statement with an ordinal in both clauses gets one finding per
        // clause -- the "first hit" rule is per-clause, not per-statement.
        let sql = "SELECT a, b FROM t GROUP BY 1 ORDER BY 2";
        assert_eq!(codes(sql), vec!["MOD011", "MOD011"]);
    }

    #[test]
    fn mod012_view_order_by() {
        assert!(fires(
            "MOD012",
            "CREATE VIEW v AS SELECT a FROM t ORDER BY a"
        ));
        assert!(fires(
            "MOD012",
            "CREATE OR REPLACE TEMP VIEW v AS SELECT a FROM t ORDER BY a"
        ));
        for sql in [
            "CREATE VIEW v AS SELECT a FROM t ORDER BY a LIMIT 10",
            "CREATE TABLE t (a INT)",
            "CREATE MACRO m(a) AS a + 1",
            "CREATE VIEW v AS SELECT a FROM t",
        ] {
            assert!(!fires("MOD012", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod012_no_fix() {
        let sql = "CREATE VIEW v AS SELECT a FROM t ORDER BY a";
        let tree = parse(sql).unwrap();
        let findings = view_order_by(&tree, sql);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].fix.is_none(), "MOD012 has no fix");
    }

    #[test]
    fn mod013_order_by_all() {
        assert!(fires("MOD013", "SELECT a, b FROM t ORDER BY a, b"));
        // Applies to any scope, not just the outermost statement -- a
        // view body's ORDER BY is a `SelectStatementInternal` scope too.
        assert!(fires(
            "MOD013",
            "CREATE VIEW v AS SELECT a, b FROM t ORDER BY a, b"
        ));
        for sql in [
            // Different order -- ORDER BY is order-significant, unlike
            // MOD003's GROUP BY comparison.
            "SELECT a, b FROM t ORDER BY b, a",
            // Partial list.
            "SELECT a, b FROM t ORDER BY a",
            // A non-ascending direction anywhere disqualifies the whole list.
            "SELECT a, b FROM t ORDER BY a DESC, b",
            // A computed order-by expression, not a bare column ref.
            "SELECT a, b FROM t ORDER BY a + 1, b",
            // Already in ALL form -- must not re-fire.
            "SELECT a, b FROM t ORDER BY ALL",
            // A computed select-list item disqualifies the whole statement,
            // even though the ORDER BY list itself is bare columns.
            "SELECT a + 1 AS c FROM t ORDER BY c",
        ] {
            assert!(!fires("MOD013", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod013_case_insensitive_and_aliased_columns_still_compare_by_name() {
        // Case-insensitive dotted-name comparison; an alias on the
        // select-list item doesn't change which underlying column is
        // compared.
        assert!(fires("MOD013", "SELECT A, b AS bee FROM t ORDER BY a, B"));
    }

    #[test]
    fn mod013_nulls_ordering_disqualifies() {
        assert!(!fires(
            "MOD013",
            "SELECT a, b FROM t ORDER BY a NULLS FIRST, b"
        ));
    }

    #[test]
    fn mod013_union_arm_does_not_fire() {
        // No single "whole select list" exists for a set-operation chain --
        // MOD013's premise doesn't apply, so this must not fire even though
        // each arm's own list happens to match its own ORDER BY textually
        // (the ORDER BY here binds to the whole chain, not either arm).
        assert!(!fires(
            "MOD013",
            "SELECT a, b FROM t UNION SELECT a, b FROM u ORDER BY a, b"
        ));
    }

    #[test]
    fn mod013_diagnostic_span_is_narrower_than_fix_span() {
        let sql = "SELECT a, b FROM t ORDER BY a, b";
        let tree = parse(sql).unwrap();
        let findings = order_by_all(&tree, sql);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        // Diagnostic span: just the `ORDER BY` keywords.
        assert_eq!(&sql[f.span.start as usize..f.span.end as usize], "ORDER BY");
        let fix = f.fix.clone().expect("MOD013 has a safe fix");
        // Fix span: the whole clause, `ORDER BY a, b` -- wider than the
        // diagnostic span, and NOT the same range.
        assert_eq!(
            &sql[fix.span.start as usize..fix.span.end as usize],
            "ORDER BY a, b"
        );
        assert_ne!(f.span, fix.span);
    }

    #[test]
    fn mod013_fix_replaces_whole_clause_with_order_by_all() {
        let sql = "SELECT a, b FROM t ORDER BY a, b";
        let tree = parse(sql).unwrap();
        let findings = order_by_all(&tree, sql);
        assert_eq!(findings.len(), 1);
        let fix = findings[0].fix.clone().unwrap();
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, "SELECT a, b FROM t ORDER BY ALL");
        let t2 = parse(&fixed).unwrap();
        assert!(
            order_by_all(&t2, &fixed).is_empty(),
            "fix is not idempotent"
        );
    }
}
