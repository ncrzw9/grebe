//! Tier 2b CST detectors: MOD010, MOD017, MOD018 and MOD022 (opt-in), and
//! MOD014, MOD015 and MOD016 (default-on).
//!
//! The four opt-in rules (`off` until `[severity]` enables them) fire too
//! often on idiomatic DuckDB SQL to default on. Hit counts on a corpus of
//! TPC-H/TPC-DS queries and examples from DuckDB's documentation: MOD017
//! 228 (`CREATE TABLE x AS SELECT * FROM 'file.csv'` is the idiomatic
//! DuckDB load pattern), MOD018 316 (the documentation's DDL declares
//! primary keys throughout), MOD010 ~408 (comma joins are idiomatic in
//! `range(...) a, range(...) b` and throughout TPC-H), and MOD022 2,496 (a
//! house-style preference, not a correctness issue). A high count for these
//! four on valid SQL is expected, not a sign of a broken detector.
//!
//! Production names are those of the vendored DuckDB grammar; `grebe tree`
//! prints them for any statement.

use std::collections::HashSet;
use std::sync::LazyLock;

use grebe_syntax::Span;
use grebe_syntax::cst::{NodeId, Tree};

use crate::detect::{Finding, Fix};

/// The single child of `id` produced by `rule`, if there is exactly one.
///
/// [`crate::detect`] has an identical private helper. Each tier module keeps
/// its helpers private, so this is a small, deliberate duplication rather
/// than a shared import.
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

/// Strip one layer of double-quoting from an identifier and lowercase it.
///
/// [`crate::detect`] has an `ident` helper doing the same job for MOD009; it
/// is private there, so this is the same small, deliberate duplication as `child`
/// above rather than a shared import. Used by MOD016 to compare a CTE's
/// declared name against a table reference case-insensitively and
/// quote-insensitively — `"Foo"` and `foo` name the same CTE.
fn ident(raw: &str) -> String {
    let t = raw.trim();
    let unquoted = if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1].replace("\"\"", "\"")
    } else {
        t.to_string()
    };
    unquoted.to_lowercase()
}

/// Strip one layer of single-quoting from a `StringLiteral`'s source text
/// and lowercase it, undoing `''` escaping — the string-content analog of
/// [`ident`]. Used by MOD016's mandatory string-reference guard: `'cities'`
/// in `stack('cities', ...)` must compare equal to the CTE name `cities`.
fn string_literal_value(raw: &str) -> String {
    let t = raw.trim();
    let unquoted = if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        t[1..t.len() - 1].replace("''", "'")
    } else {
        t.to_string()
    };
    unquoted.to_lowercase()
}

/// Walk the single-child expression-wrapper chain from `n` down to the
/// nearest descendant produced by `rule`, or `None` if the chain forks (more
/// than one child at some level before `rule` is reached) or `rule` is never
/// reached. The generic form of [`reduces_to_star`] below, used by MOD014
/// and MOD015 to see through the ~25 levels of precedence-climbing
/// expression wrappers (`LambdaArrowExpression`, `LogicalOrExpression`, ...)
/// between an `Expression` node and whatever it actually reduces to.
fn reduce_to(tree: &Tree, mut n: NodeId, rule: &str) -> Option<NodeId> {
    loop {
        if tree.rule_name(n) == rule {
            return Some(n);
        }
        let children = tree.children(n);
        if children.len() != 1 {
            return None;
        }
        n = children[0];
    }
}

/// Position-stripped structural equality over two CST subtrees: same rule
/// name at every corresponding node, same child count, and — at the leaves,
/// where a node carries no children of its own — the same source text. This
/// never compares byte spans, only shape and literal content, so two
/// occurrences of the same subject expression at different source offsets
/// (`status` in one `WHEN`, `status` in another) compare equal while two
/// different subjects (`status` vs `region`) do not. Spans are ignored
/// because a handwritten repeated subject sits at a different byte offset
/// each time.
fn structurally_equal(tree: &Tree, a: NodeId, b: NodeId, src: &str) -> bool {
    if tree.rule_name(a) != tree.rule_name(b) {
        return false;
    }
    let ca = tree.children(a);
    let cb = tree.children(b);
    if ca.len() != cb.len() {
        return false;
    }
    if ca.is_empty() {
        return tree.text(a, src) == tree.text(b, src);
    }
    ca.iter()
        .zip(cb.iter())
        .all(|(&x, &y)| structurally_equal(tree, x, y, src))
}

/// Vendored built-in aggregate function names (one lowercase name per line),
/// checked in at `vendor/aggregates.list` and regenerated from a DuckDB
/// build (the query is in `vendor/SNAPSHOT`) — same posture as
/// `grebe-syntax/vendor/grammar`. grebe does not embed the DuckDB engine,
/// so a live `duckdb_functions()` query is not available; this is the
/// static replacement. Matched case-insensitively against
/// `FunctionIdentifier` text by MOD014.
static AGGREGATE_FUNCTIONS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    include_str!("../vendor/aggregates.list")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect()
});

/// Node names that introduce a nested query scope. A "top-level only, no
/// subquery recursion" walk must stop at these rather than cross into them —
/// otherwise it would find a subquery's or CTE's own clause instead of (or
/// in addition to) the enclosing statement's. Used by MOD017, which looks at
/// the top-level `SELECT` only.
const SCOPE_BOUNDARIES: &[&str] = &[
    "SubqueryReference",
    "InSelectStatement",
    "CTESelectBody",
    "SelectParens",
];

/// Every node produced by `rule` reachable from `root` without crossing a
/// [`SCOPE_BOUNDARIES`] node — i.e. belonging to `root`'s own query, not a
/// nested one. Pre-order, so the first element is the first such node in
/// source order.
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
/// some level, meaning `n` is not itself a star — e.g. `count(*)`, whose
/// `StarExpression` sits inside a `FunctionExpression`'s argument list, not
/// at the head of the target-list item's own expression chain).
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

