//! The `CAT` rules that read what a snapshot says of a table beyond its
//! column names: types, nullability, the row estimate, keys and indexes.
//!
//! - **CAT003 unbounded-scan**: a query reads every row of a large table
//!   and returns them all.
//! - **CAT004 index-defeating-filter**: a filter wraps the leading column
//!   of a key or index in a function or cast DuckDB cannot see through.
//! - **CAT005 nullable-not-in**: `NOT IN (SELECT c ...)` where `c` may be
//!   NULL. Where `c` cannot be NULL, MOD028 is withdrawn instead.
//! - **CAT006 ambiguous-column**: an unqualified column more than one table
//!   of its FROM has; DuckDB rejects the query.
//! - **CAT007 insert-mismatch**: `INSERT ... VALUES` whose rows do not fit
//!   the table or the column list.
//! - **CAT008 type-mismatch**: a comparison of two columns whose types are
//!   of different kinds (text, number, time, boolean).
//! - **CAT009 fan-out-join**: a two-table equi-join on columns that are a
//!   key of neither table.
//! - **CAT010 redundant-distinct**: `SELECT DISTINCT` over one table whose
//!   select list holds a key that cannot be NULL.
//!
//! Every rule only looks at the tables the statement names, already looked
//! up by the caller; nothing here walks the catalog. Each one is silent
//! unless every name it reasons about is resolved: a column is attributed
//! to a table only when its query's FROM is made of catalog tables alone
//! (no subquery, CTE, table function, `VALUES`, `PIVOT` or local table)
//! and exactly one of them has it. A rule that needs a row estimate, a
//! key, an index or a nullability the snapshot does not give says nothing.

use std::collections::HashMap;

use super::{
    Checked, HashSet, NodeId, Options, Override, Relation, Table, Tree, direct_child,
    first_descendant, ident, rule, tight,
};
use crate::detect::Finding;

/// What `check_statement` worked out about one statement, lent to the
/// rules here.
pub(super) struct Statement<'a> {
    pub tree: &'a Tree,
    pub src: &'a str,
    pub nodes: &'a [NodeId],
    pub tables: &'a [&'a Table],
    pub bindings: &'a [(String, Relation)],
    /// Beside `bindings`: the node that brings each relation in.
    pub binding_nodes: &'a [NodeId],
    /// Lambda parameters and select-list aliases.
    pub shadows: &'a HashSet<String>,
    /// Lambda parameters alone.
    pub lambdas: &'a HashSet<String>,
    pub options: &'a Options,
}

/// One catalog table in a query's FROM.
struct Rel<'a> {
    /// The node that brings it in (a `BaseTableRef`).
    node: NodeId,
    /// What the query calls it.
    name: String,
    table: &'a Table,
}

/// One `SimpleSelect`: the relations its own FROM brings in.
#[derive(Default)]
struct Scope<'a> {
    rels: Vec<Rel<'a>>,
    /// Every relation of the FROM is a catalog table with known columns,
    /// brought in by a plain join (or a comma).
    known: bool,
    /// A parenthesized join: an ON inside it sees only part of the FROM.
    parens: bool,
    /// `NATURAL`, `SEMI`, `ANTI`, `PIVOT`, `JOIN BY`, `NEAREST`: joins whose
    /// effect on what an unqualified name means is not modelled here.
    odd_join: bool,
    /// Names in a `USING (...)` of this FROM: DuckDB merges them, so an
    /// unqualified one is not ambiguous.
    using: HashSet<String>,
    /// How many relations the FROM has, of any kind.
    items: usize,
}

struct Cx<'a> {
    s: &'a Statement<'a>,
    scopes: HashMap<NodeId, Scope<'a>>,
}

/// Run the rules over one statement.
pub(super) fn check(s: &Statement<'_>, out: &mut Checked) {
    // Nothing to read without a catalog table that has columns.
    if s.tables.is_empty() {
        return;
    }
    let cx = Cx {
        s,
        scopes: scopes(s),
    };
    unbounded_scan(&cx, out);
    index_defeating_filter(&cx, out);
    nullable_not_in(&cx, out);
    ambiguous_column(&cx, out);
    insert_mismatch(&cx, out);
    type_mismatch(&cx, out);
    fan_out_join(&cx, out);
    redundant_distinct(&cx, out);
}

fn push(out: &mut Checked, code: &'static str, span: grebe_syntax::Span, detail: String) {
    out.findings.push(Finding {
        code,
        span,
        fix: None,
        detail: Some(detail),
    });
}

// ---------------------------------------------------------------------
// Scopes and column resolution
// ---------------------------------------------------------------------

/// The `SimpleSelect` whose FROM `n` is in: the nearest `FromClause` above
/// it, and the `SimpleSelect` above that. `None` for a DML target.
fn from_owner(tree: &Tree, n: NodeId) -> Option<NodeId> {
    let mut seen_from = false;
    let mut cur = tree.parent(n);
    while let Some(p) = cur {
        match rule(tree, p) {
            "FromClause" => seen_from = true,
            "SimpleSelect" => return seen_from.then_some(p),
            "SelectStatementInternal" | "Statement" if !seen_from => return None,
            _ => {}
        }
        cur = tree.parent(p);
    }
    None
}

/// The `SimpleSelect` an expression node belongs to. `None` when the way
/// there leaves a query (an ORDER BY or LIMIT after a set operation, a
/// `VALUES` row) or there is none (DML).
fn expr_owner(tree: &Tree, n: NodeId) -> Option<NodeId> {
    let mut cur = tree.parent(n);
    while let Some(p) = cur {
        match rule(tree, p) {
            "SimpleSelect" => return Some(p),
            "SelectStatementInternal" | "ResultModifiers" | "ValuesClause" | "Statement" => {
                return None;
            }
            _ => {}
        }
        cur = tree.parent(p);
    }
    None
}

fn scopes<'a>(s: &Statement<'a>) -> HashMap<NodeId, Scope<'a>> {
    let tree = s.tree;
    let mut out: HashMap<NodeId, Scope<'a>> = HashMap::new();
    for &n in s.nodes {
        if rule(tree, n) == "SimpleSelect" {
            out.insert(
                n,
                Scope {
                    known: true,
                    ..Scope::default()
                },
            );
        }
    }
    for &n in s.nodes {
        let r = rule(tree, n);
        let Some(owner) = (match r {
            "InnerTableRef" | "JoinClause" | "JoinType" | "JoinPrefix" | "UsingColumnName"
            | "TablePivotClause" | "TableUnpivotClause" => from_owner(tree, n),
            _ => None,
        }) else {
            continue;
        };
        let Some(scope) = out.get_mut(&owner) else {
            continue;
        };
        match r {
            "InnerTableRef" => {
                let Some(&inner) = tree.children(n).first() else {
                    scope.known = false;
                    continue;
                };
                match rule(tree, inner) {
                    "BaseTableRef" => {
                        scope.items += 1;
                        let bound = s.binding_nodes.iter().position(|&b| b == inner);
                        match bound.map(|i| &s.bindings[i]) {
                            Some((name, Relation::Table(at))) => scope.rels.push(Rel {
                                node: inner,
                                name: name.clone(),
                                table: s.tables[*at],
                            }),
                            _ => scope.known = false,
                        }
                    }
                    // A parenthesized join only groups relations, unless it
                    // renames them.
                    "ParensTableRef" => {
                        scope.parens = true;
                        if direct_child(tree, inner, "TableAlias").is_some()
                            || direct_child(tree, inner, "TableAliasColon").is_some()
                        {
                            scope.known = false;
                        }
                    }
                    _ => {
                        scope.items += 1;
                        scope.known = false;
                    }
                }
            }
            "JoinClause" => {
                if !matches!(
                    tree.children(n).first().map(|&c| rule(tree, c)),
                    Some("RegularJoinClause" | "JoinWithoutOnClause")
                ) {
                    scope.odd_join = true;
                    scope.known = false;
                }
            }
            "JoinType" => {
                if first_descendant(tree, n, &["SemiJoin", "AntiJoin"]).is_some() {
                    scope.odd_join = true;
                }
            }
            "JoinPrefix" => {
                if first_descendant(tree, n, &["NaturalJoinPrefix"]).is_some() {
                    scope.odd_join = true;
                }
            }
            "UsingColumnName" => {
                scope.using.insert(ident(tree.text(n, s.src)));
            }
            _ => {
                scope.odd_join = true;
                scope.known = false;
            }
        }
    }
    out
}

