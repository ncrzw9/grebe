//! An optional catalog snapshot, and the `CAT` rules that read it.
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
//! CAT003 to CAT010 read what a snapshot says of a table beyond its
//! columns (types, nullability, row estimate, keys, indexes) and are in
//! the `meta` module. Each is silent when the snapshot does not carry what
//! it needs.
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
//! Extra keys are ignored (`"kind": "table" | "view"` and `"nullable":
//! true | false` are read when present). A row without a `"column"` lists a
//! table whose columns are not known: it exists for CAT001 and is never
//! checked by CAT002.
//!
//! An object is accepted too: `{"columns": [...], "tables": [...],
//! "functions": [{"name", "kind"}]}`. The functions only feed completion.
//! `"tables"` says what is known of each table beyond its columns, which
//! CAT001 and CAT002 do not need and CAT003 to CAT010 read (see
//! [`Table`]):
//!
//! ```json
//! {"database":"memory","schema":"main","table":"orders","kind":"table",
//!  "rows":120000,"primary_key":["id"],"unique":[["code"]],
//!  "indexes":[["customer_id","placed"]]}
//! ```
//!
//! Everything in it but the name is optional. It may also carry the
//! table's columns itself (`"columns"`, `"types"` and `"nullable"`, three
//! arrays side by side), which is how an editor sends a large database.
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
use std::sync::Arc;

use grebe_syntax::Span;
use grebe_syntax::cst::{NodeId, Tree};
use grebe_syntax::token::{Token, TokenKind};

use crate::detect::{Finding, Fix};
use crate::json::Json;

mod meta;

/// The query whose `-json` output is a snapshot.
pub const SNAPSHOT_SQL: &str = "SELECT database_name AS database, schema_name AS schema, \
    table_name AS \"table\", column_name AS \"column\", data_type AS type \
    FROM duckdb_columns() WHERE NOT internal";

/// [`SNAPSHOT_SQL`] with each column's `nullable` as well.
pub const SNAPSHOT_NULLABLE_SQL: &str = "SELECT database_name AS database, schema_name AS schema, \
    table_name AS \"table\", column_name AS \"column\", data_type AS type, \
    is_nullable AS nullable \
    FROM duckdb_columns() WHERE NOT internal";

/// The query whose `-json` output is a snapshot's `"tables"`: for every
/// table and view its kind, DuckDB's row estimate, the primary key, the
/// unique constraints and unique indexes, and the other indexes. An index's
/// columns are read off DuckDB's own text for them (`[a, b]`), so an
/// indexed expression comes through as that text.
pub const TABLES_SQL: &str = "WITH keys AS (\
    SELECT database_name AS d, schema_name AS s, table_name AS t, \
    first(constraint_column_names) FILTER (constraint_type = 'PRIMARY KEY') AS pk, \
    list(constraint_column_names) FILTER (constraint_type = 'UNIQUE') AS uq \
    FROM duckdb_constraints() WHERE constraint_type IN ('PRIMARY KEY', 'UNIQUE') GROUP BY ALL), \
    idx AS (\
    SELECT database_name AS d, schema_name AS s, table_name AS t, \
    list(string_split(expressions[2:-2], ', ')) FILTER (NOT is_unique) AS ix, \
    list(string_split(expressions[2:-2], ', ')) FILTER (is_unique AND NOT is_primary) AS ux \
    FROM duckdb_indexes() GROUP BY ALL) \
    SELECT t.database_name AS database, t.schema_name AS schema, t.table_name AS \"table\", \
    'table' AS kind, t.estimated_size AS rows, k.pk AS primary_key, \
    coalesce(k.uq, []) || coalesce(i.ux, []) AS \"unique\", i.ix AS indexes \
    FROM duckdb_tables() t \
    LEFT JOIN keys k ON k.d = t.database_name AND k.s = t.schema_name AND k.t = t.table_name \
    LEFT JOIN idx i ON i.d = t.database_name AND i.s = t.schema_name AND i.t = t.table_name \
    WHERE NOT t.internal \
    UNION ALL SELECT database_name, schema_name, view_name, 'view', NULL, NULL, NULL, NULL \
    FROM duckdb_views() WHERE NOT internal";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    /// Whether the column may hold NULL; `None` when the snapshot did not
    /// say.
    pub nullable: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub is_view: bool,
    /// In the table's own order. Empty when the snapshot listed the table
    /// without its columns.
    pub columns: Vec<Column>,
    /// About how many rows it holds, when the snapshot said: an estimate
    /// (DuckDB's `estimated_size`, PostgreSQL's `reltuples`), not a count.
    pub rows: Option<u64>,
    /// The primary key's columns, in key order; empty when there is none
    /// or the snapshot did not say.
    pub primary_key: Vec<String>,
    /// The column lists of the `UNIQUE` constraints and unique indexes.
    pub unique: Vec<Vec<String>>,
    /// The other indexes: each one's columns in index order. An indexed
    /// expression is there as its text.
    pub indexes: Vec<Vec<String>>,
    /// Indexes into `columns`, ordered by lowercase name: a column is found
    /// by binary search. Built with the part; empty until then.
    by_name: Vec<u32>,
}

