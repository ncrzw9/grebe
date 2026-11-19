//! `textDocument/completion`: names, read off tokens.
//!
//! Completion is asked for mid-keystroke, when the statement under the
//! cursor usually does not parse (`SELECT o. FROM orders o`), so nothing
//! here needs a tree: the statement's relations, CTEs and the cursor's
//! context are all read from the token stream. That makes it approximate in
//! the harmless direction -- a relation from a nested subquery is offered
//! one level up too -- and never unavailable.
//!
//! What is offered:
//!
//! - after `alias.` / `table.`: that relation's columns;
//! - after `database.` / `schema.` (/ `database.schema.`): what is inside;
//! - where a table goes (`FROM`, `JOIN`, `INTO`, `UPDATE`, ...): tables and
//!   views, CTEs, tables the file creates, schemas and databases;
//! - right after a join's `ON`: the join condition the tables' keys imply
//!   (`o.customer_id = c.id`), when the catalog knows a key of one table
//!   and the other has a column named for it;
//! - in a list of columns (`* EXCLUDE (`, `USING (`, `INSERT INTO t (`,
//!   the name after `AS` in `* REPLACE (`): the columns that can go there,
//!   and nothing else;
//! - anywhere else: the columns of the tables in the query around the
//!   cursor first, then those of the queries around that one and the
//!   aliases, functions, tables, keywords. A column that two of the tables
//!   have is offered once for each, inserted with its table's name.
//!
//! Tables and columns come from the catalog when the server has one (see
//! [`grebe_rules::catalog`]); without one, the statement's own CTEs and the
//! tables the file creates still complete, with keywords and functions.
//!
//! Items are kept short: a column's detail is its type (and the relation,
//! when two have the name); a table has no detail unless its name is in
//! more than one schema or database, when each place is its own item,
//! with `database.schema` as the detail. Keys, sizes and the rest are the
//! hover's (see [`crate::hover`]). An answer carries at most 200 items,
//! the best first, and is marked incomplete when there were more.
//!
//! [`crate::hover`] and [`crate::expand`] read a statement the same way,
//! with the pieces here.

use std::collections::{HashMap, HashSet};

use grebe_rules::catalog::{Catalog, FileContext, Resolved, Table, quote_ident};
use grebe_syntax::token::{Token, TokenKind};

use crate::json::Json;

/// LSP `CompletionItemKind`, the ones used here.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Function = 3,
    Field = 5,
    Variable = 6,
    Class = 7,
    Interface = 8,
    Module = 9,
    Keyword = 14,
    /// A piece of SQL that is more than a name: a join condition.
    Snippet = 15,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Item {
    pub label: String,
    pub kind: Kind,
    pub detail: String,
    /// What to insert, when it is not the label (a name that needs quotes).
    pub insert: Option<String>,
    /// Lower sorts first.
    pub rank: u8,
}