/// If `target_list`'s only item reduces to a star expression, that
/// `StarExpression` node. `None` if there's more than one item, or the sole
/// item isn't a star.
fn sole_star(tree: &Tree, target_list: NodeId) -> Option<NodeId> {
    let items: Vec<NodeId> = tree
        .children(target_list)
        .iter()
        .copied()
        .filter(|&c| tree.rule_name(c) == "AliasedExpression")
        .collect();
    let [item] = items[..] else { return None };
    reduces_to_star(tree, item)
}

/// True if `star` carries no qualifier (`t.*`) and no `EXCLUDE`/`REPLACE`/
/// `RENAME` narrowing — the shape MOD001, MOD017, and MOD022 all guard on.
fn star_not_narrowed(tree: &Tree, star: NodeId) -> bool {
    !tree.children(star).iter().any(|&c| {
        matches!(
            tree.rule_name(c),
            "StarQualifierList" | "ExcludeList" | "ReplaceList" | "RenameList"
        )
    })
}

/// MOD018 constraint-load-cost: `PRIMARY KEY` / `UNIQUE` / `FOREIGN KEY` /
/// `REFERENCES` in a `CREATE TABLE` cost 2-4x on bulk loads for no
/// query-time benefit (DuckDB's perf guide); add them after loading instead.
/// No fix: dropping a constraint has load-ordering and integrity consequences
/// that the SQL text alone cannot decide.
///
/// The statement head is matched on the grammar's own `CreateTableStmt`
/// production — `OR REPLACE`/`IF NOT EXISTS` sit under
/// `CreateStatement`/`CreateTableStmt` regardless, so `CREATE OR REPLACE
/// TABLE` matches exactly like `CREATE TABLE`.
///
/// Both the per-column (`ColumnConstraint` wrapping `PrimaryKeyConstraint` /
/// `ForeignKeyConstraint` / `UniqueConstraint`) and table-level
/// (`TopLevelConstraintList` wrapping the `Top*Constraint` variants) forms
/// are checked. First hit only, in source order: at most one finding per
/// `CREATE TABLE` statement, not one per constraint.
pub fn constraint_load_cost(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for stmt in tree.find("CreateTableStmt") {
        let hit = tree.descendants(stmt).into_iter().find(|&n| {
            matches!(
                tree.rule_name(n),
                "PrimaryKeyConstraint"
                    | "TopPrimaryKeyConstraint"
                    | "ForeignKeyConstraint"
                    | "TopForeignKeyConstraint"
                    | "UniqueConstraint"
                    | "TopUniqueConstraint"
            )
        });
        if let Some(n) = hit {
            out.push(Finding {
                code: "MOD018",
                span: tree.node(n).span,
                fix: None,
            });
        }
    }
    out
}

/// MOD022 from-first: `SELECT * FROM t ...` can be written `FROM t ...` —
/// DuckDB's FROM-first syntax makes the `SELECT *` redundant.
///
/// `SelectFrom <- SelectFromClause / FromSelectClause`: only the
/// `SelectFromClause` alternative means the source was written SELECT-first
/// (a `FromSelectClause` is already FROM-first — `FROM t` with no `SELECT`
/// segment at all — and has nothing to rewrite). `FromClause` must be
/// present (a bare `SELECT *` with no `FROM` parses but is semantically
/// moot — nothing to rewrite it into). The star must be *unconditionally*
/// bare: no qualifier and no `EXCLUDE`/`REPLACE`/`RENAME` (the same guard
/// as MOD017), since the rewrite target `FROM t ...` can't carry any of
/// those.
pub fn from_first(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for sfc in tree.find("SelectFromClause") {
        let Some(select_clause) = child(tree, sfc, "SelectClause") else {
            continue;
        };
        if child(tree, sfc, "FromClause").is_none() {
            continue;
        }
        let Some(target_list) = child(tree, select_clause, "TargetList") else {
            continue;
        };
        let Some(star) = sole_star(tree, target_list) else {
            continue;
        };
        if !star_not_narrowed(tree, star) {
            continue;
        }
        if tree
            .children(star)
            .iter()
            .any(|&c| tree.rule_name(c) == "RenameList")
        {
            continue;
        }
        let sp = tree.node(select_clause).span;
        out.push(Finding {
            code: "MOD022",
            span: sp,
            // Safe: FROM-first `FROM t ...` is defined by DuckDB to mean
            // exactly `SELECT * FROM t ...`. Delete `SelectClause`'s byte
            // span entirely; the formatter cleans up surrounding whitespace.
            fix: Some(Fix {
                span: sp,
                replacement: String::new(),
            }),
        });
    }
    out
}

/// MOD017 select-star-ctas: a bare `SELECT *` feeding a `CREATE TABLE ... AS
/// SELECT` or `INSERT ... SELECT` materializes every source column into the
/// target, which may not need them all.
///
/// Two statement-head shapes: `CreateTableStmt` whose `CreateTableDefinition`
/// is a `CreateTableAs` body, or `InsertStatement` whose `InsertValues` is a
/// `SelectInsertValues` (as opposed to `DefaultValues` for `INSERT ...
/// VALUES`). From there, only the **top-level** `SelectClause` is checked —
/// `top_level_find` stops at subquery/CTE/set-op-arm boundaries, so a CTE
/// body's own unguarded `SELECT *` that is later narrowed before reaching
/// the CTAS target does not fire: only the top-level select list shapes
/// the table being built.
///
/// `EXCLUDE`/`REPLACE`/`RENAME` all disqualify: they are the acceptable way
/// to shape the table being built, so only a bare star fires.
pub fn select_star_ctas(tree: &Tree, _src: &str) -> Vec<Finding> {
    let mut out = Vec::new();

    for stmt in tree.find("CreateTableStmt") {
        let Some(def) = child(tree, stmt, "CreateTableDefinition") else {
            continue;
        };
        let Some(as_body) = child(tree, def, "CreateTableAs") else {
            continue;
        };
        out.extend(check_top_level_star(tree, as_body));
    }

    for stmt in tree.find("InsertStatement") {
        let Some(values) = child(tree, stmt, "InsertValues") else {
            continue;
        };
        let Some(select_values) = child(tree, values, "SelectInsertValues") else {
            continue;
        };
        // `SelectInsertValues` wraps `DefaultValues` too (`INSERT ...
        // VALUES`); only a real `SELECT` body has anything to check —
        // `check_top_level_star` simply finds nothing for `DEFAULT VALUES`
        // or a `VALUES (...)` list, since neither contains a `SelectClause`.
        out.extend(check_top_level_star(tree, select_values));
    }

    out
}