/// `a` against `b` as lowercase ASCII bytes.
fn cmp_lower(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

impl Table {
    /// A table with nothing known about it but its name.
    #[must_use]
    pub fn new(database: &str, schema: &str, name: &str) -> Self {
        Table {
            database: database.to_string(),
            schema: schema.to_string(),
            name: name.to_string(),
            ..Table::default()
        }
    }

    /// Build the name index. A column named twice (any case) keeps its
    /// first mention: the readers push every column they see and leave the
    /// duplicates to be found here, in one sort, instead of searching the
    /// table for each one (which made a table of 100,000 columns take
    /// seconds to load).
    fn index(&mut self) {
        let mut order = self.sorted();
        let same = |a: u32, b: u32| {
            cmp_lower(
                &self.columns[a as usize].name,
                &self.columns[b as usize].name,
            )
            .is_eq()
        };
        if order.windows(2).any(|w| same(w[0], w[1])) {
            let mut drop = vec![false; self.columns.len()];
            for w in order.windows(2) {
                if same(w[0], w[1]) {
                    drop[w[1] as usize] = true;
                }
            }
            let mut at = 0;
            self.columns.retain(|_| {
                at += 1;
                !drop[at - 1]
            });
            order = self.sorted();
        }
        self.by_name = order;
    }

    fn sorted(&self) -> Vec<u32> {
        let mut order: Vec<u32> = (0..self.columns.len() as u32).collect();
        order.sort_by(|&a, &b| {
            cmp_lower(
                &self.columns[a as usize].name,
                &self.columns[b as usize].name,
            )
            .then(a.cmp(&b))
        });
        order
    }

    /// The column called `name` (any case).
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        if self.by_name.len() != self.columns.len() {
            // Still being read: not indexed yet.
            return self
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name));
        }
        self.by_name
            .binary_search_by(|&i| cmp_lower(&self.columns[i as usize].name, name))
            .ok()
            .map(|at| &self.columns[self.by_name[at] as usize])
    }

    #[must_use]
    pub fn has_column(&self, name: &str) -> bool {
        self.column(name).is_some()
    }

    /// The type of the column called `name`, as the snapshot spells it.
    #[must_use]
    pub fn column_type(&self, name: &str) -> Option<&str> {
        self.column(name).map(|c| c.data_type.as_str())
    }

    /// Whether the column called `name` may hold NULL. `None` when there is
    /// no such column or the snapshot did not say.
    #[must_use]
    pub fn is_nullable(&self, name: &str) -> Option<bool> {
        self.column(name).and_then(|c| c.nullable)
    }

    /// About how many rows the table holds, when known.
    #[must_use]
    pub fn row_estimate(&self) -> Option<u64> {
        self.rows
    }

    /// Every column list that identifies a row: the primary key, then the
    /// unique constraints and indexes.
    pub fn unique_keys(&self) -> impl Iterator<Item = &[String]> {
        std::iter::once(self.primary_key.as_slice())
            .filter(|k| !k.is_empty())
            .chain(self.unique.iter().map(Vec::as_slice))
    }

    /// Do `columns` (any case, any order) hold every column of some key?
    /// Then at most one row matches a value for each of them.
    #[must_use]
    pub fn is_unique_on(&self, columns: &[&str]) -> bool {
        self.unique_keys().any(|key| {
            key.iter()
                .all(|k| columns.iter().any(|c| c.eq_ignore_ascii_case(k)))
        })
    }

    /// The first column of every index, the primary key and the unique
    /// ones included: a filter on one of these can use an index.
    pub fn indexed_leading(&self) -> impl Iterator<Item = &str> {
        self.unique_keys()
            .chain(self.indexes.iter().map(Vec::as_slice))
            .filter_map(|k| k.first().map(String::as_str))
    }

    /// Is the column called `name` the first column of an index?
    #[must_use]
    pub fn is_indexed_leading(&self, name: &str) -> bool {
        self.indexed_leading().any(|c| c.eq_ignore_ascii_case(name))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    /// As `duckdb_functions().function_type` spells it: `scalar`,
    /// `aggregate`, `table`, `macro`, `table_macro`, ...
    pub kind: String,
}

/// Which characters a name has, as a set of 64 buckets. One edit changes at
/// most two of them, so two names within `d` edits differ in at most `2d`
/// bits: a scan of these turns away nearly every name before any distance
/// is computed.
fn letters(name: &str) -> u64 {
    name.chars()
        .fold(0, |m, c| m | 1 << (c.to_ascii_lowercase() as u32 % 64))
}

/// One database's share of a catalog: its tables and views, indexed for
/// case-insensitive lookup, and its macros. Built once and then only read,
/// so a catalog can swap one database's part without touching the others
/// (an editor sends a large attached database once, and the small local
/// ones after every run).
#[derive(Clone, Debug, Default)]
pub struct Part {
    /// The database's name, as first written.
    database: String,
    tables: Vec<Table>,
    /// Lowercase table name -> indexes into `tables`.
    by_name: HashMap<String, Vec<u32>>,
    /// Lowercase schema name -> indexes into `tables`, in `sorted` order.
    by_schema: HashMap<String, Vec<u32>>,
    /// Indexes into `tables`, ordered by lowercase table name: a prefix is
    /// a range of it.
    sorted: Vec<u32>,
    /// [`letters`] of each table's name, beside `tables`.
    letters: Vec<u64>,
    functions: Vec<Function>,
}

impl PartialEq for Part {
    /// The indexes follow from the tables, so they are not compared.
    fn eq(&self, other: &Self) -> bool {
        self.database.eq_ignore_ascii_case(&other.database)
            && self.tables == other.tables
            && self.functions == other.functions
    }
}

impl Eq for Part {}

impl Part {
    /// Build the part of `database` from its tables. Tables with the same
    /// schema and name (case-insensitively) are merged; each table's
    /// `database` is set to `database`.
    #[must_use]
    pub fn new(database: &str, tables: Vec<Table>, functions: Vec<Function>) -> Self {
        let mut part = Part {
            database: database.to_string(),
            functions,
            ..Part::default()
        };
        let mut index: HashMap<(String, String), usize> = HashMap::new();
        for mut t in tables {
            let key = (lower(&t.schema), lower(&t.name));
            if let Some(&i) = index.get(&key) {
                let have = &mut part.tables[i];
                have.by_name.clear();
                have.columns.extend(t.columns);
                have.is_view |= t.is_view;
                have.rows = have.rows.or(t.rows);
                if have.primary_key.is_empty() {
                    have.primary_key = t.primary_key;
                }
                have.unique.extend(t.unique);
                have.indexes.extend(t.indexes);
                continue;
            }
            index.insert(key, part.tables.len());
            if t.database != database {
                t.database = database.to_string();
            }
            part.tables.push(t);
        }
        part.index();
        part
    }

    fn index(&mut self) {
        for t in &mut self.tables {
            t.index();
        }
        let keys: Vec<String> = self.tables.iter().map(|t| lower(&t.name)).collect();
        let mut sorted: Vec<u32> = (0..self.tables.len() as u32).collect();
        sorted.sort_by(|&a, &b| keys[a as usize].cmp(&keys[b as usize]).then(a.cmp(&b)));
        for &i in &sorted {
            self.by_schema
                .entry(lower(&self.tables[i as usize].schema))
                .or_default()
                .push(i);
        }
        self.letters = keys.iter().map(|k| letters(k)).collect();
        for (i, key) in keys.into_iter().enumerate() {
            self.by_name.entry(key).or_default().push(i as u32);
        }
        self.sorted = sorted;
    }

