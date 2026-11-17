//! An optional catalog snapshot, and the two `CAT` rules that read it.
//!
//! grebe embeds no engine and needs no catalog: every `MOD` rule is a pure
//! function of the source. A catalog is something a caller may *hand in* --
//! `grebe check --catalog FILE`, `catalog = "..."` in `grebe.toml`, or an
//! editor's `grebe/catalog` notification -- and only then do the `CAT` rules
//! run:
//!
//! - **CAT001 unknown-table**: a table or view a statement reads or writes
//!   that the catalog does not have.
//! - **CAT002 unknown-column**: `alias.column` where `alias` is a catalog
//!   table in the statement and the table has no such column.
//!
//! # The snapshot
//!
//! A JSON array with one object per column, exactly what DuckDB prints for
//! [`SNAPSHOT_SQL`] in `-json` mode:
//!
//! ```json
//! [{"database":"memory","schema":"main","table":"orders","column":"id","type":"BIGINT"}]
//! ```
//!
//! Extra keys are ignored (`"kind": "table" | "view"` is read when present).
//! An object `{"columns": [...], "functions": [{"name", "kind"}]}` is
//! accepted too; the functions only feed completion. A row without a
//! `"column"` lists a table whose columns are not known: it exists for
//! CAT001 and is never checked by CAT002.
//!
//! # This is not a binder
//!
//! Names are matched, not bound. The rules flag only what cannot be right
//! under any reading, and stay silent whenever a statement or a file does
//! something this module does not model. In particular:
//!
//! - A name defined in the statement (a CTE) or anywhere in the file
//!   (`CREATE TABLE`/`VIEW`, `ALTER`, `DROP`) exists, with unknown columns.
//! - An unqualified name matches a table of that name in *any* database and
//!   schema of the catalog, so `USE` and `search_path` cannot produce a false
//!   finding; a qualifier the catalog has never heard of (a database the file
//!   attaches, `information_schema`) is not checked at all.
//! - File names, table functions, subqueries, `PIVOT` output, renamed
//!   columns (`t AS a(x, y)`), lambda parameters and select-list aliases are
//!   all opaque.
//! - A file that changes what names mean in a way a token scan cannot follow
//!   (`USE` of something outside the catalog, `SET search_path`,
//!   `IMPORT DATABASE`, a CLI dot-command) is not checked.
//!
//! Identifiers compare case-insensitively, quoted or not, as DuckDB's do.

use std::collections::{HashMap, HashSet};

use grebe_syntax::Span;
use grebe_syntax::cst::{NodeId, Tree};
use grebe_syntax::token::{Token, TokenKind};

use crate::detect::{Finding, Fix};
use crate::json::Json;

/// The query whose `-json` output is a snapshot.
pub const SNAPSHOT_SQL: &str = "SELECT database_name AS database, schema_name AS schema, \
    table_name AS \"table\", column_name AS \"column\", data_type AS type \
    FROM duckdb_columns() WHERE NOT internal";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub is_view: bool,
    /// Empty when the snapshot listed the table without its columns.
    pub columns: Vec<Column>,
}

impl Table {
    #[must_use]
    pub fn has_column(&self, lowercase: &str) -> bool {
        self.columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(lowercase))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    /// As `duckdb_functions().function_type` spells it: `scalar`,
    /// `aggregate`, `table`, `macro`, `table_macro`, ...
    pub kind: String,
}

/// A catalog snapshot, indexed for case-insensitive lookup.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    tables: Vec<Table>,
    /// Lowercase table name -> indexes into `tables`.
    by_name: HashMap<String, Vec<usize>>,
    databases: HashSet<String>,
    schemas: HashSet<String>,
    functions: Vec<Function>,
}

/// What a (possibly qualified) table name is, by the catalog.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved<'a> {
    /// The catalog cannot say: a qualifier it does not know.
    Unchecked,
    /// The catalog covers where the name points and it is not there.
    Missing,
    Found(Vec<&'a Table>),
}

impl Catalog {
    /// Build a catalog from tables. Tables with the same database, schema
    /// and name (case-insensitively) are merged.
    #[must_use]
    pub fn from_tables(tables: Vec<Table>, functions: Vec<Function>) -> Self {
        let mut cat = Catalog {
            functions,
            ..Catalog::default()
        };
        let mut index: HashMap<(String, String, String), usize> = HashMap::new();
        for t in tables {
            let key = (lower(&t.database), lower(&t.schema), lower(&t.name));
            if let Some(&i) = index.get(&key) {
                for c in t.columns {
                    if !cat.tables[i].has_column(&c.name) {
                        cat.tables[i].columns.push(c);
                    }
                }
                continue;
            }
            cat.databases.insert(key.0.clone());
            cat.schemas.insert(key.1.clone());
            cat.by_name
                .entry(key.2.clone())
                .or_default()
                .push(cat.tables.len());
            index.insert(key, cat.tables.len());
            cat.tables.push(t);
        }
        cat
    }