/// The first top-level (non-nested) `SelectClause` under `root`, if its
/// target list is a bare star.
fn check_top_level_star(tree: &Tree, root: NodeId) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(&select_clause) = top_level_find(tree, root, "SelectClause").first() {
        if let Some(target_list) = child(tree, select_clause, "TargetList") {
            if let Some(star) = sole_star(tree, target_list) {
                if star_not_narrowed(tree, star) {
                    out.push(Finding {
                        code: "MOD017",
                        span: tree.node(select_clause).span,
                        // No fix: naming the actual needed columns requires
                        // the catalog, out of scope for a CST-only rule.
                        fix: None,
                    });
                }
            }
        }
    }
    out
}

/// MOD010 implicit-cross-join: a comma-separated `FROM` list (`FROM a, b`)
/// is a legal but easy-to-miss cross join; suggest an explicit `JOIN ... ON`
/// or `CROSS JOIN`.
///
/// `FromClause <- 'FROM' List(TableRef)`: the grammar's `List(D)` production
/// already delineates items, so a `FromClause` with more than one direct
/// `TableRef` child *is* this pattern — no depth tracking needed. A comma
/// inside a table function's own argument list (`FROM foo(1, 2)`) belongs to
/// a different, nested `List(FunctionArgument)` production entirely and is
/// never a direct child of `FromClause`, so it can't reach this check.
pub fn implicit_cross_join(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for clause in tree.find("FromClause") {
        let refs: Vec<NodeId> = tree
            .children(clause)
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "TableRef")
            .collect();
        if refs.len() < 2 {
            continue;
        }
        // Fire once, at the comma between the first and second `TableRef`.
        let first_end = tree.node(refs[0]).span.end as usize;
        let second_start = tree.node(refs[1]).span.start as usize;
        let between = &src[first_end..second_start];
        let comma_offset = between.find(',').unwrap_or(0) as u32;
        let start = first_end as u32 + comma_offset;
        out.push(Finding {
            code: "MOD010",
            span: Span::new(start, start + 1),
            // No fix: rewriting into `JOIN ... ON` requires inferring join
            // keys, which is not mechanical.
            fix: None,
        });
    }
    out
}

/// If `condition` (a `CaseWhenThen`'s `WHEN`-condition `Expression`) reduces
/// to a bare equality comparison — `lhs = rhs` or `lhs == rhs`, and nothing
/// more, no `AND`/`OR`/`IS`/`BETWEEN` wrapping it and no second chained
/// operator — the comparison's left- and right-hand operand nodes.
///
/// This grammar does *not* route a simple `a = b` through
/// `ComparisonExpression`'s own `ComparisonExpressionTail` /
/// `ComparisonOperator` / `OperatorEqual` production, as `grebe tree`
/// shows. `BetweenInLikeExpression` (matched before `ComparisonExpression`'s
/// own tail is ever tried) already consumes `= rhs` on the way down, as an
/// `OtherOperatorExpression`'s `OtherOperatorTail` — `OtherOperator` here
/// matches generically (`NamedOtherOperator` / `OperatorLiteral`), the same
/// path `<`, `!=`, `<=`, etc. take. So this checks the real shape: exactly
/// one `OtherOperatorTail` off `OtherOperatorExpression`, whose operator
/// text is literally `=` or `==` (DuckDB's own alias, `OperatorEqual <- '='
/// / '=='` — treated as the same equality this rule targets).
fn as_equality(tree: &Tree, condition: NodeId, src: &str) -> Option<(NodeId, NodeId)> {
    let other_op_expr = reduce_to(tree, condition, "OtherOperatorExpression")?;
    let children = tree.children(other_op_expr);
    let [lhs, tail] = children[..] else {
        return None;
    };
    if tree.rule_name(tail) != "OtherOperatorTail" {
        return None;
    }
    let tail_children = tree.children(tail);
    let [op, rhs] = tail_children[..] else {
        return None;
    };
    let op_leaf = reduce_to(tree, op, "OperatorLiteral")?;
    let op_text = tree.text(op_leaf, src);
    if op_text != "=" && op_text != "==" {
        return None;
    }
    Some((lhs, rhs))
}