    /// Read a part from JSON, in either or both of two forms:
    ///
    /// - `columns`: an array of snapshot rows, one per column (a row's
    ///   `"database"` may be left out; when present it must be this one);
    /// - `tables`: an array with one object per table, `{"schema", "table",
    ///   "kind"?, "rows"?, "primary_key"?, "unique"?, "indexes"?,
    ///   "columns"?, "types"?, "nullable"?}`. `columns`, `types` and
    ///   `nullable` are arrays side by side, the table's columns in order:
    ///   the same as the rows say, in a quarter of the bytes. Without them
    ///   the object only adds what it knows to a table the rows list.
    ///
    /// `functions` is an optional array of `{"name", "kind"}`.
    pub fn from_json(
        database: &str,
        columns: Option<&Json>,
        tables: Option<&Json>,
        functions: Option<&Json>,
    ) -> Result<Self, String> {
        let mut reader = RowReader::default();
        reader.read(columns, tables, Some(database))?;
        let functions = read_functions(functions);
        let mut parts = reader.finish();
        Ok(match parts.pop() {
            Some(mut part) => {
                part.functions = functions;
                part
            }
            None => Part::new(database, Vec::new(), functions),
        })
    }

    #[must_use]
    pub fn database(&self) -> &str {
        &self.database
    }

    #[must_use]
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    #[must_use]
    pub fn functions(&self) -> &[Function] {
        &self.functions
    }

    /// How many columns the part lists.
    #[must_use]
    pub fn column_count(&self) -> usize {
        self.tables.iter().map(|t| t.columns.len()).sum()
    }

    fn named(&self, lowercase: &str) -> impl Iterator<Item = &Table> {
        self.by_name
            .get(lowercase)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|&i| &self.tables[i as usize])
    }

    /// The tables whose name starts with `prefix` (lowercase), in name
    /// order; in `schema` (lowercase) only, when one is given.
    fn with_prefix<'a, 'p>(
        &'a self,
        prefix: &'p str,
        schema: Option<&str>,
    ) -> impl Iterator<Item = &'a Table> + use<'a, 'p> {
        let starts = move |t: &Table| {
            let n = t.name.as_bytes();
            n.len() >= prefix.len() && n[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        };
        let (list, ranged): (&[u32], bool) = match schema {
            Some(s) => (self.by_schema.get(s).map_or(&[][..], Vec::as_slice), false),
            None => (&self.sorted, true),
        };
        // Without a schema the matches are one run of the sorted list.
        let from = if ranged {
            list.partition_point(|&i| cmp_lower(&self.tables[i as usize].name, prefix).is_lt())
        } else {
            0
        };
        list[from..]
            .iter()
            .map(move |&i| &self.tables[i as usize])
            .take_while(move |t| !ranged || starts(t))
            .filter(move |t| ranged || starts(t))
    }
}

/// Snapshot JSON gathered into tables, one list per database in the order
/// first seen.
#[derive(Default)]
struct RowReader {
    databases: Vec<(String, Vec<Table>)>,
    /// Lowercase database name -> index into `databases`, and its tables:
    /// (schema, table), lowercase -> index into that list.
    index: HashMap<String, (usize, TableIndex)>,
}

/// (schema, table), lowercase -> where the table is in its list.
type TableIndex = HashMap<(String, String), usize>;