/// Does `from` reach `to` through nodes with one child each (the
/// precedence ladder of an expression), `to` itself included?
fn reduces_to(tree: &Tree, from: NodeId, to: &str) -> Option<NodeId> {
    let mut n = from;
    loop {
        if rule(tree, n) == to {
            return Some(n);
        }
        match tree.children(n) {
            [only] => n = *only,
            _ => return None,
        }
    }
}

/// The column reference `n` is, when it is one alone.
fn bare_column(tree: &Tree, n: NodeId) -> Option<NodeId> {
    reduces_to(tree, n, "ColumnReference")
}

/// An unqualified one-part column reference: its name.
fn unqualified_name(cx: &Cx<'_>, colref: NodeId) -> Option<String> {
    let tree = cx.s.tree;
    let [nested] = tree.children(colref) else {
        return None;
    };
    if rule(tree, *nested) != "NestedColumnName" {
        return None;
    }
    let [name] = tree.children(*nested) else {
        return None;
    };
    if rule(tree, *name) != "ColumnName" {
        return None;
    }
    Some(ident(tree.text(*name, cx.s.src)))
}

/// What a column reference reads, when it is certain: the table, the
/// column's name as the table spells it, and for an unqualified one the
/// scope it was found in.
struct Resolved<'a> {
    table: &'a Table,
    column: &'a str,
    /// The name the query gives the table.
    rel_name: String,
    /// The node of the relation, when it was found through a scope.
    rel_node: Option<NodeId>,
}

fn resolve<'a>(cx: &Cx<'a>, colref: NodeId) -> Option<Resolved<'a>> {
    let tree = cx.s.tree;
    if rule(tree, colref) != "ColumnReference" {
        return None;
    }
    if let Some(name) = unqualified_name(cx, colref) {
        if cx.s.lambdas.contains(&name) {
            return None;
        }
        let scope = cx.scopes.get(&expr_owner(tree, colref)?)?;
        if !scope.known || scope.odd_join || scope.using.contains(&name) {
            return None;
        }
        let mut found = scope.rels.iter().filter(|r| r.table.has_column(&name));
        let rel = found.next()?;
        if found.next().is_some() {
            return None;
        }
        return Some(Resolved {
            table: rel.table,
            column: &rel.table.column(&name)?.name,
            rel_name: rel.name.clone(),
            rel_node: Some(rel.node),
        });
    }
    // `q.c` and nothing more.
    let [form] = tree.children(colref) else {
        return None;
    };
    if rule(tree, *form) != "TableReservedColumnName" {
        return None;
    }
    let qn = first_descendant(tree, *form, &["TableName"])?;
    let cn = first_descendant(tree, *form, &["ReservedColumnName"])?;
    let q = ident(tree.text(qn, cx.s.src));
    let c = ident(tree.text(cn, cx.s.src));
    if cx.s.shadows.contains(&q) {
        return None;
    }
    // Every relation the statement calls `q` is the same catalog table, as
    // CAT002 asks; and `q` is no column, so `q.c` is no struct field.
    let mut named =
        cx.s.bindings
            .iter()
            .filter(|(name, _)| *name == q)
            .map(|(_, r)| *r);
    let Some(Relation::Table(at)) = named.next() else {
        return None;
    };
    if named.any(|r| r != Relation::Table(at)) || cx.s.tables.iter().any(|t| t.has_column(&q)) {
        return None;
    }
    let table = cx.s.tables[at];
    Some(Resolved {
        table,
        column: &table.column(&c)?.name,
        rel_name: q,
        rel_node: None,
    })
}

/// `table.column`, as the catalog spells them.
fn qualified(table: &Table, column: &str) -> String {
    format!("{}.{column}", table.name)
}

/// Is `n` free of column references and subqueries: a value fixed before
/// any row is read?
fn is_constant(tree: &Tree, n: NodeId) -> bool {
    !std::iter::once(n).chain(tree.descendants(n)).any(|d| {
        matches!(
            rule(tree, d),
            "ColumnReference" | "SelectStatementInternal" | "StarExpression" | "ColumnsExpression"
        )
    })
}

/// The `WhereClause` or `OnClause` `pred` is a top-level conjunct of, when
/// it is one: only `AND` and one-child links between them.
fn conjunct_of(tree: &Tree, pred: NodeId) -> Option<NodeId> {
    let mut n = pred;
    while let Some(p) = tree.parent(n) {
        match rule(tree, p) {
            "WhereClause" | "OnClause" => return Some(p),
            "LogicalAndExpression" | "LogicalAndExpressionTail" => {}
            _ if tree.children(p).len() == 1 => {}
            _ => return None,
        }
        n = p;
    }
    None
}

// ---------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Text,
    /// An integer type.
    Integer,
    /// A float or decimal type.
    Fraction,
    Temporal,
    Boolean,
}

/// The kind of a type as a snapshot or a cast spells it; `None` for
/// anything else (lists, structs, JSON, UUID, BLOB, INTERVAL, enums).
fn kind(ty: &str) -> Option<Kind> {
    let t = ty.trim().to_ascii_uppercase();
    let base = t.split('(').next().unwrap_or_default().trim();
    if base.ends_with(']') {
        return None;
    }
    Some(match base {
        "VARCHAR" | "CHAR" | "BPCHAR" | "TEXT" | "STRING" | "NVARCHAR" | "CHARACTER"
        | "CHARACTER VARYING" => Kind::Text,
        "TINYINT" | "SMALLINT" | "INTEGER" | "INT" | "BIGINT" | "HUGEINT" | "UTINYINT"
        | "USMALLINT" | "UINTEGER" | "UBIGINT" | "UHUGEINT" | "INT1" | "INT2" | "INT4" | "INT8"
        | "INT16" | "INT32" | "INT64" | "INT128" | "LONG" | "SHORT" | "SIGNED" | "VARINT"
        | "BIGNUM" => Kind::Integer,
        "FLOAT" | "DOUBLE" | "REAL" | "FLOAT4" | "FLOAT8" | "DECIMAL" | "NUMERIC"
        | "DOUBLE PRECISION" => Kind::Fraction,
        "DATE"
        | "TIME"
        | "TIMESTAMP"
        | "TIMESTAMPTZ"
        | "DATETIME"
        | "TIMESTAMP_S"
        | "TIMESTAMP_MS"
        | "TIMESTAMP_NS"
        | "TIMESTAMP_US"
        | "TIMETZ"
        | "TIMESTAMP WITH TIME ZONE"
        | "TIME WITH TIME ZONE" => Kind::Temporal,
        "BOOLEAN" | "BOOL" | "LOGICAL" => Kind::Boolean,
        _ => return None,
    })
}