/// MOD015 case-to-switch: a same-subject `CASE WHEN e = v1 THEN ... WHEN e =
/// v2 THEN ... END` chain is what DuckDB's `CASE <expr> WHEN <value> ...`
/// switch form says more compactly.
///
/// `CaseExpression <- 'CASE' Expression? CaseWhenThen+ CaseElse? 'END'`: the
/// optional leading `Expression?` *is* the switch-form subject slot, so its
/// presence is a direct structural fact — a `CaseExpression` whose first
/// child is that `Expression` is already switch-form and is skipped, not
/// rewritten. Only a concrete syntax tree keeps this distinction: DuckDB's
/// own serialized AST (`json_serialize_sql`) desugars switch form into when
/// form.
///
/// Within a when-form `CaseExpression`: require at least two `CaseWhenThen`s
/// (nothing to compact otherwise), every condition a bare equality
/// (`as_equality`), and every equality's left-hand operand structurally
/// identical to the first's (`structurally_equal`) — the correctness-
/// critical check, since two different subjects sharing incidental
/// structure must never collapse into one switch form.
pub fn case_to_switch(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for case_expr in tree.find("CaseExpression") {
        let children = tree.children(case_expr);
        if children.first().map(|&c| tree.rule_name(c)) == Some("Expression") {
            continue; // already switch-form; nothing to rewrite
        }
        let whens: Vec<NodeId> = children
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "CaseWhenThen")
            .collect();
        if whens.len() < 2 {
            continue;
        }
        let mut parts: Vec<(NodeId, NodeId, NodeId)> = Vec::with_capacity(whens.len());
        let mut all_equalities = true;
        for &w in &whens {
            let wc = tree.children(w);
            let [condition, then_expr] = wc[..] else {
                all_equalities = false;
                break;
            };
            let Some((lhs, rhs)) = as_equality(tree, condition, src) else {
                all_equalities = false;
                break;
            };
            parts.push((lhs, rhs, then_expr));
        }
        if !all_equalities {
            continue;
        }
        let subject = parts[0].0;
        if !parts
            .iter()
            .all(|&(lhs, _, _)| structurally_equal(tree, subject, lhs, src))
        {
            continue;
        }

        // Safe: DuckDB's switch form is defined as sugar for exactly this
        // equality chain. A single contiguous replacement of the whole
        // `CaseExpression` achieves the same result as the compound edit
        // (insert the subject after `CASE`; strip `<subject> =` from each
        // condition) without needing a multi-span `Fix` — `Finding` carries
        // only one.
        let mut replacement = String::from("CASE ");
        replacement.push_str(tree.text(subject, src));
        for &(_, rhs, then_expr) in &parts {
            replacement.push_str(" WHEN ");
            replacement.push_str(tree.text(rhs, src));
            replacement.push_str(" THEN ");
            replacement.push_str(tree.text(then_expr, src));
        }
        if let Some(&else_node) = children.iter().find(|&&c| tree.rule_name(c) == "CaseElse") {
            if let Some(else_expr) = child(tree, else_node, "Expression") {
                replacement.push_str(" ELSE ");
                replacement.push_str(tree.text(else_expr, src));
            }
        }
        replacement.push_str(" END");

        let span = tree.node(case_expr).span;
        out.push(Finding {
            code: "MOD015",
            span,
            fix: Some(Fix { span, replacement }),
        });
    }
    out
}

/// MOD016 unused-cte: a CTE defined in a `WITH` clause and never referenced
/// anywhere in the statement.
///
/// `WithClause <- 'WITH' Recursive? List(WithStatement)`, each `WithStatement
/// <- ColIdOrString InsertColumnList? UsingKey? 'AS' Materialized? CTEBody`.
/// `WithClause` is always a direct child of `SelectStatementInternal`
/// (`SelectStatementInternal <- WithClause? SelectSetOpChain
/// ResultModifiers?`), so `tree.parent` reaches exactly "the entire
/// statement": walking every descendant of that `SelectStatementInternal`
/// (not just the main body)
/// reaches every CTE body too, so a later CTE referencing an earlier one, or
/// a `RECURSIVE` CTE referencing itself inside its own body, is picked up as
/// a real use by the same walk — deliberately not special-cased out.
///
/// A name counts as referenced if it matches, case- and quote-insensitively
/// (`ident`), any `TableName` text anywhere in that walk, *or* — a
/// required guard, not a defensive one — any
/// `StringLiteral` text (`string_literal_value`), since a table-producing
/// macro like `stack('cities', ...)` references a CTE by
/// name as a string argument, not as a `TableRef`.
pub fn unused_cte(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for with_clause in tree.find("WithClause") {
        let Some(stmt_root) = tree.parent(with_clause) else {
            continue;
        };
        let scope = tree.descendants(stmt_root);
        let table_refs: HashSet<String> = scope
            .iter()
            .copied()
            .filter(|&n| tree.rule_name(n) == "TableName")
            .map(|n| ident(tree.text(n, src)))
            .collect();
        let string_refs: HashSet<String> = scope
            .iter()
            .copied()
            .filter(|&n| tree.rule_name(n) == "StringLiteral")
            .map(|n| string_literal_value(tree.text(n, src)))
            .collect();

        let entries: Vec<NodeId> = tree
            .children(with_clause)
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "WithStatement")
            .collect();

        for (idx, &entry) in entries.iter().enumerate() {
            let Some(name_node) = child(tree, entry, "ColIdOrString") else {
                continue;
            };
            let name = ident(tree.text(name_node, src));
            if table_refs.contains(&name) || string_refs.contains(&name) {
                continue;
            }

            let entry_span = tree.node(entry).span;
            // Safe: delete the `WithStatement` entry plus one comma. Only
            // entry -> the whole `WithClause` goes; first of several -> the
            // entry plus the comma that follows; middle or last -> the comma
            // that precedes plus the entry (for a middle entry either comma
            // would do; the preceding one is used consistently).
            let fix_span = if entries.len() == 1 {
                tree.node(with_clause).span
            } else if idx == 0 {
                let next_start = tree.node(entries[1]).span.start as usize;
                let between = &src[entry_span.end as usize..next_start];
                let comma_offset = between.find(',').unwrap_or(0) as u32;
                Span::new(entry_span.start, entry_span.end + comma_offset + 1)
            } else {
                let prev_end = tree.node(entries[idx - 1]).span.end as usize;
                let between = &src[prev_end..entry_span.start as usize];
                let comma_offset = between.find(',').unwrap_or(0) as u32;
                Span::new(prev_end as u32 + comma_offset, entry_span.end)
            };

            out.push(Finding {
                code: "MOD016",
                span: tree.node(name_node).span,
                fix: Some(Fix {
                    span: fix_span,
                    replacement: String::new(),
                }),
            });
        }
    }
    out
}