impl Item {
    #[must_use]
    pub fn to_json(&self) -> Json {
        let mut pairs = vec![
            ("label".to_string(), Json::str(&self.label)),
            ("kind".to_string(), Json::num(f64::from(self.kind as u8))),
            (
                "sortText".to_string(),
                Json::str(format!("{}{}", self.rank, self.label.to_ascii_lowercase())),
            ),
        ];
        if !self.detail.is_empty() {
            pairs.push(("detail".to_string(), Json::str(&self.detail)));
        }
        if let Some(i) = &self.insert {
            pairs.push(("insertText".to_string(), Json::str(i)));
        }
        Json::object(pairs)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Completions {
    pub items: Vec<Item>,
    /// More matched than are listed; the client should ask again as the
    /// user types.
    pub incomplete: bool,
}

/// The most items one answer carries.
const LIMIT: usize = 200;

/// Words that end a table reference, so cannot be its alias.
const NOT_AN_ALIAS: &[&str] = &[
    "ANTI",
    "AS",
    "ASOF",
    "AT",
    "BY",
    "CROSS",
    "DEFAULT",
    "DO",
    "EXCEPT",
    "FETCH",
    "FOR",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "INNER",
    "INTERSECT",
    "INTO",
    "JOIN",
    "LATERAL",
    "LEFT",
    "LIMIT",
    "NATURAL",
    "OFFSET",
    "ON",
    "ORDER",
    "PIVOT",
    "POSITIONAL",
    "QUALIFY",
    "RETURNING",
    "RIGHT",
    "SAMPLE",
    "SELECT",
    "SEMI",
    "SET",
    "TABLESAMPLE",
    "UNION",
    "UNPIVOT",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// Keywords offered. A subset on purpose: the ones worth a keystroke.
pub const KEYWORDS: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "ANTI",
    "AS",
    "ASC",
    "ASOF",
    "ATTACH",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "COLUMNS",
    "COPY",
    "CREATE",
    "CROSS",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DETACH",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXCLUDE",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "LAST",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATERIALIZED",
    "MERGE",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLS",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PIVOT",
    "POSITIONAL",
    "PRECEDING",
    "PRIMARY",
    "QUALIFY",
    "RANGE",
    "RECURSIVE",
    "REPLACE",
    "RETURNING",
    "RIGHT",
    "ROWS",
    "SELECT",
    "SEMI",
    "SET",
    "SUMMARIZE",
    "TABLE",
    "TEMP",
    "THEN",
    "TRUE",
    "TRUNCATE",
    "UNBOUNDED",
    "UNION",
    "UNPIVOT",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// Built-in functions offered beside the aggregates and whatever the
/// catalog lists. A short list of the common ones, not DuckDB's two
/// thousand: an editor with a session sends the real list.
const FUNCTIONS: &[&str] = &[
    "abs",
    "ceil",
    "coalesce",
    "concat",
    "contains",
    "current_date",
    "date_add",
    "date_diff",
    "date_part",
    "date_trunc",
    "dense_rank",
    "epoch",
    "first_value",
    "floor",
    "generate_series",
    "glob",
    "greatest",
    "ifnull",
    "json_extract",
    "lag",
    "last_value",
    "lead",
    "least",
    "length",
    "list_filter",
    "list_transform",
    "lower",
    "now",
    "ntile",
    "nullif",
    "range",
    "rank",
    "read_csv",
    "read_json",
    "read_parquet",
    "regexp_extract",
    "regexp_matches",
    "regexp_replace",
    "replace",
    "round",
    "row_number",
    "strftime",
    "strptime",
    "struct_pack",
    "substring",
    "today",
    "trim",
    "try_cast",
    "unnest",
    "upper",
];

pub(crate) fn text<'s>(src: &'s str, t: &Token) -> &'s str {
    &src[t.span.start as usize..t.span.end as usize]
}

pub(crate) fn is_punct(src: &str, t: Option<&Token>, p: &str) -> bool {
    t.is_some_and(|t| t.kind == TokenKind::Punct && text(src, t) == p)
}

pub(crate) fn upper(src: &str, t: Option<&Token>) -> String {
    t.filter(|t| t.kind == TokenKind::Word)
        .map(|t| text(src, t).to_ascii_uppercase())
        .unwrap_or_default()
}

/// A token as a name, one layer of quoting stripped.
pub(crate) fn name_of(src: &str, t: &Token) -> Option<String> {
    let raw = text(src, t);
    match t.kind {
        TokenKind::Word => Some(raw.to_string()),
        TokenKind::QuotedIdent if raw.len() >= 2 && raw.ends_with('"') => {
            Some(raw[1..raw.len() - 1].replace("\"\"", "\""))
        }
        _ => None,
    }
}

pub(crate) fn dotted_name(src: &str, t: &[Token], mut i: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = vec![name_of(src, t.get(i)?)?];
    i += 1;
    while is_punct(src, t.get(i), ".") {
        let Some(next) = t.get(i + 1).and_then(|n| name_of(src, n)) else {
            break;
        };
        parts.push(next);
        i += 2;
    }
    Some((parts, i))
}

pub(crate) fn matching_paren(src: &str, t: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (j, tok) in t.iter().enumerate().skip(open) {
        if tok.kind != TokenKind::Punct {
            continue;
        }
        match text(src, tok) {
            "(" => depth += 1,
            ")" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
    }
    None
}

/// `(a, b)` at `t[open]`, or `None` when it is not a plain list of names.
pub(crate) fn name_list(src: &str, t: &[Token], open: usize) -> Option<(Vec<String>, usize)> {
    let close = matching_paren(src, t, open)?;
    let mut out = Vec::new();
    let mut i = open + 1;
    while i < close {
        out.push(name_of(src, &t[i])?);
        i += 1;
        if i < close {
            if !is_punct(src, t.get(i), ",") {
                return None;
            }
            i += 1;
        }
    }
    Some((out, close + 1))
}

/// What a name in the statement stands for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Target {
    /// A table name as written, parts in order.
    Named(Vec<String>),
    /// Columns spelled out: `t AS a(x, y)`, `WITH c(x, y) AS`.
    Columns(Vec<String>),
    /// A subquery, a function, a file.
    Unknown,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Binding {
    /// What the statement calls it: the alias, or the table's own name.
    /// Empty when it has no name (a subquery without an alias).
    pub(crate) name: String,
    pub(crate) target: Target,
    /// The index of the token the relation starts at.
    pub(crate) at: usize,
    /// The tokens of the table's name, first and one past the last; empty
    /// when the relation is not a table named outright.
    pub(crate) name_tokens: (usize, usize),
    /// The token that is the alias, when there is one.
    pub(crate) alias: Option<usize>,
    /// Written to (`INSERT INTO t`, `UPDATE t`), not read in a FROM.
    pub(crate) written: bool,
}

#[derive(Default, Debug)]
pub(crate) struct Scope {
    pub(crate) bindings: Vec<Binding>,
    pub(crate) ctes: Vec<(String, Option<Vec<String>>)>,
    /// Beside `ctes`: the token each CTE's body opens with, its `(`.
    pub(crate) cte_bodies: Vec<usize>,
}

/// The parentheses of one statement, and the queries they hold. A query is
/// the statement itself or a parenthesis that opens with `SELECT`, `FROM`,
/// `WITH` or `VALUES`; any other parenthesis (a call, a list, a nested
/// join) belongs to the query around it.
pub(crate) struct Blocks {
    /// For each token, and for the end of the statement: the parenthesis
    /// right around it.
    parent: Vec<Option<usize>>,
    /// The same, counting only the parentheses that hold a query.
    query_parent: Vec<Option<usize>>,
    /// For an opening parenthesis, the one that closes it (the end of the
    /// statement when nothing does).
    close: Vec<usize>,
    query: Vec<bool>,
}

/// One query of a statement, between the set operators around it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Block {
    /// The parenthesis it is in; `None` for the statement's own.
    pub(crate) paren: Option<usize>,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl Blocks {
    pub(crate) fn new(src: &str, t: &[Token]) -> Self {
        let n = t.len();
        let mut parent = vec![None; n + 1];
        let mut close = vec![n; n];
        let mut query = vec![false; n];
        let mut open: Vec<usize> = Vec::new();
        for (i, tok) in t.iter().enumerate() {
            parent[i] = open.last().copied();
            if tok.kind != TokenKind::Punct {
                continue;
            }
            match text(src, tok) {
                "(" => {
                    query[i] = matches!(
                        upper(src, t.get(i + 1)).as_str(),
                        "SELECT" | "FROM" | "WITH" | "VALUES"
                    );
                    open.push(i);
                }
                ")" => {
                    if let Some(o) = open.pop() {
                        close[o] = i;
                    }
                }
                _ => {}
            }
        }
        parent[n] = open.last().copied();
        let mut query_parent: Vec<Option<usize>> = vec![None; n + 1];
        for i in 0..=n {
            // A parent is always an earlier token, so its own is known.
            query_parent[i] = match parent[i] {
                Some(p) if query[p] => Some(p),
                Some(p) => query_parent[p],
                None => None,
            };
        }
        Blocks {
            parent,
            query_parent,
            close,
            query,
        }
    }

    /// The parenthesis right around token `i` (or the end of the
    /// statement), of any kind.
    pub(crate) fn parent(&self, i: usize) -> Option<usize> {
        self.parent.get(i).copied().flatten()
    }

    /// Is token `i` inside a call's or a list's parentheses that are
    /// closed? (`extract(year FROM d)`, not a `count(` still being typed.)
    fn in_closed_list(&self, i: usize) -> bool {
        self.parent(i)
            .is_some_and(|p| !self.query[p] && self.close[p] < self.close.len())
    }

    /// Is token `i` part of `block`, in parentheses of its own or not?
    pub(crate) fn member(&self, i: usize, block: &Block) -> bool {
        block.start <= i && i < block.end && self.query_parent[i] == block.paren
    }

    /// Is token `i` part of `block` and in no parentheses within it?
    pub(crate) fn direct(&self, i: usize, block: &Block) -> bool {
        block.start <= i && i < block.end && self.parent[i] == block.paren
    }

    /// The queries around the gap before token `at`, innermost first.
    pub(crate) fn blocks_at(&self, src: &str, t: &[Token], at: usize) -> Vec<Block> {
        let n = t.len();
        let mut out = Vec::new();
        let mut pos = at.min(n);
        let mut paren = self.query_parent[pos];
        loop {
            let (lo, hi) = match paren {
                Some(p) => (p + 1, self.close[p]),
                None => (0, n),
            };
            let (mut start, mut end) = (lo, hi);
            for (i, parent) in self.query_parent.iter().enumerate().take(hi).skip(lo) {
                if *parent != paren
                    || !matches!(
                        upper(src, t.get(i)).as_str(),
                        "UNION" | "INTERSECT" | "EXCEPT"
                    )
                {
                    continue;
                }
                if i < pos {
                    start = i + 1;
                } else {
                    end = i;
                    break;
                }
            }
            out.push(Block { paren, start, end });
            match paren {
                Some(p) => {
                    pos = p;
                    paren = self.query_parent[p];
                }
                None => break,
            }
        }
        out
    }

    /// The relations `block` reads, in the order written: `None` when its
    /// FROM lists something [`scope_of`] did not see as a relation, so that
    /// what it found is not the whole of it.
    pub(crate) fn relations<'a>(
        &self,
        src: &str,
        t: &[Token],
        block: &Block,
        scope: &'a Scope,
    ) -> Option<Vec<&'a Binding>> {
        let mut expected = 0usize;
        let mut regions: Vec<(usize, usize)> = Vec::new();
        let mut from: Option<usize> = None;
        for i in block.start..block.end {
            if !self.direct(i, block) {
                continue;
            }
            let tok = &t[i];
            if tok.kind == TokenKind::Punct {
                if from.is_some() && text(src, tok) == "," {
                    expected += 1;
                }
                continue;
            }
            match upper(src, Some(tok)).as_str() {
                "FROM" if !(i > 0 && upper(src, t.get(i - 1)) == "DISTINCT") => {
                    if let Some(f) = from.replace(i) {
                        regions.push((f, i));
                    }
                    expected += 1;
                }
                "JOIN" if from.is_some() => expected += 1,
                "SELECT" | "WHERE" | "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "OFFSET"
                | "QUALIFY" | "WINDOW" | "SET" | "VALUES" | "RETURNING" | "FETCH" => {
                    if let Some(f) = from.take() {
                        regions.push((f, i));
                    }
                }
                _ => {}
            }
        }
        if let Some(f) = from {
            regions.push((f, block.end));
        }
        let found: Vec<&Binding> = scope
            .bindings
            .iter()
            .filter(|b| !b.written && self.member(b.at, block))
            .collect();
        let read = found
            .iter()
            .filter(|b| regions.iter().any(|(lo, hi)| *lo < b.at && b.at < *hi))
            .count();
        (read == expected && read == found.len()).then_some(found)
    }
}

/// The one catalog table `b` stands for. `None` for anything else: a CTE,
/// a table the file creates, a subquery, a name the catalog does not have
/// or has more than once.
pub(crate) fn table_of<'c>(
    b: &Binding,
    scope: &Scope,
    file: &FileContext,
    catalog: &'c Catalog,
) -> Option<&'c Table> {
    let Target::Named(parts) = &b.target else {
        return None;
    };
    named_table(parts, scope, file, catalog)
}

/// The one catalog table called `parts` here: see [`table_of`].
pub(crate) fn named_table<'c>(
    parts: &[String],
    scope: &Scope,
    file: &FileContext,
    catalog: &'c Catalog,
) -> Option<&'c Table> {
    match named_tables(parts, scope, file, catalog).as_slice() {
        [one] => Some(one),
        _ => None,
    }
}

/// Every catalog table `b` could stand for: one, or each place of a name
/// the catalog has in more than one schema or database. Empty for a CTE,
/// a table the file creates, a subquery, or a name the catalog does not
/// have.
pub(crate) fn tables_of<'c>(
    b: &Binding,
    scope: &Scope,
    file: &FileContext,
    catalog: &'c Catalog,
) -> Vec<&'c Table> {
    match &b.target {
        Target::Named(parts) => named_tables(parts, scope, file, catalog),
        _ => Vec::new(),
    }
}