    /// Read a snapshot: an array of column rows, or
    /// `{"columns": [...], "functions": [...]}`.
    pub fn from_json(v: &Json) -> Result<Self, String> {
        let (rows, functions) = match v {
            Json::Array(rows) => (rows.as_slice(), None),
            Json::Object(_) => (
                v.get("columns")
                    .and_then(Json::as_array)
                    .ok_or("expected a \"columns\" array")?,
                v.get("functions").and_then(Json::as_array),
            ),
            _ => return Err("expected an array of column rows".into()),
        };
        let mut tables: Vec<Table> = Vec::new();
        let mut index: HashMap<(String, String, String), usize> = HashMap::new();
        for (n, row) in rows.iter().enumerate() {
            if row.as_object().is_none() {
                return Err(format!("row {}: expected an object", n + 1));
            }
            let field = |k: &str| row.get(k).and_then(Json::as_str);
            let Some(name) = field("table") else {
                return Err(format!("row {}: no \"table\"", n + 1));
            };
            let database = field("database").unwrap_or("");
            let schema = field("schema").unwrap_or("");
            let key = (lower(database), lower(schema), lower(name));
            let i = *index.entry(key).or_insert_with(|| {
                tables.push(Table {
                    database: database.to_string(),
                    schema: schema.to_string(),
                    name: name.to_string(),
                    is_view: false,
                    columns: Vec::new(),
                });
                tables.len() - 1
            });
            if field("kind").is_some_and(|k| k.eq_ignore_ascii_case("view")) {
                tables[i].is_view = true;
            }
            if let Some(column) = field("column") {
                if !tables[i].has_column(column) {
                    tables[i].columns.push(Column {
                        name: column.to_string(),
                        data_type: field("type").unwrap_or("").to_string(),
                    });
                }
            }
        }
        let functions = functions
            .unwrap_or_default()
            .iter()
            .filter_map(|f| {
                Some(Function {
                    name: f.get("name")?.as_str()?.to_string(),
                    kind: f
                        .get("kind")
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .collect();
        Ok(Self::from_tables(tables, functions))
    }

    /// Read a snapshot from JSON text.
    pub fn from_json_text(text: &str) -> Result<Self, String> {
        let v = crate::json::parse(text).map_err(|e| e.0)?;
        Self::from_json(&v)
    }

    #[must_use]
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    #[must_use]
    pub fn functions(&self) -> &[Function] {
        &self.functions
    }

    /// Is `lowercase` the name of a database in the snapshot?
    #[must_use]
    pub fn has_database(&self, lowercase: &str) -> bool {
        self.databases.contains(lowercase)
    }

    /// Is `lowercase` the name of a schema in the snapshot?
    #[must_use]
    pub fn has_schema(&self, lowercase: &str) -> bool {
        self.schemas.contains(lowercase)
    }

    /// Resolve a table name given as lowercase parts, last part the table.
    ///
    /// - `t`: a table named `t` anywhere.
    /// - `q.t`: `q` as a schema or a database, whichever the catalog knows.
    /// - `d.s.t`: database and schema.
    #[must_use]
    pub fn resolve(&self, parts: &[String]) -> Resolved<'_> {
        let Some(name) = parts.last() else {
            return Resolved::Unchecked;
        };
        let named = self.by_name.get(name).map_or(&[][..], Vec::as_slice);
        let keep: Vec<&Table> = match parts.len() {
            1 => named.iter().map(|&i| &self.tables[i]).collect(),
            2 => {
                let q = &parts[0];
                if !self.has_database(q) && !self.has_schema(q) {
                    return Resolved::Unchecked;
                }
                named
                    .iter()
                    .map(|&i| &self.tables[i])
                    .filter(|t| {
                        t.database.eq_ignore_ascii_case(q) || t.schema.eq_ignore_ascii_case(q)
                    })
                    .collect()
            }
            3 => {
                let (d, s) = (&parts[0], &parts[1]);
                if !self.has_database(d) {
                    return Resolved::Unchecked;
                }
                named
                    .iter()
                    .map(|&i| &self.tables[i])
                    .filter(|t| {
                        t.database.eq_ignore_ascii_case(d) && t.schema.eq_ignore_ascii_case(s)
                    })
                    .collect()
            }
            _ => return Resolved::Unchecked,
        };
        if keep.is_empty() {
            Resolved::Missing
        } else {
            Resolved::Found(keep)
        }
    }
}

fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// One layer of quoting stripped, lowercased: `"Foo"`, `'Foo'` and `foo`
/// are the same name.
fn ident(raw: &str) -> String {
    let t = raw.trim();
    let b = t.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        return t[1..t.len() - 1].replace("\"\"", "\"").to_ascii_lowercase();
    }
    if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        return t[1..t.len() - 1].replace("''", "'").to_ascii_lowercase();
    }
    t.to_ascii_lowercase()
}

/// `name` as SQL: bare when it can be, double-quoted when it must be.
#[must_use]
pub fn quote_ident(name: &str) -> String {
    let mut chars = name.chars();
    let bare = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !grebe_syntax::grammar::classify_keyword(name)
            .has(grebe_syntax::keyword::Keyword::RESERVED);
    if bare {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

// ---------------------------------------------------------------------
// What the file itself defines
// ---------------------------------------------------------------------

/// A table or view a file creates, alters or drops.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalTable {
    /// As written, one layer of quoting stripped.
    pub name: String,
    /// Column names, when the file spells them out (`CREATE TABLE t (a
    /// INT)`, `CREATE VIEW v (a, b) AS`) and nothing later alters them.
    pub columns: Option<Vec<String>>,
}

/// What a whole file says about names, read off its tokens so that a
/// statement that does not parse still counts.
#[derive(Clone, Debug, Default)]
pub struct FileContext {
    /// Lowercase name -> the table. Any schema: `CREATE TABLE s.t` makes
    /// `t` local.
    pub local: HashMap<String, LocalTable>,
    /// Lowercase aliases of databases the file attaches.
    pub attached: HashSet<String>,
    /// `USE` targets, lowercase parts.
    pub uses: Vec<Vec<String>>,
    /// The file does something that changes what names mean and that this
    /// scan cannot follow.
    pub opaque: bool,
}

impl FileContext {
    #[must_use]
    pub fn scan(src: &str) -> Self {
        let mut ctx = FileContext::default();
        let tokens: Vec<Token> = grebe_syntax::token::tokenize(src)
            .into_iter()
            .filter(|t| !t.kind.is_trivia())
            .collect();
        for stmt in tokens.split(|t| t.kind == TokenKind::Punct && text(src, t) == ";") {
            ctx.scan_statement(src, stmt);
        }
        ctx
    }

    /// Should the `CAT` rules check this file against `catalog` at all?
    #[must_use]
    pub fn checkable(&self, catalog: &Catalog) -> bool {
        !self.opaque
            && self.uses.iter().all(|parts| {
                parts
                    .iter()
                    .all(|p| catalog.has_database(p) || catalog.has_schema(p))
            })
    }

    fn define(&mut self, name: String, columns: Option<Vec<String>>) {
        // Defined twice, or defined and then altered: only what both agree
        // on is known, which is nothing.
        let key = lower(&name);
        let columns = if self.local.contains_key(&key) {
            None
        } else {
            columns
        };
        self.local.insert(key, LocalTable { name, columns });
    }

    fn scan_statement(&mut self, src: &str, t: &[Token]) {
        let word = |i: usize| -> String {
            t.get(i)
                .filter(|t| t.kind == TokenKind::Word)
                .map(|t| text(src, t).to_ascii_uppercase())
                .unwrap_or_default()
        };
        let Some(first) = t.first() else { return };
        // A CLI dot-command (`.read other.sql`) can bring in anything.
        if first.kind == TokenKind::Punct && text(src, first) == "." {
            self.opaque = true;
            return;
        }
        match word(0).as_str() {
            "CREATE" => {
                // CREATE [OR REPLACE] [TEMP|TEMPORARY|...] TABLE|VIEW ...
                let mut at = 1;
                while matches!(
                    word(at).as_str(),
                    "OR" | "REPLACE"
                        | "TEMP"
                        | "TEMPORARY"
                        | "PERSISTENT"
                        | "SECURE"
                        | "RECURSIVE"
                        | "UNLOGGED"
                        | "GLOBAL"
                        | "LOCAL"
                ) {
                    at += 1;
                }
                if !matches!(word(at).as_str(), "TABLE" | "VIEW") {
                    return;
                }
                let is_table = word(at) == "TABLE";
                let mut i = at + 1;
                if word(i) == "IF" && word(i + 1) == "NOT" && word(i + 2) == "EXISTS" {
                    i += 3;
                }
                let Some((parts, next)) = dotted_name(src, t, i) else {
                    return;
                };
                let Some(name) = parts.last().cloned() else {
                    return;
                };
                let columns = if is_punct(src, t.get(next), "(") {
                    if is_table {
                        table_columns(src, t, next)
                    } else {
                        name_list(src, t, next)
                    }
                } else {
                    None
                };
                self.define(name, columns);
            }
            "ALTER" | "DROP" => {
                if !matches!(word(1).as_str(), "TABLE" | "VIEW") {
                    return;
                }
                let mut i = 2;
                if word(i) == "IF" && word(i + 1) == "EXISTS" {
                    i += 2;
                }
                if let Some((parts, _)) = dotted_name(src, t, i) {
                    if let Some(name) = parts.last().cloned() {
                        self.define(name, None);
                    }
                }
                // ALTER TABLE a RENAME TO b
                for j in i..t.len() {
                    if word(j) == "RENAME" && word(j + 1) == "TO" {
                        if let Some((parts, _)) = dotted_name(src, t, j + 2) {
                            if let Some(name) = parts.last().cloned() {
                                self.define(name, None);
                            }
                        }
                    }
                }
            }
            "ATTACH" => {
                // ATTACH [OR REPLACE] [IF NOT EXISTS] [DATABASE] 'path' [AS alias] [(options)]
                let mut depth = 0usize;
                let mut alias = None;
                for (j, tok) in t.iter().enumerate() {
                    match text(src, tok) {
                        "(" => depth += 1,
                        ")" => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    if depth == 0 && word(j) == "AS" {
                        alias = t.get(j + 1).and_then(|a| name_of(src, a));
                    }
                }
                let alias = alias.or_else(|| {
                    // No alias: DuckDB names it after the file, when the
                    // path is a literal.
                    let mut i = 1;
                    while matches!(
                        word(i).as_str(),
                        "OR" | "REPLACE" | "IF" | "NOT" | "EXISTS" | "DATABASE"
                    ) {
                        i += 1;
                    }
                    let path = t.get(i).filter(|t| t.kind == TokenKind::String)?;
                    let path = ident(text(src, path));
                    let base = path.rsplit(['/', '\\']).next()?;
                    let stem = base.split('.').next()?;
                    (!stem.is_empty() && !stem.contains(':')).then(|| stem.to_string())
                });
                match alias {
                    Some(a) => {
                        self.attached.insert(lower(&a));
                    }
                    None => self.opaque = true,
                }
            }
            "USE" => match dotted_name(src, t, 1) {
                Some((parts, _)) => self.uses.push(parts.iter().map(|p| lower(p)).collect()),
                None => self.opaque = true,
            },
            "IMPORT" => self.opaque = true,
            "COPY" => {
                if word(1) == "FROM" && word(2) == "DATABASE" {
                    self.opaque = true;
                }
            }
            "SET" | "RESET" => {
                let mut i = 1;
                if matches!(word(i).as_str(), "SESSION" | "GLOBAL" | "LOCAL") {
                    i += 1;
                }
                if matches!(word(i).as_str(), "SEARCH_PATH" | "SCHEMA") {
                    self.opaque = true;
                }
            }
            _ => {}
        }
    }
}

fn text<'s>(src: &'s str, t: &Token) -> &'s str {
    &src[t.span.start as usize..t.span.end as usize]
}

fn is_punct(src: &str, t: Option<&Token>, p: &str) -> bool {
    t.is_some_and(|t| t.kind == TokenKind::Punct && text(src, t) == p)
}

/// A token as a name, one layer of quoting stripped: a word, a quoted
/// identifier, or a string literal standing in for one.
fn name_of(src: &str, t: &Token) -> Option<String> {
    let raw = text(src, t);
    match t.kind {
        TokenKind::Word => Some(raw.to_string()),
        TokenKind::QuotedIdent if raw.len() >= 2 => {
            Some(raw[1..raw.len() - 1].replace("\"\"", "\""))
        }
        TokenKind::String if raw.starts_with('\'') && raw.len() >= 2 => {
            Some(raw[1..raw.len() - 1].replace("''", "'"))
        }
        _ => None,
    }
}

/// `a.b.c` starting at `t[i]`: the parts, and the index after the name.
fn dotted_name(src: &str, t: &[Token], mut i: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = vec![name_of(src, t.get(i)?)?];
    i += 1;
    while is_punct(src, t.get(i), ".") {
        parts.push(name_of(src, t.get(i + 1)?)?);
        i += 2;
    }
    Some((parts, i))
}

/// The index of the `)` matching the `(` at `t[open]`.
fn matching_paren(src: &str, t: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (j, tok) in t.iter().enumerate().skip(open) {
        if tok.kind != TokenKind::Punct {
            continue;
        }
        match text(src, tok) {
            "(" | "[" => depth += 1,
            ")" | "]" => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
    }
    None
}

/// `(a, b, c)` at `t[open]`: the names, or `None` if it is anything else.
fn name_list(src: &str, t: &[Token], open: usize) -> Option<Vec<String>> {
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
    Some(out)
}

/// The column names of `CREATE TABLE t ( ... )`, `(` at `t[open]`: the
/// first token of each top-level element, constraints left out. `None` when
/// the definition goes on to `AS` (the query adds columns of its own).
fn table_columns(src: &str, t: &[Token], open: usize) -> Option<Vec<String>> {
    let close = matching_paren(src, t, open)?;
    if t.get(close + 1)
        .is_some_and(|n| n.kind == TokenKind::Word && text(src, n).eq_ignore_ascii_case("AS"))
    {
        return None;
    }
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut at_start = true;
    for tok in &t[open + 1..close] {
        let raw = text(src, tok);
        if tok.kind == TokenKind::Punct {
            match raw {
                "(" | "[" => depth += 1,
                ")" | "]" => depth = depth.saturating_sub(1),
                "," if depth == 0 => {
                    at_start = true;
                    continue;
                }
                _ => {}
            }
        }
        if at_start && depth == 0 {
            at_start = false;
            let constraint = tok.kind == TokenKind::Word
                && matches!(
                    raw.to_ascii_uppercase().as_str(),
                    "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "CONSTRAINT"
                );
            if !constraint {
                out.push(name_of(src, tok)?);
            }
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------

/// Schemas every database has, which a snapshot (`WHERE NOT internal`)
/// leaves out.
fn is_system_qualifier(lowercase: &str) -> bool {
    matches!(lowercase, "information_schema" | "pg_catalog" | "system")
}

/// DuckDB's built-in views, usable unqualified: `duckdb_tables`,
/// `pg_tables`, `sqlite_master`, `pragma_database_list`.
fn is_system_table(lowercase: &str) -> bool {
    ["duckdb_", "pg_", "sqlite_", "pragma_"]
        .iter()
        .any(|p| lowercase.starts_with(p))
}

/// A quoted name that is a file, a glob or a URL, not a table: DuckDB reads
/// it with a replacement scan.
fn looks_like_a_file(name: &str) -> bool {
    name.contains(['.', '/', '\\', '*', '?', ':'])
}

/// What a name in a statement's FROM (or DML target) stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Relation {
    /// One catalog table whose columns are known: its index in the lookup
    /// done for this statement.
    Table(usize),
    /// Anything else: a subquery, a function, a CTE, a local table, a
    /// renamed column list, a name the catalog has twice.
    Opaque,
}

fn rule(tree: &Tree, id: NodeId) -> &'static str {
    tree.rule_name(id)
}

fn direct_child(tree: &Tree, id: NodeId, name: &str) -> Option<NodeId> {
    tree.children(id)
        .iter()
        .copied()
        .find(|&c| rule(tree, c) == name)
}

/// The first descendant of `id` produced by one of `rules`, in source order.
fn first_descendant(tree: &Tree, id: NodeId, rules: &[&str]) -> Option<NodeId> {
    tree.descendants(id)
        .into_iter()
        .find(|&d| rules.contains(&rule(tree, d)))
}

/// The non-trivia tokens inside `span`.
fn tokens_in(tree: &Tree, span: Span) -> Vec<Token> {
    let from = tree.tokens.partition_point(|t| t.span.start < span.start);
    tree.tokens[from..]
        .iter()
        .take_while(|t| t.span.end <= span.end)
        .filter(|t| !t.kind.is_trivia())
        .copied()
        .collect()
}

/// The span of `id` without leading or trailing trivia.
fn tight(tree: &Tree, id: NodeId) -> Span {
    let span = tree.node(id).span;
    let toks = tokens_in(tree, span);
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => Span::new(a.span.start, b.span.end),
        _ => span,
    }
}

/// A table name as written in a statement.
struct TableName {
    /// Lowercase parts, last the table.
    parts: Vec<String>,
    /// The table part's node.
    node: NodeId,
    /// A string literal, or a quoted name that is a path.
    file: bool,
}

fn table_name(tree: &Tree, src: &str, base: NodeId) -> Option<TableName> {
    let mut parts = Vec::new();
    let mut node = None;
    let mut file = false;
    for d in tree.descendants(base) {
        match rule(tree, d) {
            "CatalogName" | "SchemaName" | "ReservedSchemaName" => {
                parts.push(ident(tree.text(d, src)));
            }
            "TableName" | "ReservedTableName" => {
                let raw = tree.text(d, src).trim();
                let name = ident(raw);
                file = raw.starts_with('\'') || (raw.starts_with('"') && looks_like_a_file(&name));
                parts.push(name);
                node = Some(d);
            }
            _ => {}
        }
    }
    Some(TableName {
        parts,
        node: node?,
        file,
    })
}

/// Where a `BaseTableName` is something a statement reads or writes, as
/// opposed to something it defines, drops, describes or constrains.
const CHECKED_PARENTS: &[&str] = &[
    "BaseTableRef",
    "NearestBaseTableRef",
    "TableStatement",
    "InsertTarget",
    "TargetOptAlias",
    "BaseTableAliasSet",
    "BaseTableSet",
    "CopyTable",
    "TruncateStatement",
];

/// The alias a relation node gives itself: `t a`, `t AS a`, `a: t`.
/// `(name, renamed)`, `renamed` when it also renames columns (`a(x, y)`).
fn alias_of(tree: &Tree, src: &str, rel: NodeId) -> Option<(String, bool)> {
    if let Some(colon) = direct_child(tree, rel, "TableAliasColon") {
        let tok = tokens_in(tree, tree.node(colon).span).into_iter().next()?;
        return Some((ident(text(src, &tok)), false));
    }
    let alias = direct_child(tree, rel, "TableAlias")?;
    let renamed = first_descendant(tree, alias, &["ColumnAliases"]).is_some();
    let name = tokens_in(tree, tree.node(alias).span)
        .into_iter()
        .find(|t| !(t.kind == TokenKind::Word && text(src, t).eq_ignore_ascii_case("AS")))?;
    Some((ident(text(src, &name)), renamed))
}

/// The alias of a DML target (`UPDATE t AS u`, `DELETE FROM t d`, `INSERT
/// INTO t AS i`): the last name after the table name, `AS` and `SET` aside.
fn target_alias(tree: &Tree, src: &str, target: NodeId, base: NodeId) -> Option<String> {
    let after = tree.node(base).span.end;
    tokens_in(tree, tree.node(target).span)
        .into_iter()
        .rfind(|t| {
            let w = text(src, t);
            t.span.start >= after
                && matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent)
                && !(w.eq_ignore_ascii_case("AS") || w.eq_ignore_ascii_case("SET"))
        })
        .map(|t| ident(text(src, &t)))
}

/// Run both `CAT` rules over every statement of `tree`.
///
/// `file` is the scan of the whole file the tree came from, not of the
/// tree's own text: on the per-statement fallback path the tree is one
/// statement and the file is still the file.
#[must_use]
pub fn check(tree: &Tree, src: &str, catalog: &Catalog, file: &FileContext) -> Vec<Finding> {
    if !file.checkable(catalog) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for &stmt in tree.children(tree.root()) {
        check_statement(tree, src, stmt, catalog, file, &mut out);
    }
    out.sort_by_key(|f| (f.span.start, f.code));
    out
}

fn check_statement(
    tree: &Tree,
    src: &str,
    stmt: NodeId,
    catalog: &Catalog,
    file: &FileContext,
    out: &mut Vec<Finding>,
) {
    let nodes = tree.descendants(stmt);

    // First pass: what the statement defines for itself.
    let mut ctes: HashSet<String> = HashSet::new();
    // Names that may shadow a table alias in an expression: lambda
    // parameters and select-list aliases.
    let mut shadows: HashSet<String> = HashSet::new();
    let mut check_columns = true;
    for &n in &nodes {
        match rule(tree, n) {
            // A macro's parameters and a trigger's NEW/OLD are names this
            // module does not model.
            "CreateMacroStmt" | "CreateTriggerStmt" => return,
            "PivotStatement" | "UnpivotStatement" | "TablePivotClause" | "TableUnpivotClause" => {
                check_columns = false;
            }
            "WithStatement" => {
                if let Some(tok) = tokens_in(tree, tree.node(n).span).first() {
                    ctes.insert(ident(text(src, tok)));
                }
            }
            "LambdaExpression" => {
                for &c in tree.children(n) {
                    if rule(tree, c) == "ColIdOrString" {
                        shadows.insert(ident(tree.text(c, src)));
                    }
                }
            }
            // `x -> ...` and `(x, y) -> ...`: every name left of the arrow.
            "LambdaArrowExpression" => {
                let kids = tree.children(n);
                if kids.iter().any(|&c| rule(tree, c) == "SingleArrowPair") {
                    if let Some(&left) = kids.first() {
                        for t in tokens_in(tree, tree.node(left).span) {
                            if matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent) {
                                shadows.insert(ident(text(src, &t)));
                            }
                        }
                    }
                }
            }
            "ExpressionAsCollabel" | "ExpressionOptIdentifier" => {
                // The alias is the last direct child when it is not the
                // expression itself.
                if let Some(&last) = tree.children(n).last() {
                    if matches!(rule(tree, last), "ColLabelOrString" | "Identifier") {
                        shadows.insert(ident(tree.text(last, src)));
                    }
                }
            }
            "ColIdExpression" => {
                if let Some(tok) = tokens_in(tree, tree.node(n).span).first() {
                    shadows.insert(ident(text(src, tok)));
                }
            }
            _ => {}
        }
    }