/// Do two kinds compare without a cast between kinds?
fn same_family(a: Kind, b: Kind) -> bool {
    let num = |k| matches!(k, Kind::Integer | Kind::Fraction);
    a == b || (num(a) && num(b))
}

// ---------------------------------------------------------------------
// CAT003 unbounded-scan
// ---------------------------------------------------------------------

/// Functions that return one value per row and never aggregate: a select
/// list made of these and columns returns every row it reads.
const SCALAR_FNS: &[&str] = &[
    "lower",
    "upper",
    "lcase",
    "ucase",
    "trim",
    "ltrim",
    "rtrim",
    "concat",
    "concat_ws",
    "coalesce",
    "ifnull",
    "nullif",
    "round",
    "abs",
    "floor",
    "ceil",
    "length",
    "substr",
    "substring",
    "left",
    "right",
    "replace",
    "strftime",
    "strptime",
    "date_trunc",
    "date_part",
    "year",
    "month",
    "day",
    "hour",
    "epoch",
    "struct_extract",
    "list_extract",
    "json_extract",
    "json_extract_string",
    "greatest",
    "least",
    "md5",
    "hash",
];

/// Is `select` the outermost query of a plain `SELECT` statement (one arm
/// of a set operation counts), so that what it returns is the result?
fn is_result_query(tree: &Tree, select: NodeId) -> bool {
    let mut cur = tree.parent(select);
    let mut internal = 0;
    while let Some(p) = cur {
        match rule(tree, p) {
            "OptionalParensSimpleSelect"
            | "SimpleSelectParens"
            | "SelectStatementType"
            | "SelectAtom"
            | "IntersectChain"
            | "IntersectChainTail"
            | "SelectSetOpChain"
            | "SelectSetOpChainTail"
            | "SelectStatement" => {}
            "SelectStatementInternal" => internal += 1,
            "Statement" => {
                return internal == 1
                    && tree
                        .parent(p)
                        .is_some_and(|t| rule(tree, t) == "TopLevelStatement");
            }
            _ => return false,
        }
        cur = tree.parent(p);
    }
    false
}

/// CAT003 — `SELECT` of every row of a table the catalog estimates at
/// `large_table_rows` or more: the statement's result query reads one
/// catalog table, with no WHERE, QUALIFY, SAMPLE, LIMIT, join, GROUP BY,
/// DISTINCT or aggregate anywhere in it.
///
/// Aggregate-only queries (`SELECT count(*) FROM big`) are left alone on
/// purpose: they read the whole table because the answer needs all of it,
/// and they return one row. The rule is about a query that hands millions
/// of rows back, usually a forgotten filter or LIMIT. A function the rule
/// does not know might be an aggregate (a macro, an extension's), so a
/// select list calling one keeps it silent. Only the statement's own result
/// counts: a CTE or subquery is filtered by whatever reads it, and `CREATE
/// TABLE AS`, `INSERT ... SELECT` and `COPY` copy the table on purpose.
fn unbounded_scan(cx: &Cx<'_>, out: &mut Checked) {
    let tree = cx.s.tree;
    let src = cx.s.src;
    // A LIMIT anywhere: the statement is bounded somewhere, say nothing.
    if cx
        .s
        .nodes
        .iter()
        .any(|&n| matches!(rule(tree, n), "LimitOffset" | "LimitClause" | "FetchClause"))
    {
        return;
    }
    for (&select, scope) in &cx.scopes {
        if !is_result_query(tree, select) || !scope.known || scope.items != 1 {
            continue;
        }
        let [rel] = scope.rels.as_slice() else {
            continue;
        };
        let Some(rows) = rel.table.row_estimate() else {
            continue;
        };
        if rel.table.is_view || rows < cx.s.options.large_table_rows {
            continue;
        }
        let bounded = tree.children(select).iter().any(|&c| {
            matches!(
                rule(tree, c),
                "WhereClause"
                    | "GroupByClause"
                    | "HavingClause"
                    | "QualifyClause"
                    | "SelectSampleClause"
                    | "WindowClause"
            )
        }) || direct_child(tree, rel.node, "SampleClause").is_some();
        if bounded {
            continue;
        }
        // The select list: DISTINCT, a window, or a function that may
        // aggregate.
        let mut quiet = false;
        for d in tree.descendants(select) {
            match rule(tree, d) {
                "DistinctOn" | "OverClause" | "SelectStatementInternal" => quiet = true,
                "FunctionExpression" => {
                    let name = first_descendant(tree, d, &["FunctionName"])
                        .map(|f| ident(tree.text(f, src)))
                        .unwrap_or_default();
                    if !SCALAR_FNS.contains(&name.as_str()) {
                        quiet = true;
                    }
                }
                _ => {}
            }
        }
        if quiet {
            continue;
        }
        let Some(name) = first_descendant(tree, rel.node, &["TableName", "ReservedTableName"])
        else {
            continue;
        };
        push(
            out,
            "CAT003",
            tight(tree, name),
            format!("{} holds about {} rows", rel.table.name, thousands(rows)),
        );
    }
}

/// `2400000` as `2,400,000`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------
// CAT004 index-defeating-filter
// ---------------------------------------------------------------------

/// Functions that, run against DuckDB 1.5 and 2.0, keep a comparison from
/// becoming a filter on the column: the plan filters on the function's
/// result, row by row. Functions DuckDB rewrites into a range on the
/// column (`date_trunc`, `left`, `substr` in 2.0, a DATE cast of a
/// timestamp) or simplifies away (`coalesce(c, 0) = 5`, arithmetic with a
/// constant) are not here.
const DEFEATING_FNS: &[&str] = &[
    "lower", "upper", "lcase", "ucase", "trim", "ltrim", "rtrim", "strftime", "length",
];

/// What a filter does to the column it compares.
enum Wrap {
    Function(String),
    Cast(String),
}

