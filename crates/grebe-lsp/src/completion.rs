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
//! - anywhere else: the columns of the statement's relations first, then
//!   its aliases, functions, tables, keywords.
//!
//! Tables and columns come from the catalog when the server has one (see
//! [`grebe_rules::catalog`]); without one, the statement's own CTEs and the
//! tables the file creates still complete, with keywords and functions.

use std::collections::HashSet;

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
const LIMIT: usize = 1000;

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

fn text<'s>(src: &'s str, t: &Token) -> &'s str {
    &src[t.span.start as usize..t.span.end as usize]
}

fn is_punct(src: &str, t: Option<&Token>, p: &str) -> bool {
    t.is_some_and(|t| t.kind == TokenKind::Punct && text(src, t) == p)
}

fn upper(src: &str, t: Option<&Token>) -> String {
    t.filter(|t| t.kind == TokenKind::Word)
        .map(|t| text(src, t).to_ascii_uppercase())
        .unwrap_or_default()
}

/// A token as a name, one layer of quoting stripped.
fn name_of(src: &str, t: &Token) -> Option<String> {
    let raw = text(src, t);
    match t.kind {
        TokenKind::Word => Some(raw.to_string()),
        TokenKind::QuotedIdent if raw.len() >= 2 && raw.ends_with('"') => {
            Some(raw[1..raw.len() - 1].replace("\"\"", "\""))
        }
        _ => None,
    }
}