/// Every catalog table called `parts` here: see [`tables_of`].
pub(crate) fn named_tables<'c>(
    parts: &[String],
    scope: &Scope,
    file: &FileContext,
    catalog: &'c Catalog,
) -> Vec<&'c Table> {
    let lower: Vec<String> = parts.iter().map(|p| p.to_ascii_lowercase()).collect();
    let Some(last) = lower.last() else {
        return Vec::new();
    };
    if lower.len() == 1 && scope.cte(last).is_some() {
        return Vec::new();
    }
    if file.local.contains_key(last) {
        return Vec::new();
    }
    match catalog.resolve(&lower) {
        Resolved::Found(tables) => tables,
        _ => Vec::new(),
    }
}

/// The code tokens of the statement that holds token `at` of `code` (a
/// file's tokens without trivia), and where in them that token is.
pub(crate) fn statement_at(src: &str, code: &[Token], at: usize) -> (Vec<Token>, usize) {
    let is_semi = |t: &Token| t.kind == TokenKind::Punct && text(src, t) == ";";
    let at = at.min(code.len());
    let start = code[..at].iter().rposition(is_semi).map_or(0, |i| i + 1);
    let end = code[at..]
        .iter()
        .position(is_semi)
        .map_or(code.len(), |i| at + i);
    (code[start..end].to_vec(), at - start)
}

/// An alias at `t[i]`: `[AS] name [(cols)]`. `(alias, columns, its token)`.
fn alias_at(src: &str, t: &[Token], mut i: usize) -> Option<(String, Option<Vec<String>>, usize)> {
    let explicit = upper(src, t.get(i)) == "AS";
    if explicit {
        i += 1;
    }
    let tok = t.get(i)?;
    if tok.kind == TokenKind::Word {
        let w = text(src, tok).to_ascii_uppercase();
        if NOT_AN_ALIAS.contains(&w.as_str()) {
            return None;
        }
        if !explicit
            && grebe_syntax::grammar::classify_keyword(text(src, tok))
                .has(grebe_syntax::keyword::Keyword::RESERVED)
        {
            return None;
        }
    }
    let name = name_of(src, tok)?;
    let columns = if is_punct(src, t.get(i + 1), "(") {
        name_list(src, t, i + 1).map(|(c, _)| c)
    } else {
        None
    };
    Some((name, columns, i))
}

/// The relation that starts at `t[i]`, right after `FROM`, `JOIN`, a comma
/// in a FROM list, or a DML keyword. `is_target`: after `INTO`/`UPDATE`,
/// where a `(` after the name is a column list, not a call. Whatever is
/// there is recorded, a thing with no name or no known columns included,
/// so that the scope never holds fewer relations than the statement reads.
fn relation_at(src: &str, t: &[Token], i: usize, is_target: bool, scope: &mut Scope) {
    let Some(first) = t.get(i) else { return };
    let unknown = |name: String, columns: Option<Vec<String>>, alias: Option<usize>| Binding {
        name,
        target: columns.map_or(Target::Unknown, Target::Columns),
        at: i,
        name_tokens: (i, i),
        alias,
        written: is_target,
    };
    // `(subquery) alias`, `'file.csv' alias`.
    if is_punct(src, Some(first), "(") || first.kind == TokenKind::String {
        let after = if first.kind == TokenKind::String {
            Some(i + 1)
        } else {
            matching_paren(src, t, i).map(|c| c + 1)
        };
        scope
            .bindings
            .push(match after.and_then(|a| alias_at(src, t, a)) {
                Some((name, columns, at)) => unknown(name, columns, Some(at)),
                None => unknown(String::new(), None, None),
            });
        return;
    }
    let keyword = first.kind == TokenKind::Word
        && NOT_AN_ALIAS.contains(&text(src, first).to_ascii_uppercase().as_str());
    let Some((parts, next)) = dotted_name(src, t, i).filter(|_| !keyword) else {
        // `LATERAL (...)`, `VALUES ...`, a parameter: something is read
        // here, and what it is called is not known.
        scope.bindings.push(unknown(String::new(), None, None));
        return;
    };
    let last = parts.last().cloned().unwrap_or_default();
    if !is_target && is_punct(src, t.get(next), "(") {
        // A table function: `read_csv('f') r`, `range(3) t(i)`.
        let alias = matching_paren(src, t, next).and_then(|c| alias_at(src, t, c + 1));
        scope.bindings.push(match alias {
            Some((name, columns, at)) => unknown(name, columns, Some(at)),
            None => unknown(last, None, None),
        });
        return;
    }
    let named = |name: String, alias: Option<usize>| Binding {
        name,
        target: Target::Named(parts.clone()),
        at: i,
        name_tokens: (i, next),
        alias,
        written: is_target,
    };
    scope.bindings.push(match alias_at(src, t, next) {
        Some((name, Some(columns), at)) if !is_target => unknown(name, Some(columns), Some(at)),
        Some((name, _, at)) => named(name, Some(at)),
        None => named(last, None),
    });
}

/// `name [(cols)] AS [[NOT] MATERIALIZED] (` at `t[i]`: a CTE head, and
/// the token its body opens with.
fn cte_at(src: &str, t: &[Token], i: usize) -> Option<(String, Option<Vec<String>>, usize)> {
    let name = name_of(src, t.get(i)?)?;
    let mut j = i + 1;
    let mut columns = None;
    if is_punct(src, t.get(j), "(") {
        let (cols, next) = name_list(src, t, j)?;
        columns = Some(cols);
        j = next;
    }
    if upper(src, t.get(j)) != "AS" {
        return None;
    }
    j += 1;
    if upper(src, t.get(j)) == "NOT" {
        j += 1;
    }
    if upper(src, t.get(j)) == "MATERIALIZED" {
        j += 1;
    }
    is_punct(src, t.get(j), "(").then_some((name, columns, j))
}

impl Scope {
    fn add_cte(&mut self, (name, columns, body): (String, Option<Vec<String>>, usize)) {
        self.ctes.push((name, columns));
        self.cte_bodies.push(body);
    }

    /// The CTE called `name` (any case): its columns when spelled out, and
    /// the token its body opens with.
    pub(crate) fn cte(&self, name: &str) -> Option<(Option<&[String]>, usize)> {
        let i = self
            .ctes
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(name))?;
        Some((self.ctes[i].1.as_deref(), self.cte_bodies[i]))
    }
}

/// Every relation and CTE one statement's tokens name.
pub(crate) fn scope_of(src: &str, t: &[Token], blocks: &Blocks) -> Scope {
    let mut scope = Scope::default();
    // The clause each open parenthesis level is in: a comma means "another
    // relation" only inside a FROM list.
    let mut in_from: Vec<bool> = vec![false];
    for i in 0..t.len() {
        let tok = &t[i];
        if tok.kind == TokenKind::Punct {
            match text(src, tok) {
                "(" => in_from.push(false),
                ")" => {
                    if in_from.len() > 1 {
                        in_from.pop();
                    }
                }
                "," => {
                    if in_from.last() == Some(&true) {
                        relation_at(src, t, i + 1, false, &mut scope);
                    }
                    if let Some(cte) = cte_at(src, t, i + 1) {
                        scope.add_cte(cte);
                    }
                }
                _ => {}
            }
            continue;
        }
        if tok.kind != TokenKind::Word {
            continue;
        }
        let depth = in_from.len() - 1;
        match text(src, tok).to_ascii_uppercase().as_str() {
            "WITH" => {
                let at = if upper(src, t.get(i + 1)) == "RECURSIVE" {
                    i + 2
                } else {
                    i + 1
                };
                if let Some(cte) = cte_at(src, t, at) {
                    scope.add_cte(cte);
                }
            }
            "FROM" => {
                // `x IS DISTINCT FROM y` is not a FROM clause.
                if i > 0 && upper(src, t.get(i - 1)) == "DISTINCT" {
                    continue;
                }
                // Nor is the one in `extract(year FROM d)`.
                if blocks.in_closed_list(i) {
                    continue;
                }
                in_from[depth] = true;
                relation_at(src, t, i + 1, false, &mut scope);
            }
            "JOIN" => {
                in_from[depth] = true;
                relation_at(src, t, i + 1, false, &mut scope);
            }
            "INTO" | "UPDATE" => relation_at(src, t, i + 1, true, &mut scope),
            // `DELETE FROM t USING u`, `MERGE INTO t USING u`; a join's
            // `USING (a, b)` is a column list.
            "USING" => {
                let sub = is_punct(src, t.get(i + 1), "(")
                    && matches!(
                        upper(src, t.get(i + 2)).as_str(),
                        "SELECT" | "WITH" | "VALUES" | "FROM"
                    );
                let sample = upper(src, t.get(i + 1)) == "SAMPLE";
                if sub || !(sample || is_punct(src, t.get(i + 1), "(")) {
                    relation_at(src, t, i + 1, false, &mut scope);
                }
            }
            "SELECT" | "WHERE" | "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "QUALIFY" | "WINDOW"
            | "ON" | "SET" | "VALUES" | "RETURNING" | "UNION" | "EXCEPT" | "INTERSECT" => {
                in_from[depth] = false;
            }
            _ => {}
        }
    }
    scope
}