/// `["a", "b"]` as strings; anything else in the array is skipped.
fn strings(v: Option<&Json>) -> Vec<String> {
    v.and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

/// `[["a"], ["b", "c"]]`: the non-empty lists.
fn string_lists(v: Option<&Json>) -> Vec<Vec<String>> {
    v.and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .map(|l| strings(Some(l)))
        .filter(|l| !l.is_empty())
        .collect()
}

impl RowReader {
    /// Read `columns` (rows) and then `tables` (one object per table).
    /// `database` is the one database everything belongs to, when the
    /// JSON is one database's part.
    fn read(
        &mut self,
        columns: Option<&Json>,
        tables: Option<&Json>,
        database: Option<&str>,
    ) -> Result<(), String> {
        let check = |what: &str, n: usize, v: &Json| -> Result<(), String> {
            if v.as_object().is_none() {
                return Err(format!("{what} {}: expected an object", n + 1));
            }
            if let (Some(d), Some(only)) = (v.get("database").and_then(Json::as_str), database) {
                if !d.eq_ignore_ascii_case(only) {
                    return Err(format!(
                        "{what} {}: database \"{d}\" in the part for \"{only}\"",
                        n + 1
                    ));
                }
            }
            Ok(())
        };
        if let Some(rows) = columns {
            let rows = rows.as_array().ok_or("expected an array of column rows")?;
            for (n, row) in rows.iter().enumerate() {
                check("row", n, row)?;
                self.row(n, row, database)?;
            }
        }
        if let Some(tables) = tables {
            let tables = tables.as_array().ok_or("expected an array of tables")?;
            for (n, t) in tables.iter().enumerate() {
                check("table", n, t)?;
                self.table(n, t, database)?;
            }
        }
        Ok(())
    }

    /// The table `database.schema.name`, added if it is new.
    fn entry(&mut self, database: &str, schema: &str, name: &str) -> &mut Table {
        let databases = &mut self.databases;
        let (d, tables_at) = self.index.entry(lower(database)).or_insert_with(|| {
            databases.push((database.to_string(), Vec::new()));
            (databases.len() - 1, HashMap::new())
        });
        let (stored, tables) = &mut databases[*d];
        let i = *tables_at
            .entry((lower(schema), lower(name)))
            .or_insert_with(|| {
                tables.push(Table::new(stored, schema, name));
                tables.len() - 1
            });
        &mut tables[i]
    }

    fn row(&mut self, n: usize, row: &Json, database: Option<&str>) -> Result<(), String> {
        let field = |k: &str| row.get(k).and_then(Json::as_str);
        let Some(name) = field("table") else {
            return Err(format!("row {}: no \"table\"", n + 1));
        };
        let database = database.unwrap_or_else(|| field("database").unwrap_or(""));
        let table = self.entry(database, field("schema").unwrap_or(""), name);
        if field("kind").is_some_and(|k| k.eq_ignore_ascii_case("view")) {
            table.is_view = true;
        }
        if let Some(column) = field("column") {
            // A repeated column is dropped when the table is indexed.
            table.columns.push(Column {
                name: column.to_string(),
                data_type: field("type").unwrap_or("").to_string(),
                nullable: match row.get("nullable") {
                    Some(Json::Bool(b)) => Some(*b),
                    _ => None,
                },
            });
        }
        Ok(())
    }

    fn table(&mut self, n: usize, t: &Json, database: Option<&str>) -> Result<(), String> {
        let field = |k: &str| t.get(k).and_then(Json::as_str);
        let Some(name) = field("table") else {
            return Err(format!("table {}: no \"table\"", n + 1));
        };
        let database = database.unwrap_or_else(|| field("database").unwrap_or(""));
        let table = self.entry(database, field("schema").unwrap_or(""), name);
        if let Some(kind) = field("kind") {
            table.is_view = kind.eq_ignore_ascii_case("view");
        }
        if let Some(rows) = t.get("rows").and_then(Json::as_f64) {
            if rows >= 0.0 {
                table.rows = Some(rows as u64);
            }
        }
        if t.get("primary_key").is_some() {
            table.primary_key = strings(t.get("primary_key"));
        }
        if t.get("unique").is_some() {
            table.unique = string_lists(t.get("unique"));
        }
        if t.get("indexes").is_some() {
            table.indexes = string_lists(t.get("indexes"));
        }
        let names = t
            .get("columns")
            .and_then(Json::as_array)
            .unwrap_or_default();
        let types = t.get("types").and_then(Json::as_array).unwrap_or_default();
        let nullable = t
            .get("nullable")
            .and_then(Json::as_array)
            .unwrap_or_default();
        let fresh = table.columns.is_empty();
        if fresh {
            table.columns.reserve_exact(names.len());
        }
        for (i, c) in names.iter().enumerate() {
            let Some(c) = c.as_str() else {
                return Err(format!("table {}: column {} is not a string", n + 1, i + 1));
            };
            let null = match nullable.get(i) {
                Some(Json::Bool(b)) => Some(*b),
                _ => None,
            };
            // A column the rows already listed only learns what is new.
            if let Some(have) = table
                .columns
                .iter_mut()
                .filter(|_| !fresh)
                .find(|h| h.name.eq_ignore_ascii_case(c))
            {
                have.nullable = have.nullable.or(null);
                continue;
            }
            table.columns.push(Column {
                name: c.to_string(),
                data_type: types
                    .get(i)
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_string(),
                nullable: null,
            });
        }
        Ok(())
    }

    fn finish(self) -> Vec<Part> {
        self.databases
            .into_iter()
            .map(|(database, tables)| {
                let mut part = Part {
                    database,
                    tables,
                    ..Part::default()
                };
                part.index();
                part
            })
            .collect()
    }
}

fn read_functions(functions: Option<&Json>) -> Vec<Function> {
    functions
        .and_then(Json::as_array)
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
        .collect()
}

/// A catalog snapshot, indexed for case-insensitive lookup: one [`Part`]
/// per database, shared, so that copying a catalog or replacing one
/// database's part costs nothing for the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    /// In the order first seen.
    parts: Vec<Arc<Part>>,
    /// Functions that belong to no one database (a whole snapshot's).
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
        let mut by_database: Vec<(String, Vec<Table>)> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for t in tables {
            let i = *at.entry(lower(&t.database)).or_insert_with(|| {
                by_database.push((t.database.clone(), Vec::new()));
                by_database.len() - 1
            });
            by_database[i].1.push(t);
        }
        Catalog {
            parts: by_database
                .into_iter()
                .map(|(d, tables)| Arc::new(Part::new(&d, tables, Vec::new())))
                .collect(),
            functions,
        }
    }

    /// Read a snapshot: an array of column rows, or an object with
    /// `"columns"` (those rows) and optionally `"tables"` (one object per
    /// table, see [`Part::from_json`]; each names its `"database"`) and
    /// `"functions"`.
    pub fn from_json(v: &Json) -> Result<Self, String> {
        let (columns, tables, functions) = match v {
            Json::Array(_) => (Some(v), None, None),
            Json::Object(_) => {
                let columns = v.get("columns").filter(|a| a.as_array().is_some());
                if columns.is_none() {
                    return Err("expected a \"columns\" array".into());
                }
                (columns, v.get("tables"), v.get("functions"))
            }
            _ => return Err("expected an array of column rows".into()),
        };
        let mut reader = RowReader::default();
        reader.read(columns, tables, None)?;
        Ok(Catalog {
            parts: reader.finish().into_iter().map(Arc::new).collect(),
            functions: read_functions(functions),
        })
    }

    /// Read a snapshot from JSON text.
    pub fn from_json_text(text: &str) -> Result<Self, String> {
        let v = crate::json::parse(text).map_err(|e| e.0)?;
        Self::from_json(&v)
    }

    /// Every table and view, database by database.
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        self.parts.iter().flat_map(|p| p.tables.iter())
    }

    /// How many tables and views there are.
    #[must_use]
    pub fn table_count(&self) -> usize {
        self.parts.iter().map(|p| p.tables.len()).sum()
    }

    pub fn functions(&self) -> impl Iterator<Item = &Function> {
        self.functions
            .iter()
            .chain(self.parts.iter().flat_map(|p| p.functions.iter()))
    }

    /// The parts, one per database, in the order first seen.
    #[must_use]
    pub fn parts(&self) -> &[Arc<Part>] {
        &self.parts
    }

    /// The part of `database` (any case).
    #[must_use]
    pub fn part(&self, database: &str) -> Option<&Arc<Part>> {
        self.parts
            .iter()
            .find(|p| p.database.eq_ignore_ascii_case(database))
    }

    /// Put `part` in place of the part its database has now, or add it.
    /// False, and nothing changes, when the catalog already holds an equal
    /// one.
    pub fn set_part(&mut self, part: Part) -> bool {
        match self
            .parts
            .iter_mut()
            .find(|p| p.database.eq_ignore_ascii_case(&part.database))
        {
            Some(have) if **have == part => false,
            Some(have) => {
                *have = Arc::new(part);
                true
            }
            None => {
                self.parts.push(Arc::new(part));
                true
            }
        }
    }

    /// Take the part of `database` out. False when there was none.
    pub fn remove_part(&mut self, database: &str) -> bool {
        let before = self.parts.len();
        self.parts
            .retain(|p| !p.database.eq_ignore_ascii_case(database));
        self.parts.len() != before
    }

    /// Is `lowercase` the name of a database in the snapshot?
    #[must_use]
    pub fn has_database(&self, lowercase: &str) -> bool {
        self.part(lowercase).is_some()
    }

    /// Is `lowercase` the name of a schema in the snapshot?
    #[must_use]
    pub fn has_schema(&self, lowercase: &str) -> bool {
        self.parts
            .iter()
            .any(|p| p.by_schema.contains_key(lowercase))
    }

    /// Database names, as written.
    pub fn databases(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().map(|p| p.database.as_str())
    }

    /// Each schema once per database: `(schema, database)` as written.
    pub fn schemas(&self) -> impl Iterator<Item = (&str, &str)> {
        self.parts.iter().flat_map(|p| {
            p.by_schema.values().filter_map(|v| {
                let t = &p.tables[*v.first()? as usize];
                Some((t.schema.as_str(), p.database.as_str()))
            })
        })
    }

    /// Tables whose name starts with `prefix` (lowercase), each database's
    /// in name order. `within` narrows them: `[q]` to the database or the
    /// schemas called `q`, `[d, s]` to schema `s` of database `d` (all
    /// lowercase). At most `limit` names are taken from any one database,
    /// which is enough for the first `limit` of them all; the flag says
    /// some were left out.
    #[must_use]
    pub fn tables_with_prefix(
        &self,
        prefix: &str,
        within: &[String],
        limit: usize,
    ) -> (Vec<&Table>, bool) {
        let mut out = Vec::new();
        let mut cut = false;
        for p in &self.parts {
            let is_db = |q: &str| p.database.eq_ignore_ascii_case(q);
            let schema = match within {
                [] => None,
                [q] if is_db(q) => None,
                [q] => Some(q.as_str()),
                [d, s] if is_db(d) => Some(s.as_str()),
                _ => continue,
            };
            // Names, not tables: a name that is in fifty schemas is one
            // thing to offer.
            let mut names = 0;
            let mut last: Option<&Table> = None;
            for t in p.with_prefix(prefix, schema) {
                if !last.is_some_and(|l| l.name.eq_ignore_ascii_case(&t.name)) {
                    if names == limit {
                        cut = true;
                        break;
                    }
                    names += 1;
                }
                last = Some(t);
                out.push(t);
            }
        }
        (out, cut)
    }

    /// The names of the tables that could be within `edits` edits of
    /// `name`: those whose letters differ from its letters little enough.
    /// Never leaves out a name that is that close; what it lets through
    /// still has to be measured.
    pub fn names_near<'a>(&'a self, name: &str, edits: usize) -> impl Iterator<Item = &'a str> {
        let wanted = letters(name);
        let most = 2 * edits as u32;
        self.parts.iter().flat_map(move |p| {
            p.letters
                .iter()
                .zip(&p.tables)
                .filter(move |(l, _)| (**l ^ wanted).count_ones() <= most)
                .map(|(_, t)| t.name.as_str())
        })
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
        let keep: Vec<&Table> = match parts.len() {
            1 => self.parts.iter().flat_map(|p| p.named(name)).collect(),
            2 => {
                let q = &parts[0];
                if !self.has_database(q) && !self.has_schema(q) {
                    return Resolved::Unchecked;
                }
                self.parts
                    .iter()
                    .flat_map(|p| p.named(name))
                    .filter(|t| {
                        t.database.eq_ignore_ascii_case(q) || t.schema.eq_ignore_ascii_case(q)
                    })
                    .collect()
            }
            3 => {
                let (d, s) = (&parts[0], &parts[1]);
                let Some(part) = self.part(d) else {
                    return Resolved::Unchecked;
                };
                part.named(name)
                    .filter(|t| t.schema.eq_ignore_ascii_case(s))
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

/// What a caller can set about the `CAT` rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// A table with at least this many rows (by the snapshot's estimate) is
    /// large to CAT003 unbounded-scan.
    pub large_table_rows: u64,
}

/// [`Options::large_table_rows`] when nothing sets it.
pub const DEFAULT_LARGE_TABLE_ROWS: u64 = 1_000_000;

impl Default for Options {
    fn default() -> Self {
        Options {
            large_table_rows: DEFAULT_LARGE_TABLE_ROWS,
        }
    }
}

/// A `MOD` finding the catalog has something to say about: the rule `code`
/// would report at exactly `span`, and should not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Override {
    pub code: &'static str,
    pub span: Span,
    /// `None`: the catalog proves the `MOD` finding wrong, whatever else is
    /// reported. `Some(code)`: this `CAT` rule reports the same thing with
    /// the evidence, so the `MOD` finding goes only where that one stays.
    pub replaced_by: Option<&'static str>,
}