    // Second pass: the relations in scope, and the table names to check.
    let mut tables: Vec<&Table> = Vec::new();
    let mut bindings: Vec<(String, Relation)> = Vec::new();
    for &n in &nodes {
        match rule(tree, n) {
            "BaseTableName" => {
                let Some(parent) = tree.parent(n) else {
                    continue;
                };
                let prule = rule(tree, parent);
                if !CHECKED_PARENTS.contains(&prule) {
                    continue;
                }
                let Some(name) = table_name(tree, src, n) else {
                    continue;
                };
                let last = name.parts.last().cloned().unwrap_or_default();
                let unqualified = name.parts.len() == 1;

                // What it resolves to, and whether to flag it.
                let mut relation = Relation::Opaque;
                let exempt = name.file
                    || (unqualified && ctes.contains(&last))
                    || file.local.contains_key(&last)
                    || (unqualified && is_system_table(&last))
                    || name.parts[..name.parts.len() - 1]
                        .iter()
                        .any(|q| is_system_qualifier(q) || file.attached.contains(q));
                if !exempt {
                    match catalog.resolve(&name.parts) {
                        Resolved::Missing => {
                            let span = tight(tree, name.node);
                            let fix = suggest(
                                &last,
                                catalog
                                    .tables()
                                    .iter()
                                    .map(|t| t.name.as_str())
                                    .chain(file.local.values().map(|l| l.name.as_str())),
                            )
                            .map(|s| Fix {
                                span,
                                replacement: quote_ident(s),
                            });
                            out.push(Finding {
                                code: "CAT001",
                                span,
                                fix,
                            });
                        }
                        Resolved::Found(found) => {
                            if let [only] = found.as_slice() {
                                if !only.columns.is_empty() {
                                    let at = tables
                                        .iter()
                                        .position(|t| std::ptr::eq(*t, *only))
                                        .unwrap_or_else(|| {
                                            tables.push(only);
                                            tables.len() - 1
                                        });
                                    relation = Relation::Table(at);
                                }
                            }
                        }
                        Resolved::Unchecked => {}
                    }
                }

                // What the statement calls it.
                let binding = match prule {
                    "BaseTableRef" => alias_of(tree, src, parent),
                    "NearestBaseTableRef"
                    | "TableStatement"
                    | "CopyTable"
                    | "TruncateStatement" => None,
                    _ => target_alias(tree, src, parent, n).map(|a| (a, false)),
                };
                match binding {
                    Some((alias, renamed)) => {
                        bindings.push((alias, if renamed { Relation::Opaque } else { relation }));
                    }
                    None => bindings.push((last, relation)),
                }
            }
            "TableSubquery"
            | "ValuesRef"
            | "ParensTableRef"
            | "TablePivotClause"
            | "TableUnpivotClause"
            | "TableFunctionLateralOpt"
            | "TableFunctionAliasColon" => {
                if let Some((alias, _)) = alias_of(tree, src, n) {
                    bindings.push((alias, Relation::Opaque));
                } else if let Some(f) = first_descendant(tree, n, &["TableFunctionName"]) {
                    // `FROM range(3)`: the function's name is the alias.
                    if rule(tree, n).starts_with("TableFunction") {
                        bindings.push((ident(tree.text(f, src)), Relation::Opaque));
                    }
                }
            }
            _ => {}
        }
    }