/// Where the cursor is.
struct Cursor<'s> {
    /// The statement's code tokens, the word being typed left out.
    statement: Vec<Token>,
    /// How many of `statement`'s tokens are before the cursor.
    before: usize,
    /// What has been typed of the name at the cursor.
    prefix: &'s str,
}

/// `None` inside a string or a comment, where nothing completes.
fn cursor(src: &str, offset: u32) -> Option<Cursor<'_>> {
    let tokens = grebe_syntax::token::tokenize(src);
    let mut prefix = "";
    let mut code: Vec<Token> = Vec::new();
    let mut before = 0usize;
    for t in &tokens {
        let inside = t.span.start < offset && offset < t.span.end;
        let at_end = t.span.start < offset && offset == t.span.end;
        match t.kind {
            TokenKind::String | TokenKind::BlockComment if inside => return None,
            TokenKind::LineComment if inside || at_end => return None,
            // The name being typed: what is left of the cursor is the
            // prefix, and the token itself is not part of the statement.
            TokenKind::Word | TokenKind::QuotedIdent | TokenKind::Number if inside || at_end => {
                let typed = &src[t.span.start as usize..offset as usize];
                // `1.` is a number being typed, not a qualifier.
                if t.kind == TokenKind::Number {
                    return None;
                }
                prefix = typed.strip_prefix('"').unwrap_or(typed);
                continue;
            }
            _ => {}
        }
        if t.kind.is_trivia() {
            continue;
        }
        if t.span.end <= offset {
            before += 1;
        }
        code.push(*t);
    }
    // The statement around the cursor: from the `;` before it to the next.
    let is_semi = |t: &Token| t.kind == TokenKind::Punct && text(src, t) == ";";
    let start = code[..before]
        .iter()
        .rposition(is_semi)
        .map_or(0, |i| i + 1);
    let end = code[before..]
        .iter()
        .position(is_semi)
        .map_or(code.len(), |i| before + i);
    Some(Cursor {
        statement: code[start..end].to_vec(),
        before: before - start,
        prefix,
    })
}

struct Collector {
    /// What is typed so far, lowercase: only names that start with it are
    /// kept.
    prefix: String,
    seen: HashSet<(String, Kind)>,
    /// The items, each with what `seen` knows it by.
    items: Vec<(Item, String)>,
    /// Some names were not looked at: there were more than an answer
    /// carries.
    incomplete: bool,
    /// Once more than an answer carries have been found, the last one that
    /// still makes it: anything that sorts after it is not worth building.
    /// A table of a hundred thousand columns then costs about what one of
    /// a few thousand does.
    cutoff: Option<(u8, String)>,
}

impl Collector {
    fn matches(&self, name: &str) -> bool {
        let n = self.prefix.len();
        !name.is_empty()
            && name.len() >= n
            && name.as_bytes()[..n].eq_ignore_ascii_case(self.prefix.as_bytes())
    }

    /// Would `name`, ranked `rank`, be in the answer: does it match what
    /// is typed, and sort before the cutoff?
    fn wants(&self, name: &str, rank: u8) -> bool {
        self.matches(name)
            && self.cutoff.as_ref().is_none_or(|(r, last)| {
                rank.cmp(r)
                    .then_with(|| {
                        name.bytes()
                            .map(|b| b.to_ascii_lowercase())
                            .cmp(last.bytes())
                    })
                    .is_le()
            })
    }

    fn push(&mut self, item: Item, key: String) {
        self.items.push((item, key));
        if self.items.len() < 4 * LIMIT {
            return;
        }
        // Keep the best, and forget the rest, so a name dropped here can
        // still come back with a better rank.
        sort(&mut self.items);
        for (dropped, key) in self.items.drain(LIMIT..) {
            self.seen.remove(&(key, dropped.kind));
        }
        self.incomplete = true;
        let (last, _) = &self.items[LIMIT - 1];
        self.cutoff = Some((last.rank, last.label.to_ascii_lowercase()));
    }

    fn add(&mut self, name: &str, kind: Kind, detail: impl Into<String>, rank: u8) {
        if !self.wants(name, rank) {
            return;
        }
        let key = name.to_ascii_lowercase();
        if !self.seen.insert((key.clone(), kind)) {
            return;
        }
        let quoted = match kind {
            Kind::Keyword | Kind::Function | Kind::Snippet => name.to_string(),
            _ => quote_ident(name),
        };
        let item = Item {
            label: name.to_string(),
            kind,
            detail: detail.into(),
            insert: (quoted != name).then_some(quoted),
            rank,
        };
        self.push(item, key);
    }

    /// A column that more than one relation in reach has: listed under its
    /// own name, inserted as `relation.column`.
    fn qualified(&mut self, relation: &str, name: &str, detail: String, rank: u8) {
        if !self.wants(name, rank) {
            return;
        }
        let insert = format!("{}.{}", quote_ident(relation), quote_ident(name));
        let key = insert.to_ascii_lowercase();
        if !self.seen.insert((key.clone(), Kind::Field)) {
            return;
        }
        let item = Item {
            label: name.to_string(),
            kind: Kind::Field,
            detail,
            insert: Some(insert),
            rank,
        };
        self.push(item, key);
    }

    /// The catalog's tables that match what is typed, within `within` (see
    /// [`Catalog::tables_with_prefix`]). The catalog finds them by its
    /// index and hands over no more than an answer can carry, so a catalog
    /// of a hundred thousand tables costs what a small one does.
    ///
    /// A name in one place is one item with no detail. A name in several
    /// is an item for each place, with `database.schema` as the detail,
    /// inserted with as much of that as tells the places apart (none of
    /// it when a qualifier is typed already, unless it is the database).
    fn tables(&mut self, catalog: &Catalog, within: &[String], rank: u8) {
        let (found, cut) = catalog.tables_with_prefix(&self.prefix, within, LIMIT + 1);
        self.incomplete |= cut;
        let mut places: HashMap<String, usize> = HashMap::new();
        let mut schemas: HashMap<(String, String), usize> = HashMap::new();
        for t in &found {
            *places.entry(t.name.to_ascii_lowercase()).or_default() += 1;
            *schemas
                .entry((t.schema.to_ascii_lowercase(), t.name.to_ascii_lowercase()))
                .or_default() += 1;
        }
        let typed_database =
            matches!(within, [q] if catalog.has_database(q) && !catalog.has_schema(q));
        for t in found {
            let kind = if t.is_view {
                Kind::Interface
            } else {
                Kind::Class
            };
            let lname = t.name.to_ascii_lowercase();
            if places.get(&lname).is_none_or(|n| *n < 2) {
                self.add(&t.name, kind, "", rank);
                continue;
            }
            if !self.wants(&t.name, rank) {
                continue;
            }
            let name = quote_ident(&t.name);
            let insert = if within.is_empty() || typed_database {
                let schema = format!("{}.{name}", quote_ident(&t.schema));
                let shared = schemas
                    .get(&(t.schema.to_ascii_lowercase(), lname))
                    .is_some_and(|n| *n > 1);
                if shared && !typed_database {
                    format!("{}.{schema}", quote_ident(&t.database))
                } else {
                    schema
                }
            } else {
                name
            };
            let key = format!("{}.{}.{}", t.database, t.schema, t.name).to_ascii_lowercase();
            if !self.seen.insert((key.clone(), kind)) {
                continue;
            }
            let item = Item {
                label: t.name.clone(),
                kind,
                detail: format!("{}.{}", t.database, t.schema),
                insert: (insert != t.name).then_some(insert),
                rank,
            };
            self.push(item, key);
        }
    }
}