/// What [`check`] found.
#[derive(Clone, Debug, Default)]
pub struct Checked {
    pub findings: Vec<Finding>,
    pub overrides: Vec<Override>,
}

/// Run the `CAT` rules over every statement of `tree`.
///
/// `file` is the scan of the whole file the tree came from, not of the
/// tree's own text: on the per-statement fallback path the tree is one
/// statement and the file is still the file.
#[must_use]
pub fn check(
    tree: &Tree,
    src: &str,
    catalog: &Catalog,
    file: &FileContext,
    options: &Options,
) -> Checked {
    let mut out = Checked::default();
    if !file.checkable(catalog) {
        return out;
    }
    for &stmt in tree.children(tree.root()) {
        check_statement(tree, src, stmt, catalog, file, options, &mut out);
    }
    out.findings.sort_by_key(|f| (f.span.start, f.code));
    out
}

fn check_statement(
    tree: &Tree,
    src: &str,
    stmt: NodeId,
    catalog: &Catalog,
    file: &FileContext,
    options: &Options,
    checked: &mut Checked,
) {
    let out = &mut checked.findings;
    let nodes = tree.descendants(stmt);

    // First pass: what the statement defines for itself.
    let mut ctes: HashSet<String> = HashSet::new();
    // Names that may shadow a table alias in an expression: lambda
    // parameters and select-list aliases.
    let mut shadows: HashSet<String> = HashSet::new();
    // The lambda parameters alone: an unqualified name that is one of
    // these is never a column.
    let mut lambdas: HashSet<String> = HashSet::new();
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
                        lambdas.insert(ident(tree.text(c, src)));
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
                                lambdas.insert(ident(text(src, &t)));
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
    // Beside `bindings`: the node that brings each relation in.
    let mut binding_nodes: Vec<NodeId> = Vec::new();
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
                            let fix = edit_limit(&last)
                                .and_then(|edits| {
                                    suggest(
                                        &last,
                                        catalog
                                            .names_near(&last, edits)
                                            .chain(file.local.values().map(|l| l.name.as_str())),
                                    )
                                })
                                .map(|s| Fix {
                                    span,
                                    replacement: quote_ident(s),
                                });
                            out.push(Finding {
                                detail: None,
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
                binding_nodes.push(parent);
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
                    binding_nodes.push(n);
                } else if rule(tree, n) != "ParensTableRef" {
                    // Unnamed, and still a relation of its FROM: `FROM
                    // range(3)` is called after the function, a subquery
                    // without an alias has no name to be called by.
                    let name = first_descendant(tree, n, &["TableFunctionName"])
                        .filter(|_| rule(tree, n).starts_with("TableFunction"))
                        .map(|f| ident(tree.text(f, src)))
                        .unwrap_or_default();
                    bindings.push((name, Relation::Opaque));
                    binding_nodes.push(n);
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
            detail: None,
            code: "CAT002",
            span,
            fix,
        });
    }

    meta::check(
        &meta::Statement {
            tree,
            src,
            nodes: &nodes,
            tables: &tables,
            bindings: &bindings,
            binding_nodes: &binding_nodes,
            shadows: &shadows,
            lambdas: &lambdas,
            options,
        },
        checked,
    );
}

