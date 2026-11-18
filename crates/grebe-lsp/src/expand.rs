//! "Expand * to columns": a `SELECT *` written out, from the catalog.
//!
//! A code action of kind `refactor.rewrite` on a `*` or an `alias.*` of a
//! select list. It replaces the star with the columns it stands for, in
//! the order the tables are written and each table's own order, leaving
//! out the ones an `EXCLUDE` names. The columns are written as
//! `relation.column` when the query reads more than one relation, or when
//! the star itself was qualified.
//!
//! It is a rewrite someone asks for, never a finding, and it is only
//! offered when the list it would write is the star's and nothing else:
//!
//! - the statement parses;
//! - every relation of the star's query is one table or view of the
//!   catalog with its columns known -- not a CTE, a subquery, a table
//!   function, a file, a table the file creates, or a name the catalog has
//!   twice;
//! - the query's FROM is read whole (see [`Blocks::relations`]), and joins
//!   nothing in a way that changes what `*` means: no `USING`, `NATURAL`,
//!   `SEMI`, `ANTI`, `PIVOT` or `UNPIVOT`;
//! - the star is followed by nothing but an optional `EXCLUDE` of plain
//!   names the tables have: `REPLACE`, `RENAME` and patterns are left
//!   alone.

use grebe_rules::catalog::{Catalog, FileContext, quote_ident};
use grebe_syntax::Span;
use grebe_syntax::token::{Token, TokenKind};

use crate::completion::{
    Blocks, is_punct, name_list, name_of, scope_of, statement_at, table_of, text, upper,
};
use crate::json::Json;
use crate::position::{self, Encoding};

/// The code action kind.
pub const ACTION_KIND: &str = "refactor.rewrite";