/// MOD014 case-to-filter: `agg(CASE WHEN c THEN x [ELSE k] END)` — an
/// aggregate over a single-branch CASE — can be `agg(x) FILTER (WHERE c)`.
///
/// Walk every `FunctionExpression` whose `FunctionIdentifier` text is in
/// `AGGREGATE_FUNCTIONS` (case-insensitively). Require
/// `FunctionExpressionArgumentList` to reduce to exactly one
/// `FunctionArgument`, a `PositionalFunctionArgument` (excludes
/// `NamedFunctionArgument` — a named `=>` argument isn't a positional CASE),
/// whose `Expression` reduces (`reduce_to`) to a `CaseExpression`. Require
/// that `CaseExpression`'s `Expression?` subject slot to be absent — per
/// `CaseExpression <- 'CASE' Expression? CaseWhenThen+ CaseElse? 'END'`, a
/// switch-form single-branch CASE inside an aggregate is the different,
/// rarer shape, explicitly excluded rather than covered. Require exactly one
/// `CaseWhenThen`, and `CaseElse` either absent or reducing to
/// `LiteralExpression` (`StringLiteral` / `NumberLiteral` /
/// `ConstantLiteral` — i.e. `NullLiteral` / `TrueLiteral` / `FalseLiteral`,
/// per `grebe-syntax/vendor/grammar/statements/expression.gram`), which
/// covers the "constant" requirement in one reduction.
///
/// The remaining guards keep the fix mechanical and never wrong rather than
/// narrowing the rule's scope: skip if the call already carries a
/// `FilterClause` (a second one would be
/// invalid SQL, not just redundant) or a `DistinctOrAll` / `OrderByClause` /
/// `IgnoreOrRespectNulls` modifier, or a `WithinGroupClause` / `ExportClause`
/// / `OverClause` (all optional siblings of `FunctionExpressionArguments`
/// per `FunctionExpression <- FunctionIdentifier FunctionExpressionArguments
/// WithinGroupClause? FilterClause? ExportClause? OverClause?`) — none of
/// these compose with the plain `agg(x) FILTER (WHERE c)` rewrite text.
pub fn case_to_filter(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for func in tree.find("FunctionExpression") {
        let Some(func_id) = child(tree, func, "FunctionIdentifier") else {
            continue;
        };
        let name = ident(tree.text(func_id, src));
        if !AGGREGATE_FUNCTIONS.contains(name.as_str()) {
            continue;
        }
        if tree.children(func).iter().any(|&c| {
            matches!(
                tree.rule_name(c),
                "FilterClause" | "WithinGroupClause" | "ExportClause" | "OverClause"
            )
        }) {
            continue;
        }

        let Some(args) = child(tree, func, "FunctionExpressionArguments") else {
            continue;
        };
        let Some(arg_list_wrapper) = child(tree, args, "FunctionExpressionArgumentList") else {
            continue;
        };
        if tree.children(arg_list_wrapper).iter().any(|&c| {
            matches!(
                tree.rule_name(c),
                "DistinctOrAll" | "OrderByClause" | "IgnoreOrRespectNulls"
            )
        }) {
            continue;
        }
        let Some(arg_list) = child(tree, arg_list_wrapper, "FunctionArgumentList") else {
            continue;
        };
        let args_vec: Vec<NodeId> = tree
            .children(arg_list)
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "FunctionArgument")
            .collect();
        let [only_arg] = args_vec[..] else {
            continue;
        };
        let Some(positional) = child(tree, only_arg, "PositionalFunctionArgument") else {
            continue;
        };
        let Some(expr) = child(tree, positional, "Expression") else {
            continue;
        };
        let Some(case_expr) = reduce_to(tree, expr, "CaseExpression") else {
            continue;
        };

        let case_children = tree.children(case_expr);
        if case_children.first().map(|&c| tree.rule_name(c)) == Some("Expression") {
            continue; // switch-form subject present; excluded
        }
        let whens: Vec<NodeId> = case_children
            .iter()
            .copied()
            .filter(|&c| tree.rule_name(c) == "CaseWhenThen")
            .collect();
        let [when_then] = whens[..] else {
            continue;
        };
        let wc = tree.children(when_then);
        let [condition, then_expr] = wc[..] else {
            continue;
        };
        let mut else_literal = None;
        if let Some(&else_node) = case_children
            .iter()
            .find(|&&c| tree.rule_name(c) == "CaseElse")
        {
            let Some(else_expr) = child(tree, else_node, "Expression") else {
                continue;
            };
            let Some(literal) = reduce_to(tree, else_expr, "LiteralExpression") else {
                continue;
            };
            else_literal = Some(literal);
        }

        // `sum(CASE WHEN c THEN 1 ELSE 0 END)` has an exact equivalent, so it
        // gets its own rule with a safe fix rather than MOD014's unsafe one.
        let is_literal = |n: NodeId, text: &str| {
            reduce_to(tree, n, "LiteralExpression").is_some() && tree.text(n, src).trim() == text
        };
        if name == "sum"
            && is_literal(then_expr, "1")
            && else_literal.is_some_and(|e| tree.text(e, src).trim() == "0")
        {
            let func_span = tree.node(func).span;
            out.push(Finding {
                code: "MOD030",
                span: func_span,
                fix: Some(Fix {
                    span: func_span,
                    replacement: format!("count_if({})", tree.text(condition, src).trim()),
                }),
            });
            continue;
        }

        // Unsafe (registry `fix_safety = Unsafe`): the rewrite drops the ELSE
        // value. `sum(CASE WHEN c THEN x ELSE 0 END)` returns `0` for a group
        // with no matching rows, but `sum(x) FILTER (WHERE c)` returns `NULL`
        // — a different result, not just a different spelling, whenever ELSE
        // supplies a non-NULL default. Still worth offering, just never as
        // an edit plain `--fix` applies.
        let func_span = tree.node(func).span;
        let replacement = format!(
            "{}({}) FILTER (WHERE {})",
            tree.text(func_id, src),
            tree.text(then_expr, src),
            tree.text(condition, src),
        );
        out.push(Finding {
            code: "MOD014",
            span: func_span,
            fix: Some(Fix {
                span: func_span,
                replacement,
            }),
        });
    }
    out
}