/// How many edits away a suggestion for `name` may be: one for a short
/// name, two for a longer one, none at all for a very short one.
fn edit_limit(name: &str) -> Option<usize> {
    match name.chars().count() {
        0..=2 => None,
        3..=5 => Some(1),
        _ => Some(2),
    }
}

/// The candidate closest to `name`, if exactly one is close (see
/// [`edit_limit`]).
fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let wanted: Vec<char> = name.chars().collect();
    let limit = edit_limit(name)?;
    let mut best: Option<(usize, &str)> = None;
    let mut tie = false;
    // Names already counted, lowercased: a table that is in two schemas is
    // one suggestion, not a tie with itself.
    let mut seen: HashSet<String> = HashSet::new();
    // A large catalog is thousands of candidates per finding, nearly all of
    // them nowhere near: those are turned away on length alone, and the rest
    // are compared in buffers that are reused.
    let mut lower: Vec<char> = Vec::new();
    let (mut prev, mut row) = (Vec::new(), Vec::new());
    for c in candidates {
        // A name has at most as many characters as bytes, and exactly as
        // many when it is ASCII.
        if c.len() + limit < wanted.len() || (c.is_ascii() && c.len() > wanted.len() + limit) {
            continue;
        }
        lower.clear();
        lower.extend(c.chars().map(|ch| ch.to_ascii_lowercase()));
        let d = distance(&wanted, &lower, limit, &mut prev, &mut row);
        // The name itself, somewhere else in the catalog, is no suggestion.
        if d == 0 || d > limit {
            continue;
        }
        if !seen.insert(lower.iter().collect()) {
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

/// Levenshtein distance over characters, giving up (returning `limit + 1`)
/// once it is certain to exceed `limit`. The two rows of the table are lent by
/// the caller so that a run over many candidates allocates them once.
fn distance(
    a: &[char],
    b: &[char],
    limit: usize,
    prev: &mut Vec<usize>,
    row: &mut Vec<usize>,
) -> usize {
    if a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }
    prev.clear();
    prev.extend(0..=b.len());
    for (i, ca) in a.iter().enumerate() {
        row.clear();
        row.push(i + 1);
        let mut least = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let v = (prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1);
            least = least.min(v);
            row.push(v);
        }
        if least > limit {
            return limit + 1;
        }
        std::mem::swap(prev, row);
    }
    prev[b.len()]
}