/// Does a `context.only` list ask for this action? An absent list asks for
/// everything; a kind asks for the kinds below it.
#[must_use]
pub fn asked_for(only: Option<&Json>) -> bool {
    let Some(kinds) = only.and_then(Json::as_array) else {
        return true;
    };
    kinds.iter().filter_map(Json::as_str).any(|k| {
        ACTION_KIND == k
            || ACTION_KIND
                .strip_prefix(k)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// A star's replacement: the bytes to replace, and the column list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Expansion {
    pub span: Span,
    pub columns: String,
}

/// Half-open overlap; a bare cursor counts when it touches the span.
fn touches(span: Span, range: Span) -> bool {
    if range.start == range.end {
        return span.start <= range.start && range.start <= span.end;
    }
    span.start < range.end && range.start < span.end
}

/// What may follow a star that is a whole select item.
const AFTER_STAR: &[&str] = &[
    "FROM",
    "WHERE",
    "GROUP",
    "HAVING",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "QUALIFY",
    "WINDOW",
    "UNION",
    "INTERSECT",
    "EXCEPT",
];

/// Is the item that starts at token `start` in a select list? Walks back
/// over the items before it to the `SELECT`.
fn in_select_list(src: &str, t: &[Token], start: usize) -> bool {
    let mut depth = 0usize;
    for tok in t[..start].iter().rev() {
        match tok.kind {
            TokenKind::Punct => match text(src, tok) {
                ")" => depth += 1,
                "(" => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                }
                _ => {}
            },
            TokenKind::Word if depth == 0 => match text(src, tok).to_ascii_uppercase().as_str() {
                "SELECT" => return true,
                "FROM" | "WHERE" | "GROUP" | "HAVING" | "ORDER" | "BY" | "LIMIT" | "QUALIFY"
                | "WINDOW" | "ON" | "USING" | "SET" | "VALUES" | "RETURNING" => return false,
                _ => {}
            },
            _ => {}
        }
    }
    false
}

/// The expansion of the star at token `s` of the statement `t`.
fn expand_at(src: &str, t: &[Token], s: usize, catalog: &Catalog) -> Option<Expansion> {
    // `o.*`: one name before the dot, not `schema.table.*`.
    let of = if s >= 2 && is_punct(src, t.get(s - 1), ".") {
        if s >= 3 && is_punct(src, t.get(s - 3), ".") {
            return None;
        }
        Some(name_of(src, &t[s - 2])?)
    } else {
        None
    };
    let start = if of.is_some() { s - 2 } else { s };
    let before = start.checked_sub(1).and_then(|i| t.get(i))?;
    let leads = is_punct(src, Some(before), ",")
        || matches!(
            upper(src, Some(before)).as_str(),
            "SELECT" | "DISTINCT" | "ALL"
        );
    if !leads || !in_select_list(src, t, start) {
        return None;
    }

    // `EXCLUDE (a, b)` or `EXCLUDE a`.
    let mut end = s + 1;
    let mut excluded: Vec<String> = Vec::new();
    if upper(src, t.get(end)) == "EXCLUDE" {
        if is_punct(src, t.get(end + 1), "(") {
            let (names, next) = name_list(src, t, end + 1)?;
            excluded = names;
            end = next;
        } else {
            excluded.push(name_of(src, t.get(end + 1)?)?);
            end += 2;
        }
    }
    let ends_item = match t.get(end) {
        None => true,
        Some(tok) if tok.kind == TokenKind::Punct => matches!(text(src, tok), "," | ")"),
        Some(tok) => AFTER_STAR.contains(&upper(src, Some(tok)).as_str()),
    };
    if !ends_item {
        return None;
    }

    let blocks = Blocks::new(src, t);
    let scope = scope_of(src, t, &blocks);
    let file = FileContext::scan(src);
    let block = *blocks.blocks_at(src, t, s).first()?;
    let joins_by_name = (block.start..block.end)
        .filter(|i| blocks.direct(*i, &block))
        .any(|i| {
            matches!(
                upper(src, t.get(i)).as_str(),
                "USING" | "NATURAL" | "SEMI" | "ANTI" | "PIVOT" | "UNPIVOT"
            )
        });
    if joins_by_name {
        return None;
    }
    let relations = blocks.relations(src, t, &block, &scope)?;
    if relations.is_empty() {
        return None;
    }
    let mut tables = Vec::with_capacity(relations.len());
    for b in &relations {
        let table = table_of(b, &scope, &file, catalog)?;
        if table.columns.is_empty() || b.name.is_empty() {
            return None;
        }
        tables.push((b.name.as_str(), table));
    }
    let qualify = of.is_some() || tables.len() > 1;
    if let Some(q) = &of {
        tables.retain(|(name, _)| name.eq_ignore_ascii_case(q));
        if tables.len() != 1 {
            return None;
        }
    }
    let is_excluded = |c: &str| excluded.iter().any(|e| e.eq_ignore_ascii_case(c));
    if !excluded
        .iter()
        .all(|e| tables.iter().any(|(_, table)| table.has_column(e)))
    {
        return None;
    }
    let columns: Vec<String> = tables
        .iter()
        .flat_map(|(name, table)| {
            table
                .columns
                .iter()
                .filter(|c| !is_excluded(&c.name))
                .map(move |c| {
                    if qualify {
                        format!("{}.{}", quote_ident(name), quote_ident(&c.name))
                    } else {
                        quote_ident(&c.name)
                    }
                })
        })
        .collect();
    if columns.is_empty() {
        return None;
    }
    Some(Expansion {
        span: Span::new(t[start].span.start, t[end - 1].span.end),
        columns: columns.join(", "),
    })
}

/// The expansion of the first star `range` touches that can be expanded.
#[must_use]
pub fn expansion(src: &str, range: Span, catalog: &Catalog) -> Option<Expansion> {
    let code: Vec<Token> = grebe_syntax::token::tokenize(src)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    for (i, tok) in code.iter().enumerate() {
        if tok.kind != TokenKind::Operator || text(src, tok) != "*" {
            continue;
        }
        // The alias of `o.*` is part of what can be pointed at.
        let from = if i >= 2 && is_punct(src, code.get(i - 1), ".") {
            code[i - 2].span.start
        } else {
            tok.span.start
        };
        if !touches(Span::new(from, tok.span.end), range) {
            continue;
        }
        let (statement, s) = statement_at(src, &code, i);
        // Read off tokens, a statement that does not parse could look like
        // one with more tables than it has.
        let (Some(first), Some(last)) = (statement.first(), statement.last()) else {
            continue;
        };
        let whole = &src[first.span.start as usize..last.span.end as usize];
        if !grebe_syntax::matcher::parse_check(whole).0 {
            continue;
        }
        if let Some(found) = expand_at(src, &statement, s, catalog) {
            return Some(found);
        }
    }
    None
}

/// The code action for `range` of `src`, if a star there can be expanded.
#[must_use]
pub fn action(uri: &str, src: &str, range: Span, catalog: &Catalog, enc: Encoding) -> Option<Json> {
    let found = expansion(src, range, catalog)?;
    let pos = |byte: u32| {
        let (line, character) = position::byte_to_position(src, byte, enc);
        Json::object(vec![
            ("line".into(), Json::num(f64::from(line))),
            ("character".into(), Json::num(f64::from(character))),
        ])
    };
    let edit = Json::object(vec![
        (
            "range".into(),
            Json::object(vec![
                ("start".into(), pos(found.span.start)),
                ("end".into(), pos(found.span.end)),
            ]),
        ),
        ("newText".into(), Json::str(&found.columns)),
    ]);
    Some(Json::object(vec![
        ("title".into(), Json::str("Expand * to columns")),
        ("kind".into(), Json::str(ACTION_KIND)),
        (
            "edit".into(),
            Json::object(vec![(
                "changes".into(),
                Json::object(vec![(uri.to_string(), Json::Array(vec![edit]))]),
            )]),
        ),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shop() -> Catalog {
        Catalog::from_json_text(crate::hover::tests::SHOP).unwrap()
    }

    /// `sql` with the star at the `|` expanded, or `None`.
    fn expanded(sql: &str) -> Option<String> {
        let offset = sql.find('|').expect("a | marks the cursor") as u32;
        let src = sql.replacen('|', "", 1);
        let found = expansion(&src, Span::new(offset, offset), &shop())?;
        let mut out = src.clone();
        out.replace_range(
            found.span.start as usize..found.span.end as usize,
            &found.columns,
        );
        Some(out)
    }

    #[test]
    fn a_star_becomes_the_tables_columns_in_their_order() {
        assert_eq!(
            expanded("SELECT |* FROM customers WHERE id = 1").unwrap(),
            "SELECT id, email, name, country FROM customers WHERE id = 1"
        );
        // A name that needs quotes gets them; FROM-first works too.
        assert_eq!(
            expanded("FROM orders SELECT *|").unwrap(),
            "FROM orders SELECT id, customer_id, placed, total, \"Order Note\""
        );
        assert_eq!(
            expanded("SELECT DISTINCT *| FROM main.stock;").unwrap(),
            "SELECT DISTINCT order_id, line_no, warehouse FROM main.stock;"
        );
    }

    #[test]
    fn several_tables_are_qualified_in_the_order_written() {
        assert_eq!(
            expanded("SELECT *| FROM stock s JOIN customers ON true").unwrap(),
            "SELECT s.order_id, s.line_no, s.warehouse, customers.id, customers.email, \
             customers.name, customers.country FROM stock s JOIN customers ON true"
        );
        assert_eq!(
            expanded("SELECT s.|*, 1 FROM stock s, customers c").unwrap(),
            "SELECT s.order_id, s.line_no, s.warehouse, 1 FROM stock s, customers c"
        );
        // A qualified star stays qualified with one table.
        assert_eq!(
            expanded("SELECT s|.* FROM stock s").unwrap(),
            "SELECT s.order_id, s.line_no, s.warehouse FROM stock s"
        );
    }

    #[test]
    fn exclude_is_respected_and_goes() {
        assert_eq!(
            expanded("SELECT *| EXCLUDE (email, NAME) FROM customers").unwrap(),
            "SELECT id, country FROM customers"
        );
        assert_eq!(
            expanded("SELECT * EXCLUDE| warehouse, 1 FROM stock"),
            None,
            "the cursor is not on the star"
        );
        assert_eq!(
            expanded("SELECT |* EXCLUDE warehouse, 1 FROM stock").unwrap(),
            "SELECT order_id, line_no, 1 FROM stock"
        );
        assert_eq!(
            expanded("SELECT c.*| EXCLUDE (id) FROM customers c JOIN stock s ON true").unwrap(),
            "SELECT c.email, c.name, c.country FROM customers c JOIN stock s ON true"
        );
        // A column no table has: DuckDB refuses it, and so does this.
        assert_eq!(expanded("SELECT *| EXCLUDE (nope) FROM customers"), None);
    }

    #[test]
    fn each_query_expands_its_own_tables() {
        assert_eq!(
            expanded("SELECT *| FROM stock WHERE order_id IN (SELECT id FROM customers)").unwrap(),
            "SELECT order_id, line_no, warehouse FROM stock \
             WHERE order_id IN (SELECT id FROM customers)"
        );
        assert_eq!(
            expanded("SELECT 1 FROM stock WHERE EXISTS (SELECT *| FROM customers)").unwrap(),
            "SELECT 1 FROM stock WHERE EXISTS (SELECT id, email, name, country FROM customers)"
        );
        assert_eq!(
            expanded("SELECT * FROM stock UNION ALL SELECT *| FROM stock s2").unwrap(),
            "SELECT * FROM stock UNION ALL SELECT order_id, line_no, warehouse FROM stock s2"
        );
        // The table an INSERT writes is not one the star reads.
        assert_eq!(
            expanded("INSERT INTO customers SELECT *| FROM stock").unwrap(),
            "INSERT INTO customers SELECT order_id, line_no, warehouse FROM stock"
        );
    }

    #[test]
    fn not_offered_when_a_relation_is_not_a_known_table() {
        for sql in [
            "SELECT *| FROM nope",
            "SELECT *| FROM stock JOIN nope ON true",
            "SELECT s.*| FROM stock s JOIN nope ON true",
            "SELECT *| FROM stock, (SELECT 1) x",
            "SELECT *| FROM (SELECT 1)",
            "SELECT *| FROM stock JOIN LATERAL (SELECT 1) l ON true",
            "SELECT *| FROM read_csv('stock.csv')",
            "SELECT *| FROM 'stock.csv'",
            "SELECT *| FROM stock AS s(a, b, c)",
            "WITH stock AS (SELECT 1) SELECT *| FROM stock",
            "CREATE TABLE stock (a INT);\nSELECT *| FROM stock",
            "SELECT x.*| FROM stock s",
            "SELECT *|",
            // Two statements run together, which do not parse as one.
            "SELECT 1 FROM orders o JOIN customers c ON\nFROM stock SELECT *|",
        ] {
            assert_eq!(expanded(sql), None, "{sql}");
        }
    }

    #[test]
    fn not_offered_when_the_star_means_something_else() {
        for sql in [
            // Joins that merge or drop columns.
            "SELECT *| FROM stock JOIN order_items USING (order_id)",
            "SELECT *| FROM stock NATURAL JOIN order_items",
            "SELECT *| FROM stock SEMI JOIN order_items ON true",
            // More after the star than an EXCLUDE.
            "SELECT *| REPLACE (1 AS line_no) FROM stock",
            "SELECT *| EXCLUDE (line_no) REPLACE (1 AS order_id) FROM stock",
            "SELECT *| LIKE 'o%' FROM stock",
            // Not a select item.
            "SELECT count(*|) FROM stock",
            "SELECT line_no *| 2 FROM stock",
            "SELECT COLUMNS(*|) FROM stock",
            "SELECT main.stock.*| FROM main.stock",
            // A FROM that is not read whole.
            "SELECT *| FROM stock s JOIN customers c ON s.order_id = c.id, orders",
        ] {
            assert_eq!(expanded(sql), None, "{sql}");
        }
    }

    #[test]
    fn a_name_in_two_databases_is_not_guessed() {
        let two = Catalog::from_json_text(
            r#"[
            {"database":"a","schema":"main","table":"t","column":"x","type":"INT"},
            {"database":"b","schema":"main","table":"t","column":"y","type":"INT"}
            ]"#,
        )
        .unwrap();
        assert_eq!(expansion("SELECT * FROM t", Span::new(7, 7), &two), None);
        assert_eq!(
            expansion("SELECT * FROM b.t", Span::new(7, 7), &two).map(|e| e.columns),
            Some("y".to_string())
        );
    }

    #[test]
    fn the_action_is_a_rewrite_asked_for_by_kind() {
        let only = |kinds: &[&str]| Json::Array(kinds.iter().map(|k| Json::str(*k)).collect());
        assert!(asked_for(None));
        assert!(asked_for(Some(&only(&["refactor"]))));
        assert!(asked_for(Some(&only(&["quickfix", "refactor.rewrite"]))));
        assert!(!asked_for(Some(&only(&["quickfix"]))));
        assert!(!asked_for(Some(&only(&["refactor.rewrite.more"]))));
        assert!(!asked_for(Some(&only(&["source"]))));

        let src = "SELECT\n  * FROM stock";
        let a = action(
            "file:///q.sql",
            src,
            Span::new(9, 9),
            &shop(),
            Encoding::Utf16,
        )
        .unwrap();
        assert_eq!(
            a.get("title").and_then(Json::as_str),
            Some("Expand * to columns")
        );
        assert_eq!(
            a.get("kind").and_then(Json::as_str),
            Some("refactor.rewrite")
        );
        let edit = &a
            .get("edit")
            .and_then(|e| e.get("changes"))
            .and_then(|c| c.get("file:///q.sql"))
            .and_then(Json::as_array)
            .unwrap()[0];
        assert_eq!(
            edit.get("newText").and_then(Json::as_str),
            Some("order_id, line_no, warehouse")
        );
        let start = edit.get("range").and_then(|r| r.get("start")).unwrap();
        assert_eq!(start.get("line").and_then(Json::as_f64), Some(1.0));
        assert_eq!(start.get("character").and_then(Json::as_f64), Some(2.0));
    }
}