/// Every tier-2b detector; findings are sorted by position, then code.
pub fn analyze_tier2b(tree: &Tree, src: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(constraint_load_cost(tree, src));
    out.extend(from_first(tree, src));
    out.extend(select_star_ctas(tree, src));
    out.extend(implicit_cross_join(tree, src));
    out.extend(case_to_filter(tree, src));
    out.extend(case_to_switch(tree, src));
    out.extend(unused_cte(tree, src));
    out.sort_by_key(|f| (f.span.start, f.code));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use grebe_syntax::matcher::parse;

    fn codes(sql: &str) -> Vec<&'static str> {
        let tree = parse(sql).unwrap_or_else(|| panic!("did not parse: {sql}"));
        analyze_tier2b(&tree, sql)
            .into_iter()
            .map(|f| f.code)
            .collect()
    }

    fn fires(code: &str, sql: &str) -> bool {
        codes(sql).contains(&code)
    }

    #[test]
    fn mod018_constraint_load_cost() {
        for sql in [
            "CREATE TABLE t (id INT PRIMARY KEY)",
            "CREATE TABLE t (id INT, UNIQUE(id))",
            "CREATE TABLE t (fk INT REFERENCES other(id))",
        ] {
            assert!(fires("MOD018", sql), "should fire: {sql}");
        }
        for sql in [
            "CREATE VIEW v AS SELECT 1",
            "CREATE INDEX idx ON t(a)",
            "ALTER TABLE t ADD CONSTRAINT pk PRIMARY KEY (id)",
            "CREATE TABLE t (a INT)",
        ] {
            assert!(!fires("MOD018", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod018_fires_with_or_replace_and_if_not_exists() {
        // Modifiers between `CREATE` and the table name do not hide it.
        assert!(fires(
            "MOD018",
            "CREATE OR REPLACE TABLE t (id INT PRIMARY KEY)"
        ));
        assert!(fires(
            "MOD018",
            "CREATE TABLE IF NOT EXISTS t (id INT PRIMARY KEY)"
        ));
    }

    #[test]
    fn mod022_from_first() {
        for sql in [
            "SELECT * FROM t",
            "SELECT * FROM t WHERE x > 1",
            "SELECT * FROM t, u",
        ] {
            assert!(fires("MOD022", sql), "should fire: {sql}");
        }
        for sql in [
            "SELECT a, b FROM t",
            "FROM t",
            "SELECT * EXCLUDE (a) FROM t",
            "SELECT * RENAME (a AS b) FROM t",
        ] {
            assert!(!fires("MOD022", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod022_fix_is_minimal_and_correct() {
        let sql = "SELECT * FROM t WHERE x > 1";
        let tree = parse(sql).unwrap();
        let f = analyze_tier2b(&tree, sql)
            .into_iter()
            .find(|f| f.code == "MOD022")
            .unwrap();
        let fix = f.fix.clone().expect("MOD022 has a safe fix");
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert_eq!(fixed, " FROM t WHERE x > 1");
        assert!(parse(&fixed).is_some(), "fixed SQL must still parse");
        assert!(!codes(&fixed).contains(&"MOD022"), "fix is not idempotent");
    }

    #[test]
    fn mod017_select_star_ctas() {
        for sql in [
            "CREATE TABLE t AS SELECT * FROM 'file.csv'",
            "INSERT INTO t SELECT * FROM src",
        ] {
            assert!(fires("MOD017", sql), "should fire: {sql}");
        }
        for sql in [
            "CREATE TABLE t AS SELECT * EXCLUDE (junk_col) FROM src",
            "CREATE TABLE t AS SELECT * REPLACE (a + 1 AS a) FROM src",
            "CREATE TABLE t (a INT)",
            "INSERT INTO t VALUES (1, 2)",
        ] {
            assert!(!fires("MOD017", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod017_rename_is_guarded_like_exclude_and_replace() {
        // All three star modifiers are the acceptable way to shape the
        // table being built, so only a bare star fires.
        for sql in [
            "CREATE TABLE t AS SELECT * RENAME (a AS b) FROM src",
            "CREATE TABLE t AS SELECT * EXCLUDE (a) FROM src",
            "CREATE TABLE t AS SELECT * REPLACE (upper(a) AS a) FROM src",
            "CREATE TABLE t AS SELECT * EXCLUDE (a) RENAME (b AS c) FROM src",
        ] {
            assert!(!fires("MOD017", sql), "should NOT fire: {sql}");
        }
        // A bare star in a CTAS still does.
        assert!(fires("MOD017", "CREATE TABLE t AS SELECT * FROM src"));
    }

    #[test]
    fn mod017_top_level_only_no_subquery_recursion() {
        // The CTE body's own `SELECT *` is never narrowed before reaching
        // the CTAS target -- but MOD017 only looks at the top-level clause,
        // so it must not fire here.
        assert!(!fires(
            "MOD017",
            "CREATE TABLE t AS WITH c AS (SELECT * FROM inner_t) SELECT a FROM c"
        ));
        // The outer clause IS the top-level one and IS a bare star, so this
        // must fire.
        assert!(fires(
            "MOD017",
            "CREATE TABLE t AS SELECT * FROM (SELECT * FROM inner_t) x"
        ));
    }

    #[test]
    fn mod010_implicit_cross_join() {
        for sql in ["SELECT * FROM a, b", "SELECT * FROM a, b, c"] {
            assert!(fires("MOD010", sql), "should fire: {sql}");
        }
        for sql in [
            "SELECT * FROM foo(1, 2)",
            "SELECT * FROM a JOIN b ON a.i = b.i",
        ] {
            assert!(!fires("MOD010", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod010_fires_once_at_first_comma() {
        let sql = "SELECT * FROM a, b, c";
        let hits: Vec<_> = codes(sql).into_iter().filter(|&c| c == "MOD010").collect();
        assert_eq!(
            hits.len(),
            1,
            "fires once per FROM clause, not once per comma"
        );
    }

    /// Apply `code`'s (sole, first) fix to `sql` and return the result,
    /// asserting the fixed text still parses. Mirrors
    /// `mod022_fix_is_minimal_and_correct`'s pattern.
    fn apply_fix(code: &str, sql: &str) -> String {
        let tree = parse(sql).unwrap();
        let f = analyze_tier2b(&tree, sql)
            .into_iter()
            .find(|f| f.code == code)
            .unwrap_or_else(|| panic!("{code} did not fire on: {sql}"));
        let fix = f.fix.clone().unwrap_or_else(|| panic!("{code} has no fix"));
        let mut fixed = sql.to_string();
        fixed.replace_range(
            fix.span.start as usize..fix.span.end as usize,
            &fix.replacement,
        );
        assert!(
            parse(&fixed).is_some(),
            "fixed SQL must still parse: {fixed:?}"
        );
        fixed
    }

    #[test]
    fn mod014_case_to_filter() {
        for sql in [
            "SELECT sum(CASE WHEN status = 'shipped' THEN quantity END) FROM t",
            "SELECT count(CASE WHEN active THEN 1 ELSE 0 END) FROM t",
            "SELECT SUM(CASE WHEN active THEN 1 END) FROM t", // case-insensitive fn name
        ] {
            assert!(fires("MOD014", sql), "should fire: {sql}");
        }
        for sql in [
            // more than one WHEN
            "SELECT sum(CASE WHEN a THEN x WHEN b THEN y END) FROM t",
            // non-constant ELSE
            "SELECT sum(CASE WHEN a THEN x ELSE other_col END) FROM t",
            // wrapping function is not an aggregate
            "SELECT upper(CASE WHEN a THEN 'x' END) FROM t",
            // more than one argument to the aggregate
            "SELECT sum(CASE WHEN a THEN x END, 5) FROM t",
            // no CASE at all
            "SELECT sum(x) FROM t",
            // already switch-form single-branch CASE: excluded, not extended to
            "SELECT sum(CASE a WHEN 1 THEN x END) FROM t",
        ] {
            assert!(!fires("MOD014", sql), "should NOT fire: {sql}");
        }
    }

    fn fix_text(code: &str, sql: &str) -> Option<String> {
        let tree = parse(sql).unwrap_or_else(|| panic!("did not parse: {sql}"));
        analyze_tier2b(&tree, sql)
            .into_iter()
            .find(|f| f.code == code)
            .and_then(|f| f.fix)
            .map(|fix| fix.replacement)
    }

    #[test]
    fn mod030_sum_case_to_count_if() {
        let sql = "SELECT sum(CASE WHEN status = 'shipped' THEN 1 ELSE 0 END) FROM t";
        assert_eq!(codes(sql), ["MOD030"], "carved out of MOD014, never both");
        assert_eq!(
            fix_text("MOD030", sql).as_deref(),
            Some("count_if(status = 'shipped')")
        );
        assert!(fires(
            "MOD030",
            "SELECT SUM(CASE WHEN a THEN 1 ELSE 0 END) FROM t"
        ));

        // Each of these changes the value or the type, so it stays MOD014.
        for sql in [
            "SELECT sum(CASE WHEN a THEN 1 END) FROM t", // NULL, not 0, when nothing matches
            "SELECT sum(CASE WHEN a THEN 1.0 ELSE 0 END) FROM t", // DECIMAL sum
            "SELECT sum(CASE WHEN a THEN 2 ELSE 0 END) FROM t",
            "SELECT sum(CASE WHEN a THEN 1 ELSE 1 END) FROM t",
            "SELECT count(CASE WHEN a THEN 1 ELSE 0 END) FROM t", // counts every row
        ] {
            assert!(!fires("MOD030", sql), "should NOT fire: {sql}");
            assert!(fires("MOD014", sql), "should still be MOD014: {sql}");
        }
    }

    #[test]
    fn mod014_else_constant_variants_all_fire() {
        for sql in [
            "SELECT sum(CASE WHEN a THEN x ELSE 0 END) FROM t",
            "SELECT sum(CASE WHEN a THEN x ELSE NULL END) FROM t",
            "SELECT count(CASE WHEN a THEN x ELSE TRUE END) FROM t",
            "SELECT count(CASE WHEN a THEN x ELSE 'k' END) FROM t",
        ] {
            assert!(fires("MOD014", sql), "should fire: {sql}");
        }
    }

    #[test]
    fn mod014_already_filtered_or_windowed_not_reguarded() {
        // A second FILTER, or composing with OVER, is not the plain
        // `agg(x) FILTER (WHERE c)` shape the fix text assumes -- guarded
        // defensively.
        for sql in [
            "SELECT sum(CASE WHEN a THEN x END) FILTER (WHERE y) FROM t",
            "SELECT sum(CASE WHEN a THEN x END) OVER () FROM t",
        ] {
            assert!(!fires("MOD014", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod014_fix_is_unsafe_but_present_and_correct() {
        let fixed = apply_fix(
            "MOD014",
            "SELECT sum(CASE WHEN status = 'shipped' THEN quantity END) FROM t",
        );
        assert_eq!(
            fixed,
            "SELECT sum(quantity) FILTER (WHERE status = 'shipped') FROM t"
        );
        // Registry marks MOD014 fix_safety = Unsafe; this test only checks
        // the fix text is offered and correct, not that it is auto-applied.
    }

    #[test]
    fn mod015_case_to_switch() {
        for sql in [
            "SELECT CASE WHEN status = 1 THEN 'a' WHEN status = 2 THEN 'b' END FROM t",
            "SELECT CASE WHEN status = 1 THEN 'a' WHEN status = 2 THEN 'b' ELSE 'c' END FROM t",
        ] {
            assert!(fires("MOD015", sql), "should fire: {sql}");
        }
        for sql in [
            // already switch-form -- the critical guard
            "SELECT CASE status WHEN 1 THEN 'a' WHEN 2 THEN 'b' END FROM t",
            // different subject per WHEN
            "SELECT CASE WHEN status = 1 THEN 'a' WHEN region = 2 THEN 'b' END FROM t",
            // non-equality comparison
            "SELECT CASE WHEN status > 1 THEN 'a' WHEN status > 2 THEN 'b' END FROM t",
            // single WHEN -- nothing to compact
            "SELECT CASE WHEN status = 1 THEN 'a' END FROM t",
            // one WHEN is not even a comparison
            "SELECT CASE WHEN status = 1 THEN 'a' WHEN other_flag THEN 'b' END FROM t",
        ] {
            assert!(!fires("MOD015", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod015_subject_must_match_structurally_not_textually() {
        // Same rendered text, different structure (`a.x` vs bare `x`) must
        // not be treated as the same subject.
        assert!(!fires(
            "MOD015",
            "SELECT CASE WHEN a.x = 1 THEN 'a' WHEN x = 2 THEN 'b' END FROM t"
        ));
        // Byte-position differs but structure is identical -- must fire.
        assert!(fires(
            "MOD015",
            "SELECT CASE WHEN (a+b) = 1 THEN 'a' WHEN (a+b) = 2 THEN 'b' END FROM t"
        ));
    }

    #[test]
    fn mod015_fix_rewrites_to_switch_form() {
        let fixed = apply_fix(
            "MOD015",
            "SELECT CASE WHEN status = 1 THEN 'a' WHEN status = 2 THEN 'b' ELSE 'c' END FROM t",
        );
        assert_eq!(
            fixed,
            "SELECT CASE status WHEN 1 THEN 'a' WHEN 2 THEN 'b' ELSE 'c' END FROM t"
        );
        assert!(
            !codes(&fixed).contains(&"MOD015"),
            "fix is not idempotent: already switch-form"
        );
    }

    #[test]
    fn mod016_unused_cte() {
        assert!(fires(
            "MOD016",
            "WITH unused AS (SELECT 1) SELECT * FROM other_table"
        ));
        for sql in [
            // referenced as a table
            "WITH c AS (SELECT 1) SELECT * FROM c",
            // the mandatory guard: referenced by name as a string literal
            // argument to a table-producing function
            "WITH cities AS (SELECT 1 AS x) SELECT * FROM stack('cities', 1)",
            "WITH cities AS (SELECT 1 AS x) SELECT * FROM query_table('cities')",
            // RECURSIVE self-reference inside its own body counts as used
            "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n+1 FROM r WHERE n < 5) SELECT * FROM r",
        ] {
            assert!(!fires("MOD016", sql), "should NOT fire: {sql}");
        }
    }

    #[test]
    fn mod016_multi_cte_only_the_unused_one_fires() {
        // b unused among a, c which are referenced.
        let sql = "WITH a AS (SELECT 1), b AS (SELECT 2), c AS (SELECT 3) \
                    SELECT * FROM a JOIN c ON a.x = c.x";
        let hits: Vec<_> = codes(sql).into_iter().filter(|&c| c == "MOD016").collect();
        assert_eq!(hits.len(), 1, "exactly one of the three CTEs is unused");
    }

    #[test]
    fn mod016_name_shadowing_and_case_quote_insensitive() {
        // A later CTE referencing an earlier one by name is a real use.
        assert!(!fires(
            "MOD016",
            "WITH a AS (SELECT 1), b AS (SELECT * FROM a) SELECT * FROM b"
        ));
        // Case- and quote-different spellings still count as the same name.
        assert!(!fires(
            "MOD016",
            "WITH \"Foo\" AS (SELECT 1) SELECT * FROM foo"
        ));
        assert!(!fires(
            "MOD016",
            "WITH foo AS (SELECT 1) SELECT * FROM \"FOO\""
        ));
    }

    #[test]
    fn mod016_fix_deletes_only_entry_and_whole_with_clause() {
        let fixed = apply_fix(
            "MOD016",
            "WITH unused AS (SELECT 1) SELECT * FROM other_table",
        );
        assert_eq!(fixed, " SELECT * FROM other_table");
        assert!(!codes(&fixed).contains(&"MOD016"));
    }

    #[test]
    fn mod016_fix_deletes_first_entry_and_following_comma() {
        let fixed = apply_fix(
            "MOD016",
            "WITH unused AS (SELECT 1), b AS (SELECT 2) SELECT * FROM b",
        );
        // Formatter cleans up the resulting double space, same as MOD022's
        // fix (`mod022_fix_is_minimal_and_correct`) -- not this fix's job.
        assert_eq!(fixed, "WITH  b AS (SELECT 2) SELECT * FROM b");
        assert!(parse(&fixed).is_some());
    }

    #[test]
    fn mod016_fix_deletes_last_entry_and_preceding_comma() {
        let fixed = apply_fix(
            "MOD016",
            "WITH a AS (SELECT 1), unused AS (SELECT 2) SELECT * FROM a",
        );
        assert_eq!(fixed, "WITH a AS (SELECT 1) SELECT * FROM a");
        assert!(parse(&fixed).is_some());
    }

    #[test]
    fn mod016_fix_deletes_middle_entry_and_preceding_comma() {
        let fixed = apply_fix(
            "MOD016",
            "WITH a AS (SELECT 1), unused AS (SELECT 2), c AS (SELECT 3) \
             SELECT * FROM a JOIN c ON a.x = c.x",
        );
        assert_eq!(
            fixed,
            "WITH a AS (SELECT 1), c AS (SELECT 3) SELECT * FROM a JOIN c ON a.x = c.x"
        );
        assert!(parse(&fixed).is_some());
    }
}