/// The message for a `CAT` finding: for CAT001 and CAT002 what was not
/// found and, when there is a near name, what was probably meant; for the
/// others the registry's prose and then what the catalog says that makes
/// it so ([`Finding::detail`]). `MOD` messages are fixed prose in the
/// registry.
#[must_use]
pub fn message(f: &Finding, src: &str) -> String {
    let name = src
        .get(f.span.start as usize..f.span.end as usize)
        .unwrap_or_default();
    let mut m = match f.code {
        "CAT001" => format!("Table or view {name} is not in the catalog."),
        "CAT002" => format!("Column {name} is not in the table it is qualified with."),
        _ => {
            let prose = crate::lookup(f.code).map_or("", |r| r.message);
            return match &f.detail {
                Some(d) => format!("{prose} By the catalog, {d}."),
                None => prose.to_string(),
            };
        }
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
        assert_eq!(cat.table_count(), 3);
        let orders = cat.tables().next().unwrap();
        assert_eq!((orders.name.as_str(), orders.columns.len()), ("orders", 2));
        assert!(!orders.is_view);
        assert!(cat.tables().nth(2).unwrap().is_view);

        let obj = format!(
            r#"{{"columns": {SHOP}, "functions": [{{"name":"my_macro","kind":"macro"}}]}}"#
        );
        let cat = Catalog::from_json_text(&obj).expect("the object form");
        assert_eq!(cat.table_count(), 3);
        assert_eq!(cat.functions().next().unwrap().name, "my_macro");

        // Exactly what `duckdb -json` prints for an empty database.
        assert_eq!(Catalog::from_json_text("[]").unwrap().table_count(), 0);
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

    fn json(text: &str) -> Json {
        crate::json::parse(text).expect("valid JSON")
    }

    /// `a.b.c` as the lowercase parts `resolve` takes.
    fn parts(name: &str) -> Vec<String> {
        name.split('.').map(str::to_string).collect()
    }

    /// A part from snapshot rows.
    fn part(database: &str, rows: &str) -> Part {
        Part::from_json(database, Some(&json(rows)), None, None).expect("a part")
    }

    #[test]
    fn a_part_replaces_only_its_own_database_and_an_equal_one_changes_nothing() {
        let mut cat = shop();
        let found = |cat: &Catalog, name: &str| {
            matches!(cat.resolve(&[name.to_string()]), Resolved::Found(_))
        };

        // The same part again: nothing changes, and the catalog says so.
        let lake =
            r#"[{"schema":"raw","table":"events","column":"ts","type":"TIMESTAMP","kind":"view"}]"#;
        assert!(!cat.set_part(part("lake", lake)));
        assert_eq!(cat, shop());

        // A different one takes its place; the other database is as it was.
        let shop_part = Arc::clone(cat.part("shop").unwrap());
        assert!(cat.set_part(part(
            "lake",
            r#"[{"schema":"raw","table":"clicks","column":"at","type":"TIMESTAMP"}]"#
        )));
        assert!(found(&cat, "clicks") && !found(&cat, "events"));
        assert!(found(&cat, "orders"));
        assert!(Arc::ptr_eq(&shop_part, cat.part("shop").unwrap()));
        assert_eq!(cat.parts().len(), 2);

        // A new database is added; rows may leave the database out, and
        // may not name another.
        assert!(cat.set_part(part(
            "pg",
            r#"[{"schema":"public","table":"users","column":"id","type":"INTEGER"}]"#
        )));
        assert_eq!(cat.resolve(&parts("pg.public.users")), {
            let t = cat.part("pg").unwrap().tables();
            Resolved::Found(vec![&t[0]])
        });
        assert_eq!(cat.part("pg").unwrap().tables()[0].database, "pg");
        assert!(
            Part::from_json(
                "pg",
                Some(&json(r#"[{"database":"other","table":"t"}]"#)),
                None,
                None
            )
            .is_err()
        );

        // Dropped: the names are gone and the qualifier is unknown again.
        assert!(cat.remove_part("PG"));
        assert!(!cat.remove_part("pg"));
        assert_eq!(cat.resolve(&parts("pg.public.users")), Resolved::Unchecked);
        assert_eq!(cat.table_count(), 3);
    }

    #[test]
    fn the_rules_see_a_catalog_built_from_parts() {
        let mut cat = Catalog::default();
        cat.set_part(part(
            "memory",
            r#"[{"schema":"main","table":"orders","column":"id","type":"BIGINT"}]"#,
        ));
        cat.set_part(
            Part::from_json(
                "pg",
                None,
                Some(&json(
                    r#"[{"schema":"public","table":"users","columns":["id","email"],"types":["INTEGER","VARCHAR"]}]"#,
                )),
                None,
            )
            .unwrap(),
        );
        assert_eq!(
            cat_findings(
                "SELECT u.emial FROM pg.public.users u JOIN ordrs o ON o.id = u.id;",
                &cat
            ),
            ["CAT002@emial", "CAT001@ordrs"]
        );
        assert!(cat_findings("SELECT * FROM pg.public.usrs;", &cat) == ["CAT001@usrs"]);
        // A system schema is never in a part, and is not checked.
        assert!(cat_findings("SELECT * FROM pg.pg_catalog.pg_class;", &cat).is_empty());
        assert!(cat_findings("SELECT * FROM pg.information_schema.tables;", &cat).is_empty());
        // Without the part, `pg` is a qualifier nobody knows.
        cat.remove_part("pg");
        assert!(cat_findings("SELECT * FROM pg.public.usrs;", &cat).is_empty());
    }

    const KEYED: &str = r#"{
        "columns": [
            {"database":"shop","schema":"main","table":"orders","column":"id","type":"BIGINT","nullable":false},
            {"database":"shop","schema":"main","table":"orders","column":"Code","type":"VARCHAR","nullable":false},
            {"database":"shop","schema":"main","table":"orders","column":"customer_id","type":"BIGINT","nullable":true},
            {"database":"shop","schema":"main","table":"orders","column":"placed","type":"DATE"},
            {"database":"shop","schema":"main","table":"recent","column":"id","type":"BIGINT"}
        ],
        "tables": [
            {"database":"shop","schema":"main","table":"orders","kind":"table","rows":120000,
             "primary_key":["id"],"unique":[["code"],["customer_id","placed"]],
             "indexes":[["customer_id","placed"],["(lower(code))"]]},
            {"database":"shop","schema":"main","table":"recent","kind":"view","rows":null,
             "primary_key":null,"unique":[],"indexes":null},
            {"database":"shop","schema":"main","table":"listed_only","rows":7}
        ]
    }"#;

    #[test]
    fn what_a_snapshot_says_of_a_table_can_be_looked_up() {
        let cat = Catalog::from_json_text(KEYED).expect("the object form with tables");
        let Resolved::Found(found) = cat.resolve(&parts("orders")) else {
            panic!("orders resolves")
        };
        let orders = found[0];
        assert_eq!(orders.row_estimate(), Some(120_000));
        assert!(!orders.is_view);
        // Columns in the table's own order, found in any case.
        let names: Vec<&str> = orders.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "Code", "customer_id", "placed"]);
        assert_eq!(orders.column_type("CODE"), Some("VARCHAR"));
        assert_eq!(orders.column_type("nope"), None);
        assert!(orders.has_column("Placed") && !orders.has_column("place"));
        assert_eq!(orders.is_nullable("id"), Some(false));
        assert_eq!(orders.is_nullable("customer_id"), Some(true));
        assert_eq!(
            orders.is_nullable("placed"),
            None,
            "the snapshot did not say"
        );
        assert_eq!(orders.is_nullable("nope"), None);
        // Keys.
        assert_eq!(orders.primary_key, ["id"]);
        let keys: Vec<&[String]> = orders.unique_keys().collect();
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0], ["id"]);
        assert!(orders.is_unique_on(&["ID"]));
        assert!(orders.is_unique_on(&["placed", "Customer_ID", "extra"]));
        assert!(!orders.is_unique_on(&["customer_id"]));
        // What an index leads with.
        let leading: Vec<&str> = orders.indexed_leading().collect();
        assert_eq!(
            leading,
            ["id", "code", "customer_id", "customer_id", "(lower(code))"]
        );
        assert!(orders.is_indexed_leading("CUSTOMER_ID"));
        assert!(!orders.is_indexed_leading("placed"));

        let Resolved::Found(found) = cat.resolve(&parts("recent")) else {
            panic!("recent resolves")
        };
        assert!(found[0].is_view);
        assert_eq!(found[0].row_estimate(), None);
        assert_eq!(found[0].unique_keys().count(), 0);
        assert!(!found[0].is_unique_on(&["id"]));
        // A table only "tables" lists exists, with unknown columns.
        let Resolved::Found(found) = cat.resolve(&parts("listed_only")) else {
            panic!("listed_only resolves")
        };
        assert!(found[0].columns.is_empty());
        assert_eq!(found[0].row_estimate(), Some(7));

        // The bare array says none of it, and still loads.
        let plain = shop();
        let t = plain.tables().next().unwrap();
        assert_eq!(
            (
                t.row_estimate(),
                t.is_nullable("id"),
                t.unique_keys().count()
            ),
            (None, None, 0)
        );
        // "tables" without "columns" is not a snapshot.
        assert!(Catalog::from_json_text(r#"{"tables": []}"#).is_err());
    }

    #[test]
    fn a_part_can_list_columns_table_by_table_and_means_the_same() {
        let by_rows = Part::from_json(
            "shop",
            Some(&json(
                r#"[{"schema":"main","table":"orders","column":"id","type":"BIGINT","nullable":false},
                    {"schema":"main","table":"orders","column":"total","type":"DECIMAL(10,2)","nullable":true},
                    {"schema":"main","table":"v","column":"x","type":"INT","kind":"view"}]"#,
            )),
            Some(&json(
                r#"[{"schema":"main","table":"orders","rows":5,"primary_key":["id"]}]"#,
            )),
            Some(&json(r#"[{"name":"twice","kind":"macro"}]"#)),
        )
        .unwrap();
        let by_tables = Part::from_json(
            "shop",
            None,
            Some(&json(
                r#"[{"schema":"main","table":"orders","rows":5,"primary_key":["id"],
                     "columns":["id","total"],"types":["BIGINT","DECIMAL(10,2)"],"nullable":[false,true]},
                    {"schema":"main","table":"v","kind":"view","columns":["x"],"types":["INT"]}]"#,
            )),
            Some(&json(r#"[{"name":"twice","kind":"macro"}]"#)),
        )
        .unwrap();
        assert_eq!(by_rows, by_tables);
        assert_eq!(by_tables.column_count(), 3);
        assert_eq!(by_tables.functions()[0].name, "twice");
        // An empty part is a part.
        let empty = Part::from_json("shop", None, Some(&json("[]")), None).unwrap();
        assert_eq!((empty.database(), empty.tables().len()), ("shop", 0));
        assert!(Part::from_json("shop", None, Some(&json(r#"[{"schema":"s"}]"#)), None).is_err());
        assert!(Part::from_json("shop", Some(&json("{}")), None, None).is_err());
    }

    #[test]
    fn tables_are_found_by_prefix_in_name_order_and_no_more_than_asked() {
        let mut rows = Vec::new();
        for i in 0..40 {
            for schema in ["a", "b"] {
                rows.push(format!(
                    r#"{{"schema":"{schema}","table":"T_{i:02}","column":"c","type":"INT"}}"#
                ));
            }
        }
        rows.push(r#"{"schema":"a","table":"other","column":"c","type":"INT"}"#.to_string());
        let mut cat = Catalog::default();
        cat.set_part(part("big", &format!("[{}]", rows.join(","))));
        cat.set_part(part(
            "memory",
            r#"[{"schema":"main","table":"t_local","column":"c","type":"INT"}]"#,
        ));
        let names = |prefix: &str, within: &[&str], limit: usize| {
            let within: Vec<String> = within.iter().map(|s| s.to_string()).collect();
            let (found, cut) = cat.tables_with_prefix(prefix, &within, limit);
            (
                found
                    .iter()
                    .map(|t| format!("{}.{}.{}", t.database, t.schema, t.name))
                    .collect::<Vec<_>>(),
                cut,
            )
        };
        // Case does not matter; a name in two schemas counts once.
        let (found, cut) = names("t_0", &[], 100);
        assert_eq!(found.len(), 20);
        assert_eq!(found[0], "big.a.T_00");
        assert_eq!(found[1], "big.b.T_00");
        assert!(!cut);
        // Cut to the first names, each database's own.
        let (found, cut) = names("t_", &[], 3);
        assert_eq!(
            found,
            [
                "big.a.T_00",
                "big.b.T_00",
                "big.a.T_01",
                "big.b.T_01",
                "big.a.T_02",
                "big.b.T_02",
                "memory.main.t_local"
            ]
        );
        assert!(cut);
        // Within a database, a schema, or a schema of a database.
        assert_eq!(names("o", &["big"], 10).0, ["big.a.other"]);
        assert_eq!(names("t_3", &["b"], 100).0.len(), 10);
        assert_eq!(names("", &["big", "a"], 100).0.len(), 41);
        assert_eq!(names("", &["memory", "a"], 100).0.len(), 0);
        assert_eq!(names("zz", &[], 10), (vec![], false));
        // Schemas and databases, each once.
        let mut schemas: Vec<(&str, &str)> = cat.schemas().collect();
        schemas.sort_unstable();
        assert_eq!(schemas, [("a", "big"), ("b", "big"), ("main", "memory")]);
        assert_eq!(cat.databases().collect::<Vec<_>>(), ["big", "memory"]);
    }

    #[test]
    fn names_near_never_leaves_out_a_name_that_close() {
        let names = [
            "orders",
            "order",
            "ordrs",
            "orderz",
            "borders",
            "sredro",
            "customers",
            "o",
            "ORDERS_2",
            "xrders",
            "oredrs",
            "events",
        ];
        let rows: Vec<String> = names
            .iter()
            .map(|n| format!(r#"{{"schema":"main","table":"{n}"}}"#))
            .collect();
        let cat = Catalog::from_json_text(&format!("[{}]", rows.join(","))).unwrap();
        let (mut prev, mut row) = (Vec::new(), Vec::new());
        for wanted in ["orders", "ordres", "event", "custmers"] {
            let w: Vec<char> = wanted.chars().collect();
            for edits in 1..=2 {
                let near: Vec<&str> = cat.names_near(wanted, edits).collect();
                for n in names {
                    let l: Vec<char> = n.to_ascii_lowercase().chars().collect();
                    if distance(&w, &l, edits, &mut prev, &mut row) <= edits {
                        assert!(near.contains(&n), "{n} is within {edits} of {wanted}");
                    }
                }
            }
        }
        // Most names are turned away without being measured.
        assert_eq!(cat.names_near("event", 1).collect::<Vec<_>>(), ["events"]);
        // And the suggestion made from them is the one made from all.
        assert_eq!(cat_findings("SELECT * FROM evnts;", &cat), ["CAT001@evnts"]);
        let sel = Selection {
            catalog: Some(&cat),
            ..Selection::default()
        };
        let f = analyze("SELECT * FROM evnts;", &sel);
        let fix = f
            .iter()
            .find(|f| f.code == "CAT001")
            .and_then(|f| f.fix.clone());
        assert_eq!(fix.map(|f| f.replacement), Some("events".to_string()));
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