/// The single column `side` wraps and how, when it is a shape that defeats
/// filtering on the column: a [`DEFEATING_FNS`] call with the column as an
/// argument, or a cast between kinds DuckDB cannot invert (to text from
/// anything else, or from a fraction to an integer), judged against the
/// column's type in the catalog.
fn wrapped<'a>(cx: &Cx<'a>, side: NodeId) -> Option<(Resolved<'a>, Wrap)> {
    let tree = cx.s.tree;
    let src = cx.s.src;
    let columns: Vec<NodeId> = std::iter::once(side)
        .chain(tree.descendants(side))
        .filter(|&d| rule(tree, d) == "ColumnReference")
        .collect();
    let [colref] = columns.as_slice() else {
        return None;
    };
    let colref = *colref;
    // Down the precedence ladder to the first node that does something.
    let mut n = side;
    while let [only] = tree.children(n) {
        if matches!(
            rule(tree, n),
            "FunctionExpression" | "CastExpression" | "TrimExpression"
        ) {
            break;
        }
        n = *only;
    }
    let (wrap, target) = match rule(tree, n) {
        "FunctionExpression" => {
            let name =
                first_descendant(tree, n, &["FunctionName"]).map(|f| ident(tree.text(f, src)))?;
            if !DEFEATING_FNS.contains(&name.as_str())
                || direct_child(tree, n, "OverClause").is_some()
            {
                return None;
            }
            // The column must be an argument by itself.
            let args = direct_child(tree, n, "FunctionExpressionArguments")?;
            let list = first_descendant(tree, args, &["FunctionExpressionArgumentList"])?;
            let direct = bare_column(tree, list) == Some(colref)
                || direct_child(tree, list, "FunctionArgumentList").is_some_and(|l| {
                    tree.children(l)
                        .iter()
                        .any(|&a| bare_column(tree, a) == Some(colref))
                });
            if !direct {
                return None;
            }
            (Wrap::Function(name), None)
        }
        // `trim(col)`, which has a production of its own.
        "TrimExpression" => {
            let args = direct_child(tree, n, "TrimArguments")?;
            if bare_column(tree, args) != Some(colref) {
                return None;
            }
            (Wrap::Function("trim".to_string()), None)
        }
        "CastExpression" => {
            if !tree
                .text(n, src)
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("CAST")
            {
                return None; // TRY_CAST
            }
            let args = direct_child(tree, n, "CastArguments")?;
            let expr = direct_child(tree, args, "Expression")?;
            let ty = direct_child(tree, args, "Type")?;
            if bare_column(tree, expr) != Some(colref) {
                return None;
            }
            let ty = tree.text(ty, src).trim().to_string();
            (Wrap::Cast(ty.clone()), Some(ty))
        }
        _ => {
            // `col::T`: the column, then one cast.
            let [column, indirections] = tree.children(n) else {
                return None;
            };
            if bare_column(tree, *column) != Some(colref)
                || rule(tree, *indirections) != "IndirectionList"
            {
                return None;
            }
            let [indirection] = tree.children(*indirections) else {
                return None;
            };
            let cast = direct_child(tree, *indirection, "CastOperator")?;
            let ty = direct_child(tree, cast, "Type")?;
            let ty = tree.text(ty, src).trim().to_string();
            (Wrap::Cast(ty.clone()), Some(ty))
        }
    };
    let r = resolve(cx, colref)?;
    if let Some(target) = target {
        let from = kind(r.table.column_type(r.column)?)?;
        let to = kind(&target)?;
        let defeats = (to == Kind::Text && from != Kind::Text)
            || (from == Kind::Fraction && to == Kind::Integer);
        if !defeats {
            return None;
        }
    }
    Some((r, wrap))
}

/// Which kind of index `column` leads, as words.
fn index_kind(table: &Table, column: &str) -> &'static str {
    let leads = |k: &[String]| k.first().is_some_and(|c| c.eq_ignore_ascii_case(column));
    if leads(&table.primary_key) {
        "the primary key"
    } else if table.unique.iter().any(|k| leads(k)) {
        "a unique key"
    } else {
        "an index"
    }
}