/// Every column whose name starts with `prefix` of whatever `parts` names
/// (a CTE, a table the file creates, or a catalog table), with its type
/// when known. A catalog table's
/// are taken from its name index, in name order, and no more than an answer
/// carries; true when some were left out for that.
fn each_column_named<'a>(
    parts: &[String],
    prefix: &'a str,
    scope: &'a Scope,
    file: &'a FileContext,
    catalog: Option<&'a Catalog>,
    f: &mut dyn FnMut(&'a str, &'a str),
) -> bool {
    let lower: Vec<String> = parts.iter().map(|p| p.to_ascii_lowercase()).collect();
    let Some(last) = lower.last() else {
        return false;
    };
    if lower.len() == 1 {
        if let Some((cols, _)) = scope.cte(last) {
            for c in cols.into_iter().flatten() {
                f(c, "");
            }
            return false;
        }
    }
    if let Some(local) = file.local.get(last) {
        for c in local.columns.iter().flatten() {
            f(c, "");
        }
        return false;
    }
    let mut cut = false;
    if let Some(Resolved::Found(tables)) = catalog.map(|c| c.resolve(&lower)) {
        for t in tables {
            for (n, c) in t.columns_with_prefix(prefix).enumerate() {
                if n == LIMIT {
                    cut = true;
                    break;
                }
                f(&c.name, &c.data_type);
            }
        }
    }
    cut
}

/// Every column of the relation `b` whose name starts with `prefix`: see
/// [`each_column_named`].
fn each_column<'a>(
    b: &'a Binding,
    prefix: &'a str,
    scope: &'a Scope,
    file: &'a FileContext,
    catalog: Option<&'a Catalog>,
    f: &mut dyn FnMut(&'a str, &'a str),
) -> bool {
    match &b.target {
        Target::Named(parts) => each_column_named(parts, prefix, scope, file, catalog, f),
        Target::Columns(cols) => {
            for c in cols {
                f(c, "");
            }
            false
        }
        Target::Unknown => false,
    }
}

/// Does the relation `b` have a column `name`? `None` when its columns are
/// not known. Looked up, not listed: a wide table costs what a narrow one
/// does.
fn relation_has(
    b: &Binding,
    name: &str,
    scope: &Scope,
    file: &FileContext,
    catalog: Option<&Catalog>,
) -> Option<bool> {
    let has = |cols: &[String]| cols.iter().any(|c| c.eq_ignore_ascii_case(name));
    match &b.target {
        Target::Columns(cols) => Some(has(cols)),
        Target::Unknown => None,
        Target::Named(parts) => {
            let lower: Vec<String> = parts.iter().map(|p| p.to_ascii_lowercase()).collect();
            let last = lower.last()?;
            if lower.len() == 1 {
                if let Some((cols, _)) = scope.cte(last) {
                    return cols.map(has);
                }
            }
            if let Some(local) = file.local.get(last) {
                return local.columns.as_deref().map(has);
            }
            match catalog?.resolve(&lower) {
                Resolved::Found(tables) if tables.iter().any(|t| !t.columns.is_empty()) => {
                    Some(tables.iter().any(|t| t.has_column(name)))
                }
                _ => None,
            }
        }
    }
}

/// The columns of whatever `parts` names, as items.
fn columns_named(
    out: &mut Collector,
    parts: &[String],
    scope: &Scope,
    file: &FileContext,
    catalog: Option<&Catalog>,
    rank: u8,
) {
    let prefix = out.prefix.clone();
    let cut = each_column_named(parts, &prefix, scope, file, catalog, &mut |name, ty| {
        if out.wants(name, rank) {
            out.add(name, Kind::Field, ty.to_string(), rank);
        }
    });
    out.incomplete |= cut;
}

/// The columns of the relation `b`, as items.
fn columns_of(
    out: &mut Collector,
    b: &Binding,
    scope: &Scope,
    file: &FileContext,
    catalog: Option<&Catalog>,
    rank: u8,
) {
    let prefix = out.prefix.clone();
    let cut = each_column(b, &prefix, scope, file, catalog, &mut |name, ty| {
        if out.wants(name, rank) {
            out.add(name, Kind::Field, ty.to_string(), rank);
        }
    });
    out.incomplete |= cut;
}

/// What a table's name could be in the name of a column that points at it:
/// `orders` gives `orders` and `order`, `categories` also `category`.
fn stems(table: &str) -> Vec<String> {
    let name = table.to_ascii_lowercase();
    let mut out = vec![name.clone()];
    if let Some(s) = name.strip_suffix("ies") {
        out.push(format!("{s}y"));
    }
    if let Some(s) = name.strip_suffix("es") {
        out.push(s.to_string());
    }
    if let Some(s) = name.strip_suffix('s') {
        out.push(s.to_string());
    }
    out
}