    if !check_columns {
        return;
    }

    // Third pass: qualified column references.
    for &n in &nodes {
        if rule(tree, n) != "ColumnReference" {
            continue;
        }
        let Some(&form) = tree.children(n).first() else {
            continue;
        };
        // `q.c`, or the first two parts of a longer chain (`q.c.field`).
        let (qual, col) = match rule(tree, form) {
            "TableReservedColumnName" => ("TableName", "ReservedColumnName"),
            "SchemaReservedTableColumnName" => ("SchemaName", "ReservedTableName"),
            "CatalogReservedSchemaTableColumnName" => ("CatalogName", "ReservedSchemaName"),
            _ => continue,
        };
        let (Some(qn), Some(cn)) = (
            first_descendant(tree, form, &[qual]),
            first_descendant(tree, form, &[col]),
        ) else {
            continue;
        };
        let q = ident(tree.text(qn, src));
        if shadows.contains(&q) {
            continue;
        }
        // Every relation the statement calls `q` must be the same catalog
        // table: then no scoping rule can make `q` mean anything else.
        let mut named = bindings
            .iter()
            .filter(|(name, _)| *name == q)
            .map(|(_, r)| *r);
        let Some(Relation::Table(at)) = named.next() else {
            continue;
        };
        if named.any(|r| r != Relation::Table(at)) {
            continue;
        }
        // `q.c` reads a struct column `q` when a relation in scope has one.
        if tables.iter().any(|t| t.has_column(&q)) {
            continue;
        }
        let table = tables[at];
        let c = ident(tree.text(cn, src));
        if table.has_column(&c) || c == "rowid" {
            continue;
        }
        let span = tight(tree, cn);
        let fix = suggest(&c, table.columns.iter().map(|c| c.name.as_str())).map(|s| Fix {
            span,
            replacement: quote_ident(s),
        });
        out.push(Finding {
            code: "CAT002",
            span,
            fix,
        });
    }
}