/// CAT004 — a top-level conjunct of a WHERE or ON that compares the
/// leading column of a key or index, wrapped in a function or cast DuckDB
/// cannot see through, with a constant (`=`, `<`, `<=`, `>`, `>=`, `IN
/// (...)`, `BETWEEN`); or that matches such a text column with `LIKE` or
/// `ILIKE` and a pattern starting with a wildcard.
///
/// Each shape was run against DuckDB 1.5 and 2.0 with `EXPLAIN`: the plan
/// filters on the expression, not the column, so neither an index nor the
/// row-group statistics can be used. Shapes DuckDB rewrites into a filter on
/// the column (a cast it can invert, arithmetic with a constant,
/// `date_trunc`, a timestamp cast to DATE, `LIKE 'abc%'`) are not flagged.
///
/// MOD035 flags some of the same predicates without knowing of any index.
/// Where this rule fires, it reports the predicate in MOD035's place, with
/// the index as evidence; turned off, MOD035 comes back.
fn index_defeating_filter(cx: &Cx<'_>, out: &mut Checked) {
    let tree = cx.s.tree;
    let src = cx.s.src;
    let report = |out: &mut Checked, pred: NodeId, r: &Resolved<'_>, how: &str| {
        let span = tree.node(pred).span;
        push(
            out,
            "CAT004",
            span,
            format!(
                "{how} {}, which leads {}",
                qualified(r.table, r.column),
                index_kind(r.table, r.column)
            ),
        );
        out.overrides.push(Override {
            code: "MOD035",
            span,
            replaced_by: Some("CAT004"),
        });
    };
    let indexed = |r: &Resolved<'_>| r.table.is_indexed_leading(r.column);
    let wrap_words = |w: &Wrap| match w {
        Wrap::Function(f) => format!("{f}() is applied to"),
        Wrap::Cast(t) => format!("a cast to {t} is applied to"),
    };
    for &n in cx.s.nodes {
        match rule(tree, n) {
            "ComparisonExpression" => {
                let [subject, tail] = tree.children(n) else {
                    continue;
                };
                if rule(tree, *tail) != "ComparisonExpressionTail" || conjunct_of(tree, n).is_none()
                {
                    continue;
                }
                let [operator, other] = tree.children(*tail) else {
                    continue;
                };
                let op = tree.text(*operator, src).trim();
                if !matches!(op, "=" | "<" | "<=" | ">" | ">=") {
                    continue;
                }
                // Either side may be the column.
                let hit = if is_constant(tree, *other) {
                    wrapped(cx, *subject)
                } else if is_constant(tree, *subject) {
                    wrapped(cx, *other)
                } else {
                    None
                };
                if let Some((r, w)) = hit.filter(|(r, _)| indexed(r)) {
                    report(out, n, &r, &wrap_words(&w));
                }
            }
            "BetweenInLikeExpression" => {
                let [subject, op] = tree.children(n) else {
                    continue;
                };
                if !matches!(rule(tree, *op), "InPredicate" | "BetweenLikeOp")
                    || conjunct_of(tree, n).is_none()
                    || tree
                        .text(*op, src)
                        .trim_start()
                        .to_ascii_uppercase()
                        .starts_with("NOT")
                {
                    continue;
                }
                if let Some(like) = first_descendant(tree, *op, &["LikeClause"]) {
                    // `col LIKE '%...'`: the bare column, a literal pattern.
                    let Some(colref) = bare_column(tree, *subject) else {
                        continue;
                    };
                    let token = first_descendant(tree, like, &["LikeToken", "ILikeToken"]);
                    let pattern = tree
                        .children(like)
                        .last()
                        .and_then(|&p| reduces_to(tree, p, "StringLiteral"));
                    let (Some(token), Some(pattern)) = (token, pattern) else {
                        continue;
                    };
                    let lit = tree.text(pattern, src).trim();
                    if !(lit.starts_with("'%") || lit.starts_with("'_")) {
                        continue;
                    }
                    let Some(r) = resolve(cx, colref) else {
                        continue;
                    };
                    let text_column = r
                        .table
                        .column_type(r.column)
                        .and_then(kind)
                        .is_some_and(|k| k == Kind::Text);
                    if text_column && indexed(&r) {
                        let how = if rule(tree, token) == "ILikeToken" {
                            "ILIKE with a leading wildcard matches"
                        } else {
                            "LIKE with a leading wildcard matches"
                        };
                        report(out, n, &r, how);
                    }
                    continue;
                }
                let Some(kind_node) = first_descendant(tree, *op, &["InClause", "BetweenClause"])
                else {
                    continue;
                };
                let operands_constant = if rule(tree, kind_node) == "InClause" {
                    first_descendant(tree, kind_node, &["InExpressionList"])
                        .is_some_and(|l| tree.children(l).iter().all(|&c| is_constant(tree, c)))
                } else {
                    tree.children(kind_node)
                        .iter()
                        .all(|&c| is_constant(tree, c))
                };
                if !operands_constant {
                    continue;
                }
                if let Some((r, w)) = wrapped(cx, *subject).filter(|(r, _)| indexed(r)) {
                    report(out, n, &r, &wrap_words(&w));
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------
// CAT005 nullable-not-in
// ---------------------------------------------------------------------

/// The one `SimpleSelect` a `SelectStatementInternal` is, when it is no set
/// operation.
fn single_select(tree: &Tree, internal: NodeId) -> Option<NodeId> {
    let chain = direct_child(tree, internal, "SelectSetOpChain")?;
    reduces_to(tree, chain, "SimpleSelect")
}

/// CAT005 — `x NOT IN (SELECT c FROM t ...)` where `t` is the subquery's
/// only relation and the catalog says `c` may hold NULL. Then one NULL in
/// `c` makes the test NULL for every `x` and the filter keeps nothing.
///
/// It is the subquery's column that matters. A NULL `x` only drops its
/// own row (NOT EXISTS would keep it), which is a different and much
/// smaller surprise, and MOD028 does not claim it. So where the catalog
/// says `c` cannot be NULL, MOD028 is withdrawn for that `NOT IN`; where it
/// says `c` may be NULL, this rule reports it in MOD028's place with the
/// column named. A subquery with a join, a set operation, anything but a
/// bare column, or a column whose nullability is not given leaves MOD028
/// as it is. MOD032 (a literal NULL in a `NOT IN` list) does not read any
/// column and is not touched.
fn nullable_not_in(cx: &Cx<'_>, out: &mut Checked) {
    let tree = cx.s.tree;
    let src = cx.s.src;
    for &op in cx.s.nodes {
        if rule(tree, op) != "InPredicate"
            || !tree
                .text(op, src)
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("NOT")
        {
            continue;
        }
        let Some(sub) = first_descendant(tree, op, &["InSelectStatement"]) else {
            continue;
        };
        let Some(internal) = first_descendant(tree, sub, &["SelectStatementInternal"]) else {
            continue;
        };
        let Some(select) = single_select(tree, internal) else {
            continue;
        };
        let Some(scope) = cx.scopes.get(&select) else {
            continue;
        };
        if !scope.known || scope.items != 1 || scope.rels.len() != 1 {
            continue;
        }
        let Some(targets) = first_descendant(tree, select, &["TargetList"]) else {
            continue;
        };
        let [item] = tree.children(targets) else {
            continue;
        };
        // The expression itself, alias or not.
        let expr = first_descendant(tree, *item, &["Expression"]).unwrap_or(*item);
        let Some(colref) = bare_column(tree, expr) else {
            continue;
        };
        if expr_owner(tree, colref) != Some(select) {
            continue;
        }
        let Some(r) = resolve(cx, colref) else {
            continue;
        };
        let span = tree.node(op).span;
        match r.table.is_nullable(r.column) {
            Some(false) => out.overrides.push(Override {
                code: "MOD028",
                span,
                replaced_by: None,
            }),
            Some(true) => {
                if crate::tier4::excludes_nulls(tree, sub, src) {
                    continue;
                }
                push(
                    out,
                    "CAT005",
                    span,
                    format!("{} may be NULL", qualified(r.table, r.column)),
                );
                out.overrides.push(Override {
                    code: "MOD028",
                    span,
                    replaced_by: Some("CAT005"),
                });
            }
            None => {}
        }
    }
}

// ---------------------------------------------------------------------
// CAT006 ambiguous-column
// ---------------------------------------------------------------------

/// CAT006 — an unqualified column name that two or more tables of its
/// query's FROM have. DuckDB rejects the statement ("Ambiguous reference
/// to column name"), in every clause of the query: the select list (a
/// select-list alias of the same name does not help), WHERE, GROUP BY,
/// HAVING, QUALIFY and a window's ORDER BY.
///
/// Checked against DuckDB 1.5 and 2.0. Silent where DuckDB resolves the
/// name or this module cannot tell: a name in a `USING (...)` of the FROM,
/// a NATURAL, SEMI or ANTI join, a FROM with anything but catalog tables,
/// a name no table of the query has (it may be an outer query's), a lambda
/// parameter, and the statement's ORDER BY (which prefers select-list
/// names). An ON condition sees only the tables joined before it in its
/// own item of a comma-separated FROM, and an ON inside a parenthesized
/// join is not checked.
fn ambiguous_column(cx: &Cx<'_>, out: &mut Checked) {
    let tree = cx.s.tree;
    for &n in cx.s.nodes {
        if rule(tree, n) != "ColumnReference" {
            continue;
        }
        let Some(name) = unqualified_name(cx, n) else {
            continue;
        };
        if cx.s.lambdas.contains(&name) {
            continue;
        }
        let Some(scope) = expr_owner(tree, n).and_then(|s| cx.scopes.get(&s)) else {
            continue;
        };
        if !scope.known || scope.odd_join || scope.using.contains(&name) {
            continue;
        }
        let on = tree.ancestor(n, "OnClause").filter(|&on| {
            // The ON of this query, not of a subquery inside it.
            expr_owner(tree, on) == expr_owner(tree, n)
        });
        if on.is_some() && scope.parens {
            continue;
        }
        // An ON sees the tables joined before it in its own item of the
        // FROM list: `FROM a, b JOIN c ON ...` joins b and c, and a is not
        // in sight.
        let sight = on.map_or(grebe_syntax::Span::new(0, u32::MAX), |on| {
            let mut item = on;
            while let Some(p) = tree.parent(item) {
                if rule(tree, p) == "FromClause" {
                    break;
                }
                item = p;
            }
            grebe_syntax::Span::new(tree.node(item).span.start, tree.node(on).span.start)
        });
        let having: Vec<&Rel<'_>> = scope
            .rels
            .iter()
            .filter(|r| {
                let at = tree.node(r.node).span.start;
                at >= sight.start && at < sight.end && r.table.has_column(&name)
            })
            .collect();
        if having.len() < 2 {
            continue;
        }
        let mut names: Vec<String> = Vec::new();
        for r in &having {
            if !names.contains(&r.name) {
                names.push(r.name.clone());
            }
        }
        let list = match names.as_slice() {
            [a, b] => format!("{a} and {b}"),
            _ => {
                let (last, rest) = names.split_last().expect("two or more");
                format!("{}, and {last}", rest.join(", "))
            }
        };
        let shown = tree.text(n, cx.s.src).trim();
        push(
            out,
            "CAT006",
            tight(tree, n),
            format!("{shown} is in {list}"),
        );
    }
}

// ---------------------------------------------------------------------
// CAT007 insert-mismatch
// ---------------------------------------------------------------------

/// CAT007 — `INSERT INTO t ... VALUES` that DuckDB rejects by the shape of
/// the table alone:
///
/// - with a column list, a name `t` does not have, or a row whose width is
///   not the list's;
/// - without one, a row wider than `t` has columns.
///
/// A row narrower than the table is not flagged: DuckDB counts the table's
/// columns without its generated ones, and a snapshot does not say which
/// those are. `BY NAME`, `BY POSITION`, `DEFAULT VALUES` and `INSERT ...
/// SELECT` are not checked, nor a target the file defines itself or a
/// view.
fn insert_mismatch(cx: &Cx<'_>, out: &mut Checked) {
    let tree = cx.s.tree;
    let src = cx.s.src;
    for &ins in cx.s.nodes {
        if rule(tree, ins) != "InsertStatement" {
            continue;
        }
        let Some(target) = direct_child(tree, ins, "InsertTarget") else {
            continue;
        };
        let Some(Relation::Table(at)) =
            cx.s.binding_nodes
                .iter()
                .position(|&b| b == target)
                .map(|i| cx.s.bindings[i].1)
        else {
            continue;
        };
        let table = cx.s.tables[at];
        if table.is_view || direct_child(tree, ins, "ByNameOrPosition").is_some() {
            continue;
        }
        let Some(values) = direct_child(tree, ins, "InsertValues")
            .and_then(|v| first_descendant(tree, v, &["SelectInsertValues"]))
            .and_then(|v| direct_child(tree, v, "SelectStatementInternal"))
            .and_then(|i| {
                let chain = direct_child(tree, i, "SelectSetOpChain")?;
                reduces_to(tree, chain, "ValuesClause")
            })
        else {
            continue;
        };
        let names: Option<Vec<NodeId>> = direct_child(tree, ins, "InsertColumnList")
            .and_then(|l| first_descendant(tree, l, &["ColumnList"]))
            .map(|l| tree.children(l).to_vec());
        if let Some(names) = &names {
            for &c in names {
                let name = ident(tree.text(c, src));
                if !table.has_column(&name) {
                    push(
                        out,
                        "CAT007",
                        tight(tree, c),
                        format!("{} has no column {}", table.name, tree.text(c, src).trim()),
                    );
                }
            }
        }
        for &row in tree.children(values) {
            if rule(tree, row) != "ValuesExpressions" {
                continue;
            }
            let width = tree
                .children(row)
                .iter()
                .filter(|&&c| rule(tree, c) == "Expression")
                .count();
            let detail = match &names {
                Some(names) if names.len() != width => format!(
                    "{} column{} named, {width} value{} given",
                    names.len(),
                    plural(names.len()),
                    plural(width)
                ),
                None if width > table.columns.len() => format!(
                    "{} has {} column{}, the row has {width} values",
                    table.name,
                    table.columns.len(),
                    plural(table.columns.len())
                ),
                _ => continue,
            };
            push(out, "CAT007", tight(tree, row), detail);
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ---------------------------------------------------------------------
// CAT008 type-mismatch
// ---------------------------------------------------------------------

/// CAT008 — `a <op> b` in a query, both sides bare columns the catalog
/// resolves, of different kinds: text, a number, a date or time, a
/// boolean. DuckDB compares them by casting one side to the other's type
/// (run against 1.5 and 2.0: no error), which is rarely what was meant and
/// fails at run time on the first value that does not cast. A literal on
/// either side is never flagged, nor a type outside the four kinds (JSON,
/// UUID, lists, structs, enums, intervals).
fn type_mismatch(cx: &Cx<'_>, out: &mut Checked) {
    let tree = cx.s.tree;
    let src = cx.s.src;
    for &n in cx.s.nodes {
        if rule(tree, n) != "ComparisonExpression" {
            continue;
        }
        let [subject, tail] = tree.children(n) else {
            continue;
        };
        if rule(tree, *tail) != "ComparisonExpressionTail" {
            continue;
        }
        let [operator, other] = tree.children(*tail) else {
            continue;
        };
        let op = tree.text(*operator, src).trim();
        if !matches!(op, "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=") {
            continue;
        }
        let (Some(a), Some(b)) = (bare_column(tree, *subject), bare_column(tree, *other)) else {
            continue;
        };
        let (Some(ra), Some(rb)) = (resolve(cx, a), resolve(cx, b)) else {
            continue;
        };
        let (Some(ta), Some(tb)) = (
            ra.table.column_type(ra.column),
            rb.table.column_type(rb.column),
        ) else {
            continue;
        };
        let (Some(ka), Some(kb)) = (kind(ta), kind(tb)) else {
            continue;
        };
        if same_family(ka, kb) {
            continue;
        }
        push(
            out,
            "CAT008",
            tree.node(n).span,
            format!(
                "{} is {ta} and {} is {tb}",
                qualified(ra.table, ra.column),
                qualified(rb.table, rb.column)
            ),
        );
    }
}

// ---------------------------------------------------------------------
// CAT009 fan-out-join
// ---------------------------------------------------------------------

/// CAT009 — a join of exactly two catalog tables (the query's whole FROM)
/// whose condition is only column equalities between them, or a `USING`
/// list, where both tables have a key in the snapshot and the join's
/// columns hold a key of neither. Each row can then meet many rows on the
/// other side and many on its own, so the result can hold more rows than
/// either table: usually a missing join column or a missing aggregate
/// before the join.
///
/// A table with no key in the snapshot is not judged (it may simply not
/// declare one). SEMI, ANTI, ASOF, NATURAL, POSITIONAL and cross joins, a
/// condition with anything but equalities of the two tables' columns, and
/// a self-join through the same column are left alone.
fn fan_out_join(cx: &Cx<'_>, out: &mut Checked) {
    let tree = cx.s.tree;
    let src = cx.s.src;
    for &n in cx.s.nodes {
        if rule(tree, n) != "RegularJoinClause" {
            continue;
        }
        let Some(scope) = from_owner(tree, n).and_then(|s| cx.scopes.get(&s)) else {
            continue;
        };
        if !scope.known || scope.odd_join || scope.items != 2 || scope.rels.len() != 2 {
            continue;
        }
        if direct_child(tree, n, "Asof").is_some() {
            continue;
        }
        let (left, right) = (&scope.rels[0], &scope.rels[1]);
        if left.table.unique_keys().next().is_none() || right.table.unique_keys().next().is_none() {
            continue;
        }
        let Some(qualifier) = direct_child(tree, n, "JoinQualifier") else {
            continue;
        };
        let mut lcols: Vec<String> = Vec::new();
        let mut rcols: Vec<String> = Vec::new();
        if let Some(using) = direct_child(tree, qualifier, "UsingClause") {
            for c in tree.descendants(using) {
                if rule(tree, c) == "UsingColumnName" {
                    let name = ident(tree.text(c, src));
                    let (Some(l), Some(r)) = (left.table.column(&name), right.table.column(&name))
                    else {
                        lcols.clear();
                        break;
                    };
                    lcols.push(l.name.clone());
                    rcols.push(r.name.clone());
                }
            }
        } else if let Some(on) = direct_child(tree, qualifier, "OnClause") {
            let Some(pairs) = equalities(cx, on, left, right) else {
                continue;
            };
            for (l, r) in pairs {
                lcols.push(l);
                rcols.push(r);
            }
        }
        if lcols.is_empty() {
            continue;
        }
        let lrefs: Vec<&str> = lcols.iter().map(String::as_str).collect();
        let rrefs: Vec<&str> = rcols.iter().map(String::as_str).collect();
        if left.table.is_unique_on(&lrefs) || right.table.is_unique_on(&rrefs) {
            continue;
        }
        if std::ptr::eq(left.table, right.table) && lcols == rcols {
            continue;
        }
        push(
            out,
            "CAT009",
            tight(tree, qualifier),
            format!(
                "neither {}({}) nor {}({}) is a key",
                left.table.name,
                lcols.join(", "),
                right.table.name,
                rcols.join(", ")
            ),
        );
    }
}

/// The ON condition `on` as `left.column = right.column` pairs, when it is
/// nothing else: every conjunct one equality between a column of each
/// side.
fn equalities(
    cx: &Cx<'_>,
    on: NodeId,
    left: &Rel<'_>,
    right: &Rel<'_>,
) -> Option<Vec<(String, String)>> {
    let tree = cx.s.tree;
    let src = cx.s.src;
    let expr = direct_child(tree, on, "Expression")?;
    // The conjuncts: the AND chain's first operand and each tail's.
    let mut n = expr;
    while let [only] = tree.children(n) {
        if rule(tree, n) == "LogicalAndExpression" {
            break;
        }
        n = *only;
    }
    let conjuncts: Vec<NodeId> = if rule(tree, n) == "LogicalAndExpression" {
        tree.children(n)
            .iter()
            .map(|&c| {
                if rule(tree, c) == "LogicalAndExpressionTail" {
                    tree.children(c).last().copied().unwrap_or(c)
                } else {
                    c
                }
            })
            .collect()
    } else {
        vec![n]
    };
    let side = |r: &Resolved<'_>| -> Option<bool> {
        // Which side a resolved column is on: by the relation it was
        // found in, or by the name it is qualified with.
        if let Some(node) = r.rel_node {
            if node == left.node {
                return Some(true);
            }
            if node == right.node {
                return Some(false);
            }
            return None;
        }
        match (r.rel_name == left.name, r.rel_name == right.name) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    };
    let mut pairs = Vec::new();
    for c in conjuncts {
        let cmp = reduces_to(tree, c, "ComparisonExpression")?;
        let [a, tail] = tree.children(cmp) else {
            return None;
        };
        let [op, b] = tree.children(*tail) else {
            return None;
        };
        if tree.text(*op, src).trim() != "=" {
            return None;
        }
        let ra = resolve(cx, bare_column(tree, *a)?)?;
        let rb = resolve(cx, bare_column(tree, *b)?)?;
        match (side(&ra)?, side(&rb)?) {
            (true, false) => pairs.push((ra.column.to_string(), rb.column.to_string())),
            (false, true) => pairs.push((rb.column.to_string(), ra.column.to_string())),
            _ => return None,
        }
    }
    Some(pairs)
}

// ---------------------------------------------------------------------
// CAT010 redundant-distinct
// ---------------------------------------------------------------------

/// The columns of a key of `table` that cannot be NULL: the primary key,
/// or a unique key whose every column the snapshot says is NOT NULL. A
/// unique key with a nullable column lets several rows hold NULL there.
fn non_null_keys(table: &Table) -> impl Iterator<Item = &[String]> {
    table.unique_keys().filter(move |k| {
        std::ptr::eq(*k, table.primary_key.as_slice())
            || k.iter().all(|c| table.is_nullable(c) == Some(false))
    })
}

/// CAT010 — `SELECT DISTINCT` over one catalog table (no join, no GROUP
/// BY) whose select list is bare columns of it, or a plain `*`, and holds
/// every column of a key that cannot be NULL. The rows already differ in
/// the key, so DISTINCT only costs a hash of every row. `DISTINCT ON`, a
/// select list with any expression, and a unique key that allows NULL are
/// left alone.
fn redundant_distinct(cx: &Cx<'_>, out: &mut Checked) {
    let tree = cx.s.tree;
    let src = cx.s.src;
    for (&select, scope) in &cx.scopes {
        if !scope.known || scope.items != 1 || scope.rels.len() != 1 {
            continue;
        }
        let rel = &scope.rels[0];
        let Some(clause) = first_descendant(tree, select, &["SelectClause"]) else {
            continue;
        };
        if expr_owner(tree, clause) != Some(select)
            || direct_child(tree, select, "GroupByClause").is_some()
        {
            continue;
        }
        let Some(distinct) = direct_child(tree, clause, "DistinctClause")
            .and_then(|d| direct_child(tree, d, "DistinctOn"))
        else {
            continue;
        };
        if !tree.children(distinct).is_empty() {
            continue; // DISTINCT ON (...)
        }
        let Some(targets) = direct_child(tree, clause, "TargetList") else {
            continue;
        };
        let mut columns: Vec<String> = Vec::new();
        let mut plain = true;
        for &item in tree.children(targets) {
            if let Some(star) = reduces_to(tree, item, "StarExpression") {
                // `*` alone, nothing excluded or replaced.
                if tree.text(star, src).trim() == "*" {
                    columns.extend(rel.table.columns.iter().map(|c| c.name.clone()));
                    continue;
                }
                plain = false;
                break;
            }
            let expr = first_descendant(tree, item, &["Expression"]).unwrap_or(item);
            let resolved = bare_column(tree, expr).and_then(|c| resolve(cx, c));
            match resolved {
                Some(r) if r.rel_node.map_or(r.rel_name == rel.name, |n| n == rel.node) => {
                    columns.push(r.column.to_string());
                }
                _ => {
                    plain = false;
                    break;
                }
            }
        }
        if !plain {
            continue;
        }
        let have: Vec<&str> = columns.iter().map(String::as_str).collect();
        let Some(key) = non_null_keys(rel.table).find(|k| {
            k.iter()
                .all(|c| have.iter().any(|h| h.eq_ignore_ascii_case(c)))
        }) else {
            continue;
        };
        let what = if std::ptr::eq(key, rel.table.primary_key.as_slice()) {
            "the primary key"
        } else {
            "a unique key"
        };
        let detail = format!("{} is {what} of {}", key.join(", "), rel.table.name);
        push(out, "CAT010", tight(tree, distinct), detail);
    }
}

#[cfg(test)]
mod tests {
    use crate::Severity;
    use crate::analysis::{Selection, analyze};
    use crate::catalog::{Catalog, Options, message};
    use crate::detect::Finding;

    const SHOP: &str = r#"{"columns": [
        {"database":"shop","schema":"main","table":"orders","column":"id","type":"BIGINT","nullable":false},
        {"database":"shop","schema":"main","table":"orders","column":"customer_id","type":"BIGINT","nullable":true},
        {"database":"shop","schema":"main","table":"orders","column":"note","type":"VARCHAR","nullable":true},
        {"database":"shop","schema":"main","table":"orders","column":"placed","type":"TIMESTAMP","nullable":true},
        {"database":"shop","schema":"main","table":"customers","column":"id","type":"BIGINT","nullable":false},
        {"database":"shop","schema":"main","table":"customers","column":"email","type":"VARCHAR","nullable":true},
        {"database":"shop","schema":"main","table":"customers","column":"note","type":"VARCHAR","nullable":true}
    ], "tables": [
        {"database":"shop","schema":"main","table":"orders","rows":2000000,"primary_key":["id"],"indexes":[["placed"]]},
        {"database":"shop","schema":"main","table":"customers","rows":500,"primary_key":["id"],"unique":[["email"]]}
    ]}"#;

    /// The same tables with their columns only.
    const BARE: &str = r#"[
        {"database":"shop","schema":"main","table":"orders","column":"id","type":"BIGINT"},
        {"database":"shop","schema":"main","table":"orders","column":"customer_id","type":"BIGINT"},
        {"database":"shop","schema":"main","table":"orders","column":"note","type":"VARCHAR"},
        {"database":"shop","schema":"main","table":"orders","column":"placed","type":"TIMESTAMP"},
        {"database":"shop","schema":"main","table":"customers","column":"id","type":"BIGINT"},
        {"database":"shop","schema":"main","table":"customers","column":"email","type":"VARCHAR"},
        {"database":"shop","schema":"main","table":"customers","column":"note","type":"VARCHAR"}
    ]"#;

    fn run_with(src: &str, json: &str, severity: &[(String, Severity)], rows: u64) -> Vec<Finding> {
        let catalog = Catalog::from_json_text(json).expect("snapshot");
        analyze(
            src,
            &Selection {
                severity,
                catalog: Some(&catalog),
                catalog_options: Options {
                    large_table_rows: rows,
                },
                ..Selection::default()
            },
        )
    }

    fn run(src: &str) -> Vec<Finding> {
        run_with(src, SHOP, &[], 1_000_000)
    }

    fn codes(f: &[Finding]) -> Vec<&'static str> {
        f.iter().map(|f| f.code).collect()
    }

    fn has(src: &str, code: &str) -> bool {
        run(src).iter().any(|f| f.code == code)
    }

    #[test]
    fn unbounded_scan_names_the_table_and_its_size() {
        let src = "SELECT * FROM orders;";
        let f = run(src);
        let hit = f.iter().find(|f| f.code == "CAT003").expect("CAT003");
        assert_eq!(
            &src[hit.span.start as usize..hit.span.end as usize],
            "orders"
        );
        assert_eq!(
            hit.detail.as_deref(),
            Some("orders holds about 2,000,000 rows")
        );
        assert!(message(hit, src).ends_with("By the catalog, orders holds about 2,000,000 rows."));
        for quiet in [
            "SELECT * FROM orders WHERE id = 1;",
            "SELECT * FROM orders LIMIT 5;",
            "SELECT count() FROM orders;",
            "SELECT * FROM customers;",
            "CREATE TABLE t AS SELECT * FROM orders;",
        ] {
            assert!(!has(quiet, "CAT003"), "{quiet}");
        }
    }

    #[test]
    fn the_threshold_is_an_option() {
        let src = "SELECT * FROM customers;";
        assert!(codes(&run_with(src, SHOP, &[], 100)).contains(&"CAT003"));
        assert!(
            !codes(&run_with("SELECT * FROM orders;", SHOP, &[], 3_000_000)).contains(&"CAT003")
        );
    }

    #[test]
    fn a_snapshot_with_columns_only_changes_nothing() {
        for src in [
            "SELECT * FROM orders;",
            "SELECT id FROM orders WHERE strftime(placed, '%Y') = '2024';",
            "SELECT 1 FROM customers WHERE id NOT IN (SELECT customer_id FROM orders);",
            "SELECT DISTINCT id FROM orders;",
            "SELECT * FROM orders o JOIN customers c ON c.note = o.note;",
        ] {
            let with = run_with(src, BARE, &[], 1_000_000);
            let without = crate::analysis::analyze_source(src, None);
            assert_eq!(codes(&with), codes(&without), "{src}");
        }
    }

    #[test]
    fn index_defeating_filter_takes_mod035s_place() {
        let src = "SELECT id FROM orders WHERE strftime(placed, '%Y') = '2024';";
        let f = run(src);
        assert!(codes(&f).contains(&"CAT004"), "{f:?}");
        assert!(!codes(&f).contains(&"MOD035"), "{f:?}");
        let off = [("CAT004".to_string(), Severity::Off)];
        let f = run_with(src, SHOP, &off, 1_000_000);
        assert!(codes(&f).contains(&"MOD035"), "{f:?}");
        // Without an index, MOD035 alone.
        let f = run(
            "SELECT id FROM orders WHERE strftime(placed, '%Y') = '2024' AND year(placed) = 2024;",
        );
        assert_eq!(
            codes(&f).iter().filter(|c| **c == "MOD035").count(),
            1,
            "{f:?}"
        );
        assert!(!has(
            "SELECT id FROM orders WHERE lower(note) = 'x';",
            "CAT004"
        ));
        assert!(has(
            "SELECT id FROM customers WHERE lower(email) = 'x';",
            "CAT004"
        ));
        assert!(!has("SELECT id FROM customers WHERE id + 1 = 2;", "CAT004"));
    }

    #[test]
    fn not_in_follows_the_columns_nullability() {
        // NOT NULL: MOD028 is withdrawn.
        let f = run("SELECT 1 FROM orders WHERE customer_id NOT IN (SELECT id FROM customers);");
        assert!(!codes(&f).contains(&"MOD028"), "{f:?}");
        assert!(!codes(&f).contains(&"CAT005"), "{f:?}");
        // Nullable: CAT005 in its place, and MOD028 back when CAT005 is off.
        let src = "SELECT 1 FROM customers WHERE id NOT IN (SELECT customer_id FROM orders);";
        let f = run(src);
        assert!(
            codes(&f).contains(&"CAT005") && !codes(&f).contains(&"MOD028"),
            "{f:?}"
        );
        let hit = f.iter().find(|f| f.code == "CAT005").unwrap();
        assert_eq!(
            hit.detail.as_deref(),
            Some("orders.customer_id may be NULL")
        );
        let off = [("CAT005".to_string(), Severity::Off)];
        assert!(codes(&run_with(src, SHOP, &off, 1_000_000)).contains(&"MOD028"));
        // Unknown nullability, or no catalog: MOD028 as before.
        assert!(codes(&run_with(src, BARE, &[], 1_000_000)).contains(&"MOD028"));
        assert!(codes(&crate::analysis::analyze_source(src, None)).contains(&"MOD028"));
        // A join in the subquery: an outer join could add NULLs.
        let f = run(
            "SELECT 1 FROM orders WHERE id NOT IN (SELECT c.id FROM customers c LEFT JOIN orders o ON o.id = c.id);",
        );
        assert!(codes(&f).contains(&"MOD028"), "{f:?}");
    }

    #[test]
    fn ambiguous_column_names_the_tables() {
        let src = "SELECT note FROM orders o JOIN customers c ON c.id = o.customer_id;";
        let f = run(src);
        let hit = f.iter().find(|f| f.code == "CAT006").expect("CAT006");
        assert_eq!(hit.detail.as_deref(), Some("note is in o and c"));
        assert!(!has(
            "SELECT id FROM orders JOIN customers USING (id);",
            "CAT006"
        ));
        assert!(!has(
            "SELECT o.id FROM orders o, customers c ORDER BY id;",
            "CAT006"
        ));
        assert!(!has("SELECT id FROM orders, (SELECT 1 AS id);", "CAT006"));
    }

    #[test]
    fn insert_mismatch_counts_values_and_names() {
        assert!(has(
            "INSERT INTO customers VALUES (1, 'a', 'b', 'c');",
            "CAT007"
        ));
        assert!(has("INSERT INTO customers (id) VALUES (1, 2);", "CAT007"));
        assert!(has(
            "INSERT INTO customers (id, mail) VALUES (1, 2);",
            "CAT007"
        ));
        assert!(!has("INSERT INTO customers VALUES (1, 'a');", "CAT007"));
        assert!(!has(
            "INSERT INTO customers BY NAME SELECT 1 AS id;",
            "CAT007"
        ));
        // A table the file makes is its own.
        assert!(!has(
            "CREATE TABLE customers (id INT);\nINSERT INTO customers VALUES (1, 2, 3, 4);",
            "CAT007"
        ));
    }

    #[test]
    fn type_fan_out_and_distinct() {
        assert!(has(
            "SELECT 1 FROM orders o JOIN customers c ON c.email = o.id;",
            "CAT008"
        ));
        assert!(!has(
            "SELECT 1 FROM orders o JOIN customers c ON c.id = o.customer_id;",
            "CAT008"
        ));
        assert!(!has("SELECT 1 FROM orders o WHERE o.note = 1;", "CAT008"));
        assert!(has(
            "SELECT * FROM orders o JOIN customers c ON c.note = o.note;",
            "CAT009"
        ));
        assert!(!has(
            "SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id;",
            "CAT009"
        ));
        assert!(has("SELECT DISTINCT id, note FROM orders;", "CAT010"));
        // email is unique but nullable.
        assert!(!has("SELECT DISTINCT email FROM customers;", "CAT010"));
    }

    #[test]
    fn spans_are_file_absolute_on_the_fallback_path() {
        let src = "NOT SQL AT ALL;\nSELECT * FROM orders;";
        let f = run(src);
        let hit = f.iter().find(|f| f.code == "CAT003").expect("CAT003");
        assert_eq!(
            &src[hit.span.start as usize..hit.span.end as usize],
            "orders"
        );
        assert!(hit.detail.is_some());
    }
}