/// The join conditions the keys of two tables imply. A key of one column `k` of a table `things` pairs with a
/// column `things_k` or `thing_k` of the other table, or with one called
/// `k` too unless that is a bare `id`, which every table has for itself.
/// A key of several columns pairs with the columns of the same names.
/// `left` is the relation written first; it is written first in the
/// condition too.
fn join_conditions(left: (&str, &Table), right: (&str, &Table)) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let side =
        |alias: &str, column: &str| format!("{}.{}", quote_ident(alias), quote_ident(column));
    for (keyed, other, keyed_is_left) in [(left, right, true), (right, left, false)] {
        let (ka, kt) = keyed;
        let (oa, ot) = other;
        for key in kt.unique_keys() {
            let pairs: Option<Vec<(&str, &str)>> = if let [k] = key {
                let named_for = stems(&kt.name)
                    .iter()
                    .find_map(|s| ot.column(&format!("{s}_{k}")));
                let same = || ot.column(k).filter(|_| !k.eq_ignore_ascii_case("id"));
                named_for
                    .or_else(same)
                    .map(|c| vec![(k.as_str(), c.name.as_str())])
            } else {
                key.iter()
                    .map(|k| ot.column(k).map(|c| (k.as_str(), c.name.as_str())))
                    .collect()
            };
            let Some(pairs) = pairs else { continue };
            let condition = pairs
                .iter()
                .map(|(k, o)| {
                    if keyed_is_left {
                        format!("{} = {}", side(ka, k), side(oa, o))
                    } else {
                        format!("{} = {}", side(oa, o), side(ka, k))
                    }
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            if !out.contains(&condition) {
                out.push(condition);
            }
        }
    }
    out
}

/// The names already written in the parentheses that open at `open`.
fn listed(src: &str, t: &[Token], open: usize, blocks: &Blocks) -> HashSet<String> {
    t.iter()
        .enumerate()
        .skip(open + 1)
        .filter(|(i, _)| blocks.parent(*i) == Some(open))
        .filter_map(|(_, tok)| name_of(src, tok))
        .map(|n| n.to_ascii_lowercase())
        .collect()
}

/// Complete at byte `offset` of `src`.
#[must_use]
pub fn complete(src: &str, offset: u32, catalog: Option<&Catalog>) -> Completions {
    let Some(cur) = cursor(src, offset) else {
        return Completions::default();
    };
    let t = &cur.statement;
    let blocks = Blocks::new(src, t);
    let scope = scope_of(src, t, &blocks);
    let file = FileContext::scan(src);
    let mut out = Collector {
        prefix: cur.prefix.to_ascii_lowercase(),
        seen: HashSet::new(),
        items: Vec::new(),
        incomplete: false,
        cutoff: None,
    };

    // `a.b.` before the cursor: the qualifier chain, outermost first.
    let mut chain: Vec<String> = Vec::new();
    let mut at = cur.before;
    while at >= 2 && is_punct(src, t.get(at - 1), ".") {
        let Some(name) = name_of(src, &t[at - 2]) else {
            break;
        };
        chain.insert(0, name);
        at -= 2;
    }
    // A dot with nothing nameable before it (`1.`, `).`): nothing to offer.
    if chain.is_empty() && at >= 1 && is_punct(src, t.get(at - 1), ".") {
        return Completions::default();
    }

    if !chain.is_empty() {
        let lower: Vec<String> = chain.iter().map(|p| p.to_ascii_lowercase()).collect();
        if let [q] = lower.as_slice() {
            // An alias or a relation of the statement.
            let mut bound = false;
            for b in scope
                .bindings
                .iter()
                .filter(|b| b.name.eq_ignore_ascii_case(q))
            {
                bound = true;
                columns_of(&mut out, b, &scope, &file, catalog, 0);
            }
            if !bound {
                // A table named outright (`orders.` before its FROM is
                // typed), a CTE, a local table.
                columns_named(&mut out, &chain, &scope, &file, catalog, 0);
            }
            if let Some(cat) = catalog {
                // A database's schemas and tables; a schema's tables.
                for (schema, database) in cat.schemas() {
                    if database.eq_ignore_ascii_case(q) {
                        out.add(schema, Kind::Module, "", 1);
                    }
                }
                out.tables(cat, &lower, 2);
            }
        } else {
            // `schema.table.` / `db.schema.table.`: columns;
            // `db.schema.`: tables.
            columns_named(&mut out, &chain, &scope, &file, catalog, 0);
            if let (Some(cat), [_, _]) = (catalog, lower.as_slice()) {
                out.tables(cat, &lower, 2);
            }
        }
        return finish(out);
    }

    // The queries around the cursor, innermost first, and the relations of
    // each.
    let around = blocks.blocks_at(src, t, at);
    let within = |block: &Block| -> Vec<&Binding> {
        scope
            .bindings
            .iter()
            .filter(|b| blocks.member(b.at, block))
            .collect()
    };
    let here = around.first().map(&within).unwrap_or_default();

    // A list of columns: only the columns that can be in it, less the ones
    // already there.
    if let Some(open) = blocks.parent(at) {
        let before = upper(src, open.checked_sub(1).and_then(|i| t.get(i)));
        let already = listed(src, t, open, &blocks);
        let prefix = out.prefix.clone();
        let offer = |out: &mut Collector, from: &[&Binding]| {
            for b in from {
                let cut = each_column(b, &prefix, &scope, &file, catalog, &mut |name, ty| {
                    if out.wants(name, 0) && !already.contains(&name.to_ascii_lowercase()) {
                        out.add(name, Kind::Field, ty.to_string(), 0);
                    }
                });
                out.incomplete |= cut;
            }
        };
        let star = open >= 2 && text(src, &t[open - 2]) == "*";
        // `* REPLACE (expr AS |`: the name is one of the star's columns.
        // Before the `AS` it is an expression, which completes as usual.
        let replaces = before == "REPLACE"
            && star
            && upper(src, at.checked_sub(1).and_then(|i| t.get(i))) == "AS";
        if (before == "EXCLUDE" || replaces) && star {
            // `* EXCLUDE (`: every relation's; `o.* EXCLUDE (`: `o`'s.
            let of = (open >= 4 && is_punct(src, t.get(open - 3), "."))
                .then(|| name_of(src, &t[open - 4]))
                .flatten();
            let from: Vec<&Binding> = match &of {
                Some(q) => scope
                    .bindings
                    .iter()
                    .filter(|b| b.name.eq_ignore_ascii_case(q))
                    .collect(),
                None => here.clone(),
            };
            offer(&mut out, &from);
            return finish(out);
        }
        let subquery = matches!(
            upper(src, t.get(open + 1)).as_str(),
            "SELECT" | "WITH" | "VALUES" | "FROM"
        );
        if before == "USING" && !subquery {
            // The columns the joined relation shares with the ones before
            // it; all of its own when theirs are not known.
            let earlier: Vec<&Binding> = here.iter().copied().filter(|b| b.at < open).collect();
            if let Some((joined, rest)) = earlier.split_last() {
                // Whether the relations before it have a name, when every
                // one of them has known columns.
                let known = !rest.is_empty()
                    && rest
                        .iter()
                        .all(|b| relation_has(b, "", &scope, &file, catalog).is_some());
                let theirs = |name: &str| {
                    rest.iter()
                        .any(|b| relation_has(b, name, &scope, &file, catalog) == Some(true))
                };
                let cut = each_column(joined, &prefix, &scope, &file, catalog, &mut |name, ty| {
                    if out.wants(name, 0)
                        && !already.contains(&name.to_ascii_lowercase())
                        && (!known || theirs(name))
                    {
                        out.add(name, Kind::Field, ty.to_string(), 0);
                    }
                });
                out.incomplete |= cut;
                if relation_has(joined, "", &scope, &file, catalog).is_none() {
                    offer(&mut out, rest);
                }
            }
            return finish(out);
        }
        // `INSERT INTO t (`: `t`'s. Anything but a column typed here (a
        // `SELECT` in parentheses) completes as usual.
        if let Some(target) = scope.bindings.iter().find(|b| {
            b.written
                && b.name_tokens.1 == open
                && upper(src, b.at.checked_sub(1).and_then(|i| t.get(i))) == "INTO"
        }) {
            offer(&mut out, &[target]);
            if !out.items.is_empty() {
                return finish(out);
            }
        }
    }

    // Is a table expected here? After a keyword that takes one, or after a
    // comma in a FROM list.
    let prev = upper(src, at.checked_sub(1).and_then(|i| t.get(i)));
    let mut table_context = matches!(
        prev.as_str(),
        "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" | "DESCRIBE" | "SUMMARIZE" | "TRUNCATE"
    );
    if !table_context && at >= 1 && is_punct(src, t.get(at - 1), ",") {
        let mut depth = 0usize;
        for tok in t[..at - 1].iter().rev() {
            if tok.kind == TokenKind::Punct {
                match text(src, tok) {
                    ")" => depth += 1,
                    "(" => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
            } else if depth == 0 && tok.kind == TokenKind::Word {
                match text(src, tok).to_ascii_uppercase().as_str() {
                    "FROM" => {
                        table_context = true;
                        break;
                    }
                    "SELECT" | "WHERE" | "GROUP" | "HAVING" | "ORDER" | "ON" | "SET" | "VALUES"
                    | "BY" | "USING" | "RETURNING" => break,
                    _ => {}
                }
            }
        }
    }

    // Right after a join's `ON`: the condition the keys imply, between the
    // relation just joined and each one before it.
    if let (true, Some(cat)) = (prev == "ON", catalog) {
        let earlier: Vec<&Binding> = here.iter().copied().filter(|b| b.at < at).collect();
        if let Some((joined, rest)) = earlier.split_last() {
            let is_join = upper(src, joined.at.checked_sub(1).and_then(|i| t.get(i))) == "JOIN";
            if let (true, Some(right)) = (is_join, table_of(joined, &scope, &file, cat)) {
                for b in rest {
                    let Some(left) = table_of(b, &scope, &file, cat) else {
                        continue;
                    };
                    for condition in join_conditions((&b.name, left), (&joined.name, right)) {
                        out.add(&condition, Kind::Snippet, "join", 0);
                    }
                }
            }
        }
    }

    let tables_rank = if table_context { 0 } else { 5 };
    if !table_context {
        // The columns in reach: the query's own relations first, then those
        // of the queries around it. A name more than one of them has is
        // offered for each, with the relation it is from.
        let reach: Vec<(&Binding, u8)> = around
            .iter()
            .enumerate()
            .flat_map(|(n, block)| {
                within(block)
                    .into_iter()
                    .map(move |b| (b, if n == 0 { 1 } else { 2 }))
            })
            .collect();
        // A name another relation in reach has too is offered once for
        // each, inserted with the relation's name. Looked up, not listed.
        let prefix = out.prefix.clone();
        for (b, rank) in &reach {
            let cut = each_column(b, &prefix, &scope, &file, catalog, &mut |name, ty| {
                if !out.wants(name, *rank) {
                    return;
                }
                let shared = !b.name.is_empty()
                    && reach.iter().any(|(other, _)| {
                        !std::ptr::eq(*other, *b)
                            && relation_has(other, name, &scope, &file, catalog) == Some(true)
                    });
                let detail = ty.to_string();
                if shared {
                    let detail = if detail.is_empty() {
                        b.name.clone()
                    } else {
                        format!("{detail} · {}", b.name)
                    };
                    out.qualified(&b.name, name, detail, *rank);
                } else {
                    out.add(name, Kind::Field, detail, *rank);
                }
            });
            out.incomplete |= cut;
        }
        for b in &scope.bindings {
            out.add(&b.name, Kind::Variable, "", 2);
        }
        for a in grebe_rules::detect::aggregate_names() {
            out.add(a, Kind::Function, "", 4);
        }
        for f in FUNCTIONS {
            out.add(f, Kind::Function, "", 4);
        }
        if let Some(cat) = catalog {
            for f in cat.functions() {
                out.add(&f.name, Kind::Function, "", 4);
            }
        }
    }
    for (name, _) in &scope.ctes {
        out.add(name, Kind::Class, "", tables_rank);
    }
    let mut local: Vec<_> = file.local.values().collect();
    local.sort_by(|a, b| a.name.cmp(&b.name));
    for l in local {
        out.add(&l.name, Kind::Class, "", tables_rank);
    }
    if let Some(cat) = catalog {
        out.tables(cat, &[], tables_rank);
        for (schema, _) in cat.schemas() {
            out.add(schema, Kind::Module, "", tables_rank + 1);
        }
        for database in cat.databases() {
            out.add(database, Kind::Module, "", tables_rank + 1);
        }
        if table_context {
            for f in cat.functions().filter(|f| f.kind.contains("table")) {
                out.add(&f.name, Kind::Function, "", 2);
            }
        }
    }
    for k in KEYWORDS {
        out.add(k, Kind::Keyword, "", 7);
    }
    finish(out)
}

/// Best first: by rank, then by name.
fn sort(items: &mut [(Item, String)]) {
    items.sort_by_cached_key(|(i, _)| (i.rank, i.label.to_ascii_lowercase()));
}

fn finish(out: Collector) -> Completions {
    let mut items = out.items;
    sort(&mut items);
    let incomplete = out.incomplete || items.len() > LIMIT;
    items.truncate(LIMIT);
    Completions {
        items: items.into_iter().map(|(i, _)| i).collect(),
        incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::from_json_text(
            r#"[
            {"database":"memory","schema":"main","table":"orders","column":"id","type":"BIGINT"},
            {"database":"memory","schema":"main","table":"orders","column":"customer_id","type":"BIGINT"},
            {"database":"memory","schema":"main","table":"orders","column":"Order Date","type":"DATE"},
            {"database":"memory","schema":"main","table":"customers","column":"id","type":"BIGINT"},
            {"database":"memory","schema":"main","table":"customers","column":"name","type":"VARCHAR"},
            {"database":"lake","schema":"raw","table":"events","column":"ts","type":"TIMESTAMP","kind":"view"}
            ]"#,
        )
        .unwrap()
    }

    /// Labels offered at the `|` in `sql`.
    fn at(sql: &str, cat: Option<&Catalog>) -> Vec<String> {
        let offset = sql.find('|').expect("a | marks the cursor");
        let src = sql.replacen('|', "", 1);
        complete(&src, offset as u32, cat)
            .items
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    #[test]
    fn an_alias_offers_its_tables_columns_even_when_the_statement_does_not_parse() {
        let cat = catalog();
        let got = at(
            "SELECT o.| FROM orders o JOIN customers c ON o.customer_id = c.id",
            Some(&cat),
        );
        assert_eq!(got, ["customer_id", "id", "Order Date"]);
        let got = at(
            "SELECT c.n| FROM orders o JOIN customers AS c ON 1",
            Some(&cat),
        );
        assert_eq!(got, ["name"]);
    }

    #[test]
    fn a_name_that_needs_quotes_is_inserted_quoted() {
        let cat = catalog();
        let offset = "SELECT o.".len() as u32;
        let found = complete("SELECT o. FROM orders o", offset, Some(&cat));
        let date = found
            .items
            .iter()
            .find(|i| i.label == "Order Date")
            .unwrap();
        assert_eq!(date.insert.as_deref(), Some("\"Order Date\""));
        assert_eq!(date.detail, "DATE");
    }

    #[test]
    fn after_from_tables_come_first() {
        let cat = catalog();
        let got = at("SELECT * FROM |", Some(&cat));
        assert_eq!(&got[..3], ["customers", "events", "orders"]);
        let got = at("SELECT * FROM orders o, c|", Some(&cat));
        assert_eq!(got[0], "customers");
        let got = at("SELECT * FROM orders JOIN cu|", Some(&cat));
        assert_eq!(got, ["customers"]);
    }

    #[test]
    fn a_database_or_schema_offers_what_is_in_it() {
        let cat = catalog();
        assert_eq!(at("FROM lake.|", Some(&cat)), ["raw", "events"]);
        assert_eq!(at("FROM lake.raw.|", Some(&cat)), ["events"]);
        assert_eq!(at("FROM raw.|", Some(&cat)), ["events"]);
        assert_eq!(
            at("SELECT lake.raw.events.| FROM lake.raw.events", Some(&cat)),
            ["ts"]
        );
    }

    #[test]
    fn an_expression_offers_the_scopes_columns_before_anything_else() {
        let cat = catalog();
        let got = at("SELECT | FROM customers", Some(&cat));
        assert_eq!(&got[..3], ["id", "name", "customers"]);
        // Aggregates and keywords come after, as what is typed narrows it.
        assert!(at("SELECT su| FROM customers", Some(&cat)).contains(&"sum".to_string()));
        assert!(at("SELECT se| FROM customers", Some(&cat)).contains(&"SELECT".to_string()));
        // Only what the prefix matches.
        assert_eq!(
            at("SELECT na| FROM customers", Some(&cat)),
            ["name", "NATURAL"]
        );
    }

    #[test]
    fn works_without_a_catalog() {
        let got = at(
            "CREATE TABLE staging (id INT, label VARCHAR, PRIMARY KEY (id));\n\
             WITH recent(day, n) AS (SELECT 1, 2) SELECT r.| FROM recent r",
            None,
        );
        assert_eq!(got, ["day", "n"]);
        let got = at(
            "CREATE TABLE staging (id INT, label VARCHAR, PRIMARY KEY (id));\nSELECT s.| FROM staging s",
            None,
        );
        assert_eq!(got, ["id", "label"]);
        let got = at("WITH recent AS (SELECT 1) SELECT * FROM re|", None);
        assert_eq!(got, ["recent", "RECURSIVE", "REPLACE", "RETURNING"]);
    }

    #[test]
    fn renamed_columns_and_functions() {
        let cat = catalog();
        assert_eq!(
            at("SELECT a.| FROM orders AS a(x, y)", Some(&cat)),
            ["x", "y"]
        );
        assert_eq!(at("SELECT t.| FROM range(3) t(i)", Some(&cat)), ["i"]);
        // A subquery's columns are not known; nothing wrong is offered.
        assert!(at("SELECT s.| FROM (SELECT 1 AS one) s", Some(&cat)).is_empty());
    }

    #[test]
    fn dml_targets_are_in_scope() {
        let cat = catalog();
        assert_eq!(at("UPDATE customers SET n|", Some(&cat))[0], "name");
        assert_eq!(
            at("UPDATE customers c SET name = 'x' WHERE c.|", Some(&cat)),
            ["id", "name"]
        );
        assert_eq!(
            at(
                "DELETE FROM orders o USING customers c WHERE c.|",
                Some(&cat)
            ),
            ["id", "name"]
        );
        assert_eq!(
            at("INSERT INTO customers (|", Some(&cat))[..2],
            ["id", "name"]
        );
    }

    #[test]
    fn only_the_statement_under_the_cursor_is_in_scope() {
        let cat = catalog();
        let got = at(
            "SELECT * FROM orders o;\nSELECT o.| FROM customers o;",
            Some(&cat),
        );
        assert_eq!(got, ["id", "name"]);
    }

    #[test]
    fn nothing_inside_strings_comments_or_numbers() {
        let cat = catalog();
        assert!(at("SELECT 'o.|' FROM orders o", Some(&cat)).is_empty());
        assert!(at("SELECT 1 -- o.|\nFROM orders o", Some(&cat)).is_empty());
        assert!(at("SELECT 1.| FROM orders o", Some(&cat)).is_empty());
    }

    #[test]
    fn every_offered_keyword_is_a_keyword() {
        for k in KEYWORDS {
            assert!(
                !grebe_syntax::grammar::classify_keyword(k).is_none(),
                "{k} is not a DuckDB keyword"
            );
        }
    }

    fn shop() -> Catalog {
        Catalog::from_json_text(crate::hover::tests::SHOP).unwrap()
    }

    /// `label [detail]` of the items at the `|`, the first `n`.
    fn shown(sql: &str, cat: Option<&Catalog>, n: usize) -> Vec<String> {
        let offset = sql.find('|').expect("a | marks the cursor");
        let src = sql.replacen('|', "", 1);
        complete(&src, offset as u32, cat)
            .items
            .into_iter()
            .take(n)
            .map(|i| match i.insert {
                Some(insert) => format!("{} [{}] {insert}", i.label, i.detail),
                None => format!("{} [{}]", i.label, i.detail),
            })
            .collect()
    }

    #[test]
    fn items_are_a_name_and_a_type() {
        let cat = shop();
        // A table: its name, nothing else, when the name is in one place.
        assert_eq!(
            shown("SELECT * FROM ord|", Some(&cat), 2),
            ["order_items []", "orders []"]
        );
        assert_eq!(shown("FROM big|", Some(&cat), 1), ["big_orders []"]);
        // A column: its type.
        assert_eq!(
            shown("SELECT | FROM order_items", Some(&cat), 4),
            [
                "line_no [INTEGER]",
                "order_id [BIGINT]",
                "qty [INTEGER]",
                "sku [VARCHAR]",
            ]
        );
        assert_eq!(
            shown("SELECT em| FROM customers", Some(&cat), 1),
            ["email [VARCHAR]"]
        );
        assert_eq!(
            shown("SELECT pl| FROM orders", Some(&cat), 1),
            ["placed [TIMESTAMP]"]
        );
        // Nothing else carries a detail: not an alias, a function, a
        // keyword, a schema or a CTE.
        for sql in [
            "SELECT | FROM orders o",
            "SELECT * FROM |",
            "WITH w AS (SELECT 1) SELECT | FROM w, orders",
            "SELECT * FROM shop.|",
        ] {
            let offset = sql.find('|').unwrap();
            let src = sql.replacen('|', "", 1);
            for i in complete(&src, offset as u32, Some(&cat)).items {
                let typed = i.kind == Kind::Field && !i.detail.is_empty();
                assert!(i.detail.is_empty() || typed, "{sql}: {i:?}");
            }
        }
    }

    #[test]
    fn a_table_in_two_schemas_is_offered_for_each_with_where_it_is() {
        let cat = Catalog::from_json_text(
            r#"[
            {"database":"pg","schema":"public","table":"orders","column":"id","type":"INT"},
            {"database":"pg","schema":"staging","table":"orders","column":"id","type":"INT"},
            {"database":"pg","schema":"public","table":"customers","column":"id","type":"INT"},
            {"database":"wh","schema":"public","table":"orders","column":"id","type":"INT"}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            shown("SELECT * FROM ord|", Some(&cat), 3),
            [
                "orders [pg.public] pg.public.orders",
                "orders [pg.staging] staging.orders",
                "orders [wh.public] wh.public.orders",
            ]
        );
        assert_eq!(shown("SELECT * FROM cu|", Some(&cat), 5), ["customers []"]);
        // After the database, the schema tells them apart; after the
        // schema, the database is typed already or cannot be.
        assert_eq!(
            shown("SELECT * FROM pg.|", Some(&cat), 5),
            [
                "public []",
                "staging []",
                "customers []",
                "orders [pg.public] public.orders",
                "orders [pg.staging] staging.orders",
            ]
        );
        assert_eq!(
            shown("SELECT * FROM pg.staging.|", Some(&cat), 5),
            ["orders []"]
        );
    }

    #[test]
    fn the_from_scopes_columns_come_first_wherever_a_column_goes() {
        let cat = shop();
        let first = |sql: &str| at(sql, Some(&cat)).into_iter().take(5).collect::<Vec<_>>();
        let orders = ["customer_id", "id", "Order Note", "placed", "total"];
        for sql in [
            "SELECT | FROM orders",
            "SELECT id, | FROM orders WHERE true",
            "FROM orders SELECT |",
            "FROM orders o SELECT o.id, |",
            "SELECT 1 FROM orders WHERE |",
            "SELECT 1 FROM orders WHERE id = 1 AND |",
            "SELECT 1 FROM orders GROUP BY |",
            "SELECT 1 FROM orders ORDER BY |",
            "SELECT 1 FROM orders o JOIN stock s ON s.order_id = o.id AND o.| ",
        ] {
            assert_eq!(first(sql), orders, "{sql}");
        }
        // A name two tables have is offered for each, qualified.
        assert_eq!(
            shown(
                "SELECT i| FROM orders o JOIN customers c ON true",
                Some(&cat),
                2
            ),
            ["id [BIGINT · o] o.id", "id [BIGINT · c] c.id",]
        );
    }

    #[test]
    fn a_join_condition_is_offered_first_when_the_keys_imply_one() {
        let cat = shop();
        assert_eq!(
            shown(
                "SELECT * FROM orders o JOIN customers c ON |",
                Some(&cat),
                1
            ),
            ["o.customer_id = c.id [join]"]
        );
        // The relation written first is written first.
        assert_eq!(
            shown("FROM customers JOIN orders ON |", Some(&cat), 1),
            ["customers.id = orders.customer_id [join]"]
        );
        // A key of two columns.
        assert_eq!(
            shown("FROM order_items i JOIN stock s ON |", Some(&cat), 1),
            ["i.order_id = s.order_id AND i.line_no = s.line_no \
                 [join]"]
        );
        // `orders.id` and `stock.order_id`.
        assert_eq!(
            shown("FROM stock s LEFT JOIN orders o ON |", Some(&cat), 1),
            ["s.order_id = o.id [join]"]
        );
        // Nothing to rest on: no key (`stock`, a view), no column named for
        // it, a bare `id` on both sides, a table not in the catalog, no
        // catalog, or not right after `ON`.
        let joins = |sql: &str, cat: Option<&Catalog>| -> Vec<Item> {
            let offset = sql.find('|').unwrap();
            let src = sql.replacen('|', "", 1);
            complete(&src, offset as u32, cat)
                .items
                .into_iter()
                .filter(|i| i.kind == Kind::Snippet)
                .collect()
        };
        for sql in [
            "FROM stock s JOIN big_orders b ON |",
            "FROM customers c JOIN stock s ON |",
            "FROM nope n JOIN customers c ON |",
            "FROM orders o JOIN customers c ON o.customer_id = |",
            "FROM orders o JOIN customers c ON true AND |",
            "FROM orders o, customers c WHERE |",
        ] {
            assert!(joins(sql, Some(&cat)).is_empty(), "{sql}");
        }
        assert!(joins("FROM orders o JOIN customers c ON |", None).is_empty());
    }

    #[test]
    fn a_list_of_columns_offers_only_the_columns_that_go_there() {
        let cat = shop();
        assert_eq!(
            at("SELECT * EXCLUDE (| FROM customers", Some(&cat)),
            ["country", "email", "id", "name"]
        );
        assert_eq!(
            at("SELECT * EXCLUDE (id, | FROM customers", Some(&cat)),
            ["country", "email", "name"]
        );
        assert_eq!(
            at(
                "SELECT c.* EXCLUDE (| FROM customers c JOIN stock s ON true",
                Some(&cat)
            ),
            ["country", "email", "id", "name"]
        );
        assert_eq!(
            at(
                "SELECT * REPLACE (upper(name) AS | FROM customers",
                Some(&cat)
            ),
            ["country", "email", "id", "name"]
        );
        // Before the `AS` it is an expression: columns first, then the rest.
        let replace = at("SELECT * REPLACE (| FROM customers", Some(&cat));
        assert_eq!(replace[..4], ["country", "email", "id", "name"]);
        assert!(replace.contains(&"upper".to_string()), "{replace:?}");
        // `USING (`: the columns both sides have.
        assert_eq!(
            at("FROM order_items JOIN stock USING (|", Some(&cat)),
            ["line_no", "order_id"]
        );
        assert_eq!(
            at("FROM order_items JOIN stock USING (order_id, |", Some(&cat)),
            ["line_no"]
        );
        assert_eq!(
            at("INSERT INTO customers (id, |", Some(&cat)),
            ["country", "email", "name"]
        );
        // A list of a table the catalog does not have offers nothing wrong.
        assert!(at("SELECT * EXCLUDE (| FROM nope", Some(&cat)).is_empty());
        assert!(at("SELECT * EXCLUDE (| FROM customers", None).is_empty());
    }

    #[test]
    fn a_large_catalog_answers_within_the_cap() {
        let mut rows = Vec::new();
        for t in 0..2_000 {
            for c in 0..50 {
                rows.push(format!(
                    r#"{{"database":"d","schema":"main","table":"t{t}","column":"c{c}","type":"INT"}}"#
                ));
            }
        }
        let cat = Catalog::from_json_text(&format!("[{}]", rows.join(","))).unwrap();
        let found = complete("SELECT * FROM ", 14, Some(&cat));
        assert!(found.incomplete);
        assert_eq!(found.items.len(), LIMIT);
        let found = complete("SELECT  FROM t7", 7, Some(&cat));
        assert_eq!(found.items[0].label, "c0");
        assert_eq!(found.items[0].detail, "INT");
    }

    #[test]
    fn a_wide_table_gives_the_first_columns_by_name() {
        let rows: Vec<String> = (0..5_000)
            .map(|c| {
                format!(r#"{{"database":"d","schema":"main","table":"wide","column":"c{c}","type":"INT"}}"#)
            })
            .collect();
        let cat = Catalog::from_json_text(&format!("[{}]", rows.join(","))).unwrap();
        let found = complete("SELECT  FROM wide", 7, Some(&cat));
        assert!(found.incomplete);
        let mut names: Vec<String> = (0..5_000).map(|c| format!("c{c}")).collect();
        names.sort();
        names.truncate(200);
        let got: Vec<String> = found.items.into_iter().map(|i| i.label).collect();
        assert_eq!(got, names);
        // What is typed narrows it, by the table's index of names.
        let found = complete("SELECT c49 FROM wide", 10, Some(&cat));
        assert!(!found.incomplete);
        assert_eq!(found.items.len(), 111);
        assert_eq!(found.items[0].label, "c49");
    }
}