/// The candidate closest to `name`, if exactly one is close: within one
/// edit for a short name, two for a longer one.
fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let limit = match name.chars().count() {
        0..=2 => return None,
        3..=5 => 1,
        _ => 2,
    };
    let mut best: Option<(usize, &str)> = None;
    let mut tie = false;
    let mut seen: HashSet<String> = HashSet::new();
    for c in candidates {
        let lc = c.to_ascii_lowercase();
        if !seen.insert(lc.clone()) {
            continue;
        }
        let d = edit_distance(name, &lc, limit);
        // The name itself, somewhere else in the catalog, is no suggestion.
        if d == 0 || d > limit {
            continue;
        }
        match best {
            Some((b, _)) if d > b => {}
            Some((b, _)) if d == b => tie = true,
            _ => {
                best = Some((d, c));
                tie = false;
            }
        }
    }
    if tie { None } else { best.map(|(_, c)| c) }
}

/// Levenshtein distance, giving up (returning `limit + 1`) once it is
/// certain to exceed `limit`.
fn edit_distance(a: &str, b: &str, limit: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            row.push((prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        if row.iter().min().is_some_and(|&m| m > limit) {
            return limit + 1;
        }
        prev = row;
    }
    prev[b.len()]
}

/// The message for a `CAT` finding, naming what was not found and, when
/// there is a near name, what was probably meant. `MOD` messages are fixed
/// prose in the registry; these two are about one name each.
#[must_use]
pub fn message(f: &Finding, src: &str) -> String {
    let name = src
        .get(f.span.start as usize..f.span.end as usize)
        .unwrap_or_default();
    let mut m = match f.code {
        "CAT001" => format!("Table or view {name} is not in the catalog."),
        "CAT002" => format!("Column {name} is not in the table it is qualified with."),
        _ => return crate::lookup(f.code).map_or_else(String::new, |r| r.message.to_string()),
    };
    if let Some(fix) = &f.fix {
        m.push_str(&format!(" Did you mean {}?", fix.replacement));
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{Selection, analyze};

    const SHOP: &str = r#"[
        {"database":"shop","schema":"main","table":"orders","column":"id","type":"BIGINT"},
        {"database":"shop","schema":"main","table":"orders","column":"total","type":"DECIMAL(10,2)"},
        {"database":"shop","schema":"main","table":"customers","column":"id","type":"BIGINT"},
        {"database":"shop","schema":"main","table":"customers","column":"name","type":"VARCHAR"},
        {"database":"lake","schema":"raw","table":"events","column":"ts","type":"TIMESTAMP","kind":"view"}
    ]"#;

    fn shop() -> Catalog {
        Catalog::from_json_text(SHOP).expect("a snapshot")
    }

    /// `code@text` for each CAT finding in `sql`, checked against `cat`.
    fn cat_findings(sql: &str, cat: &Catalog) -> Vec<String> {
        analyze(
            sql,
            &Selection {
                catalog: Some(cat),
                ..Selection::default()
            },
        )
        .into_iter()
        .filter(|f| f.code.starts_with("CAT"))
        .map(|f| {
            format!(
                "{}@{}",
                f.code,
                &sql[f.span.start as usize..f.span.end as usize]
            )
        })
        .collect()
    }

    #[test]
    fn reads_rows_and_the_object_form() {
        let cat = shop();
        assert_eq!(cat.tables().len(), 3);
        let orders = &cat.tables()[0];
        assert_eq!((orders.name.as_str(), orders.columns.len()), ("orders", 2));
        assert!(!orders.is_view);
        assert!(cat.tables()[2].is_view);

        let obj = format!(
            r#"{{"columns": {SHOP}, "functions": [{{"name":"my_macro","kind":"macro"}}]}}"#
        );
        let cat = Catalog::from_json_text(&obj).expect("the object form");
        assert_eq!(cat.tables().len(), 3);
        assert_eq!(cat.functions()[0].name, "my_macro");

        // Exactly what `duckdb -json` prints for an empty database.
        assert!(Catalog::from_json_text("[]").unwrap().tables().is_empty());
        // Not a snapshot.
        assert!(Catalog::from_json_text("{}").is_err());
        assert!(Catalog::from_json_text("[1]").is_err());
        assert!(Catalog::from_json_text(r#"[{"column":"a"}]"#).is_err());
        assert!(Catalog::from_json_text("not json").is_err());
    }

    #[test]
    fn resolves_by_what_the_catalog_knows() {
        let cat = shop();
        let parts = |s: &str| -> Vec<String> { s.split('.').map(str::to_string).collect() };
        assert!(matches!(cat.resolve(&parts("orders")), Resolved::Found(t) if t.len() == 1));
        assert!(matches!(cat.resolve(&parts("events")), Resolved::Found(_)));
        assert!(matches!(
            cat.resolve(&parts("main.orders")),
            Resolved::Found(_)
        ));
        assert!(matches!(
            cat.resolve(&parts("shop.orders")),
            Resolved::Found(_)
        ));
        assert!(matches!(
            cat.resolve(&parts("lake.raw.events")),
            Resolved::Found(_)
        ));
        assert_eq!(cat.resolve(&parts("nope")), Resolved::Missing);
        assert_eq!(cat.resolve(&parts("raw.orders")), Resolved::Missing);
        assert_eq!(cat.resolve(&parts("lake.main.events")), Resolved::Missing);
        // A qualifier the catalog has never heard of: it cannot say.
        assert_eq!(cat.resolve(&parts("elsewhere.orders")), Resolved::Unchecked);
        assert_eq!(
            cat.resolve(&parts("elsewhere.main.orders")),
            Resolved::Unchecked
        );
    }

    #[test]
    fn nothing_without_a_catalog() {
        let f = crate::analysis::analyze_source("SELECT x.y FROM no_such_table x;", None);
        assert!(f.iter().all(|f| !f.code.starts_with("CAT")), "{f:?}");
    }

    #[test]
    fn flags_a_missing_table_and_a_missing_column() {
        let cat = shop();
        assert_eq!(cat_findings("SELECT * FROM ordrs;", &cat), ["CAT001@ordrs"]);
        assert_eq!(
            cat_findings("SELECT o.totl FROM orders o;", &cat),
            ["CAT002@totl"]
        );
        assert!(cat_findings("SELECT o.total, o.ID FROM Orders o;", &cat).is_empty());
    }

    #[test]
    fn suggests_the_one_near_name() {
        let cat = shop();
        let sel = Selection {
            catalog: Some(&cat),
            ..Selection::default()
        };
        let sql = "SELECT c.nmae FROM custmers c JOIN orders o ON o.id = c.id WHERE o.totl > 1;";
        let f = analyze(sql, &sel);
        let fix = |code: &str| {
            f.iter()
                .find(|f| f.code == code)
                .and_then(|f| f.fix.as_ref())
                .map(|x| x.replacement.as_str())
        };
        assert_eq!(fix("CAT001"), Some("customers"));
        // `c` is not a catalog table here (its table is unknown), so only
        // `o.totl` is a column finding.
        assert_eq!(fix("CAT002"), Some("total"));
        assert_eq!(f.iter().filter(|f| f.code == "CAT002").count(), 1);
        assert_eq!(
            message(f.iter().find(|f| f.code == "CAT001").unwrap(), sql),
            "Table or view custmers is not in the catalog. Did you mean customers?"
        );
        // Nothing near: no suggestion, and never the same name again.
        let f = analyze("SELECT * FROM zzz, lake.main.orders;", &sel);
        assert_eq!(f.len(), 2);
        assert!(f.iter().all(|f| f.fix.is_none()), "{f:?}");
        // Two equally near names: no guess.
        assert_eq!(suggest("ab_", ["abc", "abd"].into_iter()), None);
        assert_eq!(suggest("abx", ["abc", "ABC"].into_iter()), Some("abc"));
    }

    #[test]
    fn a_suggestion_is_never_applied_by_fix() {
        let cat = shop();
        let sel = Selection {
            catalog: Some(&cat),
            ..Selection::default()
        };
        let done = crate::fix::apply_until_stable_with(
            "SELECT * FROM ordrs;",
            &sel,
            crate::fix::Safety::IncludeUnsafe,
        );
        assert_eq!(done.output, None);
    }

    #[test]
    fn a_name_the_catalog_has_twice_is_found_but_its_columns_are_not_judged() {
        let cat = Catalog::from_json_text(
            r#"[{"database":"a","schema":"main","table":"t","column":"x","type":"INT"},
                {"database":"b","schema":"main","table":"t","column":"y","type":"INT"}]"#,
        )
        .unwrap();
        assert!(cat_findings("SELECT t.z FROM t;", &cat).is_empty());
        // Qualified, it is one table again.
        assert_eq!(cat_findings("SELECT t.z FROM a.t;", &cat), ["CAT002@z"]);
    }

    #[test]
    fn a_table_listed_without_columns_exists_and_is_not_checked() {
        let cat = Catalog::from_json_text(r#"[{"database":"d","schema":"main","table":"wide"}]"#)
            .unwrap();
        assert!(cat_findings("SELECT w.anything FROM wide w;", &cat).is_empty());
        assert_eq!(cat_findings("SELECT * FROM wyde;", &cat), ["CAT001@wyde"]);
    }

    #[test]
    fn what_the_file_defines_counts_even_in_a_statement_that_does_not_parse() {
        let cat = shop();
        // The CREATE is broken (so the file takes the per-statement path),
        // and still makes `staging` a name of this file.
        let sql = "CREATE TABLE staging (id INT,, );\nSELECT * FROM staging;\nSELECT * FROM ordrs;";
        assert_eq!(cat_findings(sql, &cat), ["CAT001@ordrs"]);
        // Spans on that path are file-absolute.
        let f = analyze(
            sql,
            &Selection {
                catalog: Some(&cat),
                ..Selection::default()
            },
        );
        assert!(f.iter().any(|f| f.code == "PRS001"), "{f:?}");
        let c = f.iter().find(|f| f.code == "CAT001").unwrap();
        assert_eq!(&sql[c.span.start as usize..c.span.end as usize], "ordrs");
        let fix = c.fix.as_ref().unwrap();
        assert_eq!(
            &sql[fix.span.start as usize..fix.span.end as usize],
            "ordrs"
        );
    }

    #[test]
    fn a_file_that_changes_what_names_mean_is_not_checked() {
        let cat = shop();
        let bad = "SELECT * FROM ordrs;";
        assert_eq!(cat_findings(bad, &cat).len(), 1);
        for head in [
            "USE elsewhere;",
            "USE elsewhere.main;",
            "SET search_path = 'x,y';",
            "SET SESSION schema = 'x';",
            "IMPORT DATABASE 'dump';",
            "COPY FROM DATABASE a TO b;",
            ".read other.sql\n;",
            "ATTACH getvariable('p');",
        ] {
            assert!(
                cat_findings(&format!("{head}\n{bad}"), &cat).is_empty(),
                "{head}"
            );
        }
        // A USE of something the catalog covers changes nothing: an
        // unqualified name is looked up everywhere anyway.
        for head in ["USE shop;", "USE lake.raw;", "USE main;"] {
            assert_eq!(
                cat_findings(&format!("{head}\n{bad}"), &cat).len(),
                1,
                "{head}"
            );
        }
    }

    #[test]
    fn a_database_the_file_attaches_is_the_files_business() {
        let cat = shop();
        // `lake` is in the catalog, but this file attaches its own.
        assert!(
            cat_findings(
                "ATTACH 'other.duckdb' AS lake;\nSELECT * FROM lake.raw.nope;",
                &cat
            )
            .is_empty()
        );
        // Named after the file when there is no alias.
        assert!(
            cat_findings(
                "ATTACH 'data/lake.duckdb';\nSELECT * FROM lake.raw.nope;",
                &cat
            )
            .is_empty()
        );
        assert_eq!(
            cat_findings(
                "ATTACH 'x.duckdb' AS x;\nSELECT * FROM lake.raw.nope;",
                &cat
            ),
            ["CAT001@nope"]
        );
    }

    #[test]
    fn the_file_scan_reads_definitions() {
        let ctx = FileContext::scan(
            "CREATE TABLE a (id INT, \"Full Name\" VARCHAR, PRIMARY KEY (id), CHECK (id > 0));\n\
             CREATE OR REPLACE TEMP VIEW s.b (x, y) AS SELECT 1, 2;\n\
             CREATE TABLE IF NOT EXISTS c AS SELECT 1;\n\
             CREATE TABLE d (n DECIMAL(10, 2), m INT[]);\n\
             ALTER TABLE d ADD COLUMN z INT;\n\
             CREATE MACRO m() AS TABLE SELECT 1;\n\
             CREATE INDEX i ON a (id);",
        );
        let cols = |n: &str| ctx.local.get(n).and_then(|l| l.columns.clone());
        assert_eq!(
            cols("a"),
            Some(vec!["id".to_string(), "Full Name".to_string()])
        );
        assert_eq!(cols("b"), Some(vec!["x".to_string(), "y".to_string()]));
        assert_eq!(cols("c"), None);
        assert!(ctx.local.contains_key("c"));
        // Altered after it was created: its columns are no longer known.
        assert_eq!(cols("d"), None);
        assert_eq!(ctx.local.len(), 4, "{:?}", ctx.local.keys());
        assert!(!ctx.opaque);
    }

    #[test]
    fn quotes_only_what_needs_it() {
        assert_eq!(quote_ident("orders"), "orders");
        assert_eq!(quote_ident("Order Date"), "\"Order Date\"");
        assert_eq!(quote_ident("select"), "\"select\"");
        assert_eq!(quote_ident("9lives"), "\"9lives\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