fn dotted_name(src: &str, t: &[Token], mut i: usize) -> Option<(Vec<String>, usize)> {
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

fn matching_paren(src: &str, t: &[Token], open: usize) -> Option<usize> {
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
fn name_list(src: &str, t: &[Token], open: usize) -> Option<(Vec<String>, usize)> {
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
enum Target {
    /// A table name as written, parts in order.
    Named(Vec<String>),
    /// Columns spelled out: `t AS a(x, y)`, `WITH c(x, y) AS`.
    Columns(Vec<String>),
    /// A subquery, a function, a file.
    Unknown,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Binding {
    name: String,
    target: Target,
}

#[derive(Default, Debug)]
struct Scope {
    bindings: Vec<Binding>,
    ctes: Vec<(String, Option<Vec<String>>)>,
}

/// An alias at `t[i]`: `[AS] name [(cols)]`. `(alias, columns)`.
fn alias_at(src: &str, t: &[Token], mut i: usize) -> Option<(String, Option<Vec<String>>)> {
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
    Some((name, columns))
}

/// The relation that starts at `t[i]`, right after `FROM`, `JOIN`, a comma
/// in a FROM list, or a DML keyword. `is_target`: after `INTO`/`UPDATE`,
/// where a `(` after the name is a column list, not a call.
fn relation_at(src: &str, t: &[Token], i: usize, is_target: bool, scope: &mut Scope) {
    let Some(first) = t.get(i) else { return };
    // `(subquery) alias`, `'file.csv' alias`.
    if is_punct(src, Some(first), "(") || first.kind == TokenKind::String {
        let after = if first.kind == TokenKind::String {
            Some(i + 1)
        } else {
            matching_paren(src, t, i).map(|c| c + 1)
        };
        if let Some((name, columns)) = after.and_then(|a| alias_at(src, t, a)) {
            scope.bindings.push(Binding {
                name,
                target: columns.map_or(Target::Unknown, Target::Columns),
            });
        }
        return;
    }
    if first.kind == TokenKind::Word
        && NOT_AN_ALIAS.contains(&text(src, first).to_ascii_uppercase().as_str())
    {
        return;
    }
    let Some((parts, next)) = dotted_name(src, t, i) else {
        return;
    };
    let last = parts.last().cloned().unwrap_or_default();
    if !is_target && is_punct(src, t.get(next), "(") {
        // A table function: `read_csv('f') r`, `range(3) t(i)`.
        let alias = matching_paren(src, t, next).and_then(|c| alias_at(src, t, c + 1));
        let (name, columns) = alias.unwrap_or((last, None));
        scope.bindings.push(Binding {
            name,
            target: columns.map_or(Target::Unknown, Target::Columns),
        });
        return;
    }
    match alias_at(src, t, next) {
        Some((name, Some(columns))) if !is_target => scope.bindings.push(Binding {
            name,
            target: Target::Columns(columns),
        }),
        Some((name, _)) => scope.bindings.push(Binding {
            name,
            target: Target::Named(parts),
        }),
        None => scope.bindings.push(Binding {
            name: last,
            target: Target::Named(parts),
        }),
    }
}

/// `name [(cols)] AS [[NOT] MATERIALIZED] (` at `t[i]`: a CTE head.
fn cte_at(src: &str, t: &[Token], i: usize) -> Option<(String, Option<Vec<String>>)> {
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
    is_punct(src, t.get(j), "(").then_some((name, columns))
}

/// Every relation and CTE one statement's tokens name.
fn scope_of(src: &str, t: &[Token]) -> Scope {
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
                        scope.ctes.push(cte);
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
                    scope.ctes.push(cte);
                }
            }
            "FROM" => {
                // `x IS DISTINCT FROM y` is not a FROM clause.
                if i > 0 && upper(src, t.get(i - 1)) == "DISTINCT" {
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
                if sub || !is_punct(src, t.get(i + 1), "(") {
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
    items: Vec<Item>,
    /// Some names were not looked at: there were more than an answer
    /// carries.
    incomplete: bool,
}

impl Collector {
    fn add(&mut self, name: &str, kind: Kind, detail: impl Into<String>, rank: u8) {
        let lname = name.to_ascii_lowercase();
        if name.is_empty() || !lname.starts_with(&self.prefix) {
            return;
        }
        if !self.seen.insert((lname, kind)) {
            return;
        }
        let quoted = match kind {
            Kind::Keyword | Kind::Function => name.to_string(),
            _ => quote_ident(name),
        };
        self.items.push(Item {
            label: name.to_string(),
            kind,
            detail: detail.into(),
            insert: (quoted != name).then_some(quoted),
            rank,
        });
    }

    fn table(&mut self, t: &Table, rank: u8) {
        let kind = if t.is_view {
            Kind::Interface
        } else {
            Kind::Class
        };
        let what = if t.is_view { "view" } else { "table" };
        self.add(
            &t.name,
            kind,
            format!("{what} · {}.{}", t.database, t.schema),
            rank,
        );
    }

    /// The catalog's tables that match what is typed, within `within` (see
    /// [`Catalog::tables_with_prefix`]). The catalog finds them by its
    /// index and hands over no more than an answer can carry, so a catalog
    /// of a hundred thousand tables costs what a small one does.
    fn tables(&mut self, catalog: &Catalog, within: &[String], rank: u8) {
        let (found, cut) = catalog.tables_with_prefix(&self.prefix, within, LIMIT + 1);
        for t in found {
            self.table(t, rank);
        }
        self.incomplete |= cut;
    }

    fn columns_of(&mut self, t: &Table, rank: u8) {
        for c in &t.columns {
            self.add(
                &c.name,
                Kind::Field,
                format!("{} · {}", c.data_type, t.name),
                rank,
            );
        }
    }

    fn names(&mut self, names: &[String], detail: &str, rank: u8) {
        for n in names {
            self.add(n, Kind::Field, detail, rank);
        }
    }
}

/// The columns of whatever `parts` names: a CTE, a table the file creates,
/// or a catalog table.
fn columns_named(
    out: &mut Collector,
    parts: &[String],
    scope: &Scope,
    file: &FileContext,
    catalog: Option<&Catalog>,
    rank: u8,
) {
    let lower: Vec<String> = parts.iter().map(|p| p.to_ascii_lowercase()).collect();
    let Some(last) = lower.last() else { return };
    if lower.len() == 1 {
        if let Some((name, cols)) = scope
            .ctes
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(last))
        {
            if let Some(cols) = cols {
                out.names(cols, name, rank);
            }
            return;
        }
    }
    if let Some(local) = file.local.get(last) {
        if let Some(cols) = &local.columns {
            out.names(cols, &local.name, rank);
        }
        return;
    }
    if let Some(Resolved::Found(tables)) = catalog.map(|c| c.resolve(&lower)) {
        for t in tables {
            out.columns_of(t, rank);
        }
    }
}

/// Complete at byte `offset` of `src`.
#[must_use]
pub fn complete(src: &str, offset: u32, catalog: Option<&Catalog>) -> Completions {
    let Some(cur) = cursor(src, offset) else {
        return Completions::default();
    };
    let t = &cur.statement;
    let scope = scope_of(src, t);
    let file = FileContext::scan(src);
    let mut out = Collector {
        prefix: cur.prefix.to_ascii_lowercase(),
        seen: HashSet::new(),
        items: Vec::new(),
        incomplete: false,
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
                match &b.target {
                    Target::Named(parts) => {
                        columns_named(&mut out, parts, &scope, &file, catalog, 0)
                    }
                    Target::Columns(cols) => out.names(cols, &b.name, 0),
                    Target::Unknown => {}
                }
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
                        out.add(schema, Kind::Module, format!("schema · {database}"), 1);
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

    let tables_rank = if table_context { 0 } else { 4 };
    if !table_context {
        for b in &scope.bindings {
            match &b.target {
                Target::Named(parts) => columns_named(&mut out, parts, &scope, &file, catalog, 0),
                Target::Columns(cols) => out.names(cols, &b.name, 0),
                Target::Unknown => {}
            }
        }
        for b in &scope.bindings {
            let detail = match &b.target {
                Target::Named(parts)
                    if !parts
                        .last()
                        .is_some_and(|l| l.eq_ignore_ascii_case(&b.name)) =>
                {
                    parts.join(".")
                }
                _ => String::new(),
            };
            out.add(&b.name, Kind::Variable, detail, 1);
        }
        for a in grebe_rules::detect::aggregate_names() {
            out.add(a, Kind::Function, "aggregate", 3);
        }
        for f in FUNCTIONS {
            out.add(f, Kind::Function, "", 3);
        }
        if let Some(cat) = catalog {
            for f in cat.functions() {
                out.add(&f.name, Kind::Function, f.kind.as_str(), 3);
            }
        }
    }
    for (name, _) in &scope.ctes {
        out.add(name, Kind::Class, "CTE", tables_rank);
    }
    let mut local: Vec<_> = file.local.values().collect();
    local.sort_by(|a, b| a.name.cmp(&b.name));
    for l in local {
        out.add(&l.name, Kind::Class, "created in this file", tables_rank);
    }
    if let Some(cat) = catalog {
        out.tables(cat, &[], tables_rank);
        for (schema, _) in cat.schemas() {
            out.add(schema, Kind::Module, "schema", tables_rank + 1);
        }
        for database in cat.databases() {
            out.add(database, Kind::Module, "database", tables_rank + 1);
        }
        if table_context {
            for f in cat.functions().filter(|f| f.kind.contains("table")) {
                out.add(&f.name, Kind::Function, f.kind.as_str(), 2);
            }
        }
    }
    for k in KEYWORDS {
        out.add(k, Kind::Keyword, "", 6);
    }
    finish(out)
}

fn finish(out: Collector) -> Completions {
    let mut items = out.items;
    items.sort_by(|a, b| {
        (a.rank, a.label.to_ascii_lowercase()).cmp(&(b.rank, b.label.to_ascii_lowercase()))
    });
    let incomplete = out.incomplete || items.len() > LIMIT;
    items.truncate(LIMIT);
    Completions { items, incomplete }
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
        assert_eq!(date.detail, "DATE · orders");
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
        assert!(got.contains(&"sum".to_string()), "aggregates: {got:?}");
        assert!(got.contains(&"SELECT".to_string()), "keywords: {got:?}");
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
}
