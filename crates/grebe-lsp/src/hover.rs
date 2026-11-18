//! `textDocument/hover`: what the catalog knows of the name under the
//! cursor.
//!
//! - On a table or a view in a FROM (or after `JOIN`, `INTO`, `UPDATE`,
//!   `DESCRIBE`, ...): its full name, what kind it is, about how many rows
//!   it holds, its keys and indexes, and its columns with their types.
//! - On an alias, where it is given or where it is used: the same, for the
//!   table it stands for.
//! - On a column: its type, whether it can be NULL, and its place in the
//!   table's keys and indexes.
//!
//! The statement is read off its tokens, as completion reads it (see
//! [`crate::completion`]), so a statement that does not parse still
//! answers. What is said has to be true, so a name is only described when
//! it can be one thing:
//!
//! - a table name has to be exactly one table of the catalog, and not a
//!   CTE or a table the file itself creates, alters or drops;
//! - `alias.column` has to name a relation of the query around it (or of a
//!   query around that one) which is such a table, and the table has to
//!   have the column;
//! - a column with nothing before it has to be a column of exactly one of
//!   the query's relations, and every one of them has to be a table whose
//!   columns are known: next to a subquery, a table function or a file, the
//!   column could be theirs. When no relation of the query has it, the
//!   query around it is tried the same way. A name the select list also
//!   gives as an alias, or a lambda's parameter, is left alone.
//!
//! Anything else -- no catalog, a keyword, a function, a string, a name
//! that is not there or is there twice -- has no hover.

use grebe_rules::catalog::{Catalog, Column, FileContext, Table, quote_ident};
use grebe_syntax::Span;
use grebe_syntax::token::{Token, TokenKind};

use crate::completion::{
    Binding, Blocks, Scope, Target, is_punct, name_of, named_table, rows_text, scope_of,
    statement_at, table_of, text, upper,
};

/// The most columns a table's hover lists.
const COLUMN_LIMIT: usize = 40;
/// The widest a column's name is padded to.
const NAME_WIDTH: usize = 32;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hover {
    /// Markdown.
    pub contents: String,
    /// The name it is about.
    pub span: Span,
}

/// `text` as a Markdown code span.
fn code(text: &str) -> String {
    format!("`{}`", text.replace('`', "'"))
}

fn qualified(t: &Table) -> String {
    [&t.database, &t.schema, &t.name]
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| quote_ident(p))
        .collect::<Vec<_>>()
        .join(".")
}

/// A key's columns: `` `id` ``, or ``(`a`, `b`)``.
fn key_text(key: &[String]) -> String {
    match key {
        [one] => code(one),
        _ => format!(
            "({})",
            key.iter().map(|k| code(k)).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn what(t: &Table) -> &'static str {
    if t.is_view { "view" } else { "table" }
}

/// The hover of a table, or of `alias` that stands for it.
fn table_markdown(t: &Table, alias: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(a) = alias {
        out.push_str(&code(&quote_ident(a)));
        out.push_str(" is ");
    }
    out.push_str(&code(&qualified(t)));
    out.push_str(" — ");
    out.push_str(what(t));
    if let Some(rows) = rows_text(t) {
        out.push_str(", ");
        out.push_str(&rows);
    }
    let mut keys: Vec<String> = Vec::new();
    if !t.primary_key.is_empty() {
        keys.push(format!("Primary key: {}", key_text(&t.primary_key)));
    }
    let list = |keys: &[Vec<String>]| {
        keys.iter()
            .map(|k| key_text(k))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !t.unique.is_empty() {
        keys.push(format!("Unique: {}", list(&t.unique)));
    }
    if !t.indexes.is_empty() {
        keys.push(format!("Indexes: {}", list(&t.indexes)));
    }
    if !keys.is_empty() {
        out.push_str("\n\n");
        // Two spaces end a line without ending the paragraph.
        out.push_str(&keys.join("  \n"));
    }
    if t.columns.is_empty() {
        return out;
    }
    let shown = &t.columns[..t.columns.len().min(COLUMN_LIMIT)];
    let width = shown
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(NAME_WIDTH);
    out.push_str("\n\n```text\n");
    for c in shown {
        let mut line = format!("{:width$}  {}", c.name, c.data_type);
        if c.nullable == Some(false) {
            line.push_str(" NOT NULL");
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push_str("```");
    let more = t.columns.len() - shown.len();
    if more > 0 {
        out.push_str(&format!(
            "\n\n… {more} more column{} ({} in all)",
            if more == 1 { "" } else { "s" },
            t.columns.len()
        ));
    }
    out
}

/// The hover of a column of `t`.
fn column_markdown(t: &Table, c: &Column) -> String {
    let mut out = code(&format!(
        "{}.{}",
        quote_ident(&t.name),
        quote_ident(&c.name)
    ));
    let mut facts: Vec<String> = Vec::new();
    if !c.data_type.is_empty() {
        facts.push(code(&c.data_type));
    }
    match c.nullable {
        Some(false) => facts.push("NOT NULL".into()),
        Some(true) => facts.push("nullable".into()),
        None => {}
    }
    if !facts.is_empty() {
        out.push_str(" — ");
        out.push_str(&facts.join(", "));
    }
    out.push_str(&format!(
        "\n\nColumn of {} {}.",
        what(t),
        code(&qualified(t))
    ));
    let holds = |key: &[String]| key.iter().any(|k| k.eq_ignore_ascii_case(&c.name));
    if holds(&t.primary_key) {
        out.push_str(&match t.primary_key.len() {
            1 => " Primary key.".to_string(),
            _ => format!(" Part of the primary key {}.", key_text(&t.primary_key)),
        });
    }
    for key in t.unique.iter().filter(|k| holds(k)) {
        out.push_str(&match key.len() {
            1 => " Unique.".to_string(),
            _ => format!(" Part of the unique key {}.", key_text(key)),
        });
    }
    for key in t.indexes.iter().filter(|k| holds(k)).take(3) {
        out.push_str(&match key.len() {
            1 => " Indexed.".to_string(),
            _ => format!(" In the index {}.", key_text(key)),
        });
    }
    out
}

/// A statement, read for names.
struct Reading<'a> {
    src: &'a str,
    t: &'a [Token],
    blocks: &'a Blocks,
    scope: &'a Scope,
    file: &'a FileContext,
    catalog: &'a Catalog,
}

impl<'a> Reading<'a> {
    /// The relation `qualifier` names where token `at` is: looked for in
    /// the query around it, then in the ones around that. `None` when a
    /// query has two.
    fn relation(&self, qualifier: &[String], at: usize) -> Option<&'a Binding> {
        let last = qualifier.last()?;
        for block in self.blocks.blocks_at(self.src, self.t, at) {
            let found: Vec<&Binding> = self
                .scope
                .bindings
                .iter()
                .filter(|b| self.blocks.member(b.at, &block))
                .filter(|b| b.name.eq_ignore_ascii_case(last))
                .filter(|b| match (&b.target, qualifier.len()) {
                    (_, 1) => true,
                    // `main.orders.id`: the table as the FROM wrote it,
                    // and not under another name.
                    (Target::Named(parts), n) => {
                        parts.len() >= n
                            && parts[parts.len() - n..]
                                .iter()
                                .zip(qualifier)
                                .all(|(p, q)| p.eq_ignore_ascii_case(q))
                    }
                    _ => false,
                })
                .collect();
            match found.as_slice() {
                [] => {}
                [one] => return Some(one),
                _ => return None,
            }
        }
        None
    }

    /// The one table in reach of token `at` that has a column `name`.
    fn column(&self, name: &str, at: usize) -> Option<(&'a Table, &'a Column)> {
        for block in self.blocks.blocks_at(self.src, self.t, at) {
            let relations = self
                .blocks
                .relations(self.src, self.t, &block, self.scope)?;
            let written = self
                .scope
                .bindings
                .iter()
                .filter(|b| b.written && self.blocks.member(b.at, &block));
            let mut found: Option<(&Table, &Column)> = None;
            for b in relations.into_iter().chain(written) {
                let table = table_of(b, self.scope, self.file, self.catalog)?;
                if table.columns.is_empty() {
                    return None;
                }
                if let Some(c) = table.column(name) {
                    if found.is_some() {
                        return None;
                    }
                    found = Some((table, c));
                }
            }
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// Is `name` something other than a column where it stands alone: an
    /// alias the statement gives an expression, or a lambda's parameter?
    fn shadowed(&self, name: &str) -> bool {
        let is = |i: usize| {
            self.t
                .get(i)
                .and_then(|tok| name_of(self.src, tok))
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        };
        (0..self.t.len()).any(|i| {
            is(i)
                && (self.defines(i) || self.t.get(i + 1).is_some_and(|n| text(self.src, n) == "->"))
        })
    }

    /// Does the name at token `i` name what is before it, rather than
    /// refer to something? `x AS name`, `x name`, `1 name`.
    fn defines(&self, i: usize) -> bool {
        let Some(prev) = i.checked_sub(1).and_then(|p| self.t.get(p)) else {
            return false;
        };
        match prev.kind {
            TokenKind::Number | TokenKind::String | TokenKind::QuotedIdent => true,
            TokenKind::Punct => matches!(text(self.src, prev), ")" | "]"),
            TokenKind::Word => {
                let word = text(self.src, prev);
                let class = grebe_syntax::grammar::classify_keyword(word);
                if word.eq_ignore_ascii_case("AS") || word.eq_ignore_ascii_case("END") {
                    return true;
                }
                if class.is_none() {
                    // A plain name: `total t`.
                    return true;
                }
                if class.has(grebe_syntax::keyword::Keyword::RESERVED) {
                    return false;
                }
                // A word that is a keyword and can be a name (`name`,
                // `date`): a name here when it starts a select item or
                // follows a dot, a keyword after `ORDER` or `UPDATE t`.
                let before = i.checked_sub(2).and_then(|p| self.t.get(p));
                is_punct(self.src, before, ".")
                    || is_punct(self.src, before, ",")
                    || matches!(
                        upper(self.src, before).as_str(),
                        "SELECT" | "DISTINCT" | "ALL"
                    )
            }
            _ => false,
        }
    }
}

/// The hover at byte `offset` of `src`, by `catalog`.
#[must_use]
pub fn hover(src: &str, offset: u32, catalog: &Catalog) -> Option<Hover> {
    let code_tokens: Vec<Token> = grebe_syntax::token::tokenize(src)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let is_name = |t: &Token| matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent);
    // The name the cursor is in, or else the one it is right after.
    let under = code_tokens
        .iter()
        .position(|t| is_name(t) && t.span.start <= offset && offset < t.span.end)
        .or_else(|| {
            code_tokens
                .iter()
                .position(|t| is_name(t) && t.span.end == offset)
        })?;
    let (statement, k) = statement_at(src, &code_tokens, under);
    let t = statement.as_slice();
    let token = t[k];
    let reserved = |tok: &Token| {
        tok.kind == TokenKind::Word
            && grebe_syntax::grammar::classify_keyword(text(src, tok))
                .has(grebe_syntax::keyword::Keyword::RESERVED)
    };
    if reserved(&token) {
        return None;
    }

    let blocks = Blocks::new(src, t);
    let scope = scope_of(src, t, &blocks);
    let file = FileContext::scan(src);
    let reading = Reading {
        src,
        t,
        blocks: &blocks,
        scope: &scope,
        file: &file,
        catalog,
    };
    let answer = |contents: String| {
        Some(Hover {
            contents,
            span: token.span,
        })
    };

    // The dotted name the token is part of, and which part it is.
    let mut lo = k;
    while lo >= 2 && is_punct(src, t.get(lo - 1), ".") && name_of(src, &t[lo - 2]).is_some() {
        lo -= 2;
    }
    let mut hi = k;
    while is_punct(src, t.get(hi + 1), ".") && t.get(hi + 2).is_some_and(is_name) {
        hi += 2;
    }
    let chain: Vec<String> = (lo..=hi)
        .step_by(2)
        .filter_map(|i| name_of(src, &t[i]))
        .collect();
    let part = (k - lo) / 2;
    let last = chain.len() - 1;

    // A table where a FROM names it, or its alias there.
    for b in &scope.bindings {
        if b.alias == Some(k) {
            let table = table_of(b, &scope, &file, catalog)?;
            return answer(table_markdown(table, Some(&b.name)));
        }
        if b.name_tokens.0 <= k && k < b.name_tokens.1 {
            if k + 1 != b.name_tokens.1 {
                return None;
            }
            let table = table_of(b, &scope, &file, catalog)?;
            return answer(table_markdown(table, None));
        }
    }
    // A function, or a name before a string (`DATE '2026-01-01'`).
    if part == last
        && (is_punct(src, t.get(hi + 1), "(")
            || t.get(hi + 1).is_some_and(|n| n.kind == TokenKind::String))
    {
        return None;
    }
    // A table a statement names without a FROM.
    let before = upper(src, lo.checked_sub(1).and_then(|i| t.get(i)));
    if part == last
        && matches!(
            before.as_str(),
            "TABLE" | "DESCRIBE" | "SUMMARIZE" | "TRUNCATE"
        )
    {
        let table = named_table(&chain, &scope, &file, catalog)?;
        return answer(table_markdown(table, None));
    }

    if part > 0 {
        // `relation.column`; or the table in `schema.table.column`.
        if let Some(b) = reading.relation(&chain[..part], k) {
            let table = table_of(b, &scope, &file, catalog)?;
            let column = table.column(&chain[part])?;
            return answer(column_markdown(table, column));
        }
        if part < last {
            let b = reading.relation(&chain[..=part], k)?;
            let table = table_of(b, &scope, &file, catalog)?;
            return answer(table_markdown(table, None));
        }
        return None;
    }

    // A name with nothing before it: a relation of the statement, or a
    // column of one. Not a name being given (`AS total`), a type
    // (`x::date`), a named argument, or the field in `extract(year FROM d)`.
    let name = &chain[0];
    let next = t.get(hi + 1);
    let prev = k.checked_sub(1).and_then(|i| t.get(i));
    if reading.defines(k)
        || prev.is_some_and(|p| text(src, p) == "::")
        || next.is_some_and(|n| matches!(text(src, n), ":=" | "=>" | "->"))
    {
        return None;
    }
    if upper(src, next) == "FROM"
        && is_punct(src, prev, "(")
        && k.checked_sub(2)
            .and_then(|i| t.get(i))
            .is_some_and(|w| w.kind == TokenKind::Word)
    {
        return None;
    }
    let relation = reading.relation(std::slice::from_ref(name), k);
    let shadowed = reading.shadowed(name);
    let column = if shadowed {
        None
    } else {
        reading.column(name, k)
    };
    match (relation, column) {
        (Some(b), None) if last > 0 || !shadowed => {
            let table = table_of(b, &scope, &file, catalog)?;
            let alias = (!table.name.eq_ignore_ascii_case(&b.name)).then_some(b.name.as_str());
            answer(table_markdown(table, alias))
        }
        (None, Some((table, c))) => answer(column_markdown(table, c)),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What `grebe catalog --with-metadata` writes for a small database.
    pub(crate) const SHOP: &str = r#"{"columns": [
{"database":"shop","schema":"main","table":"big_orders","column":"id","type":"BIGINT","nullable":true},
{"database":"shop","schema":"main","table":"big_orders","column":"customer_id","type":"BIGINT","nullable":true},
{"database":"shop","schema":"main","table":"big_orders","column":"total","type":"DECIMAL(10,2)","nullable":true},
{"database":"shop","schema":"main","table":"customers","column":"id","type":"BIGINT","nullable":false},
{"database":"shop","schema":"main","table":"customers","column":"email","type":"VARCHAR","nullable":false},
{"database":"shop","schema":"main","table":"customers","column":"name","type":"VARCHAR","nullable":true},
{"database":"shop","schema":"main","table":"customers","column":"country","type":"VARCHAR","nullable":true},
{"database":"shop","schema":"main","table":"orders","column":"id","type":"BIGINT","nullable":false},
{"database":"shop","schema":"main","table":"orders","column":"customer_id","type":"BIGINT","nullable":false},
{"database":"shop","schema":"main","table":"orders","column":"placed","type":"TIMESTAMP","nullable":false},
{"database":"shop","schema":"main","table":"orders","column":"total","type":"DECIMAL(10,2)","nullable":true},
{"database":"shop","schema":"main","table":"orders","column":"Order Note","type":"VARCHAR","nullable":true},
{"database":"shop","schema":"main","table":"order_items","column":"order_id","type":"BIGINT","nullable":false},
{"database":"shop","schema":"main","table":"order_items","column":"line_no","type":"INTEGER","nullable":false},
{"database":"shop","schema":"main","table":"order_items","column":"sku","type":"VARCHAR","nullable":false},
{"database":"shop","schema":"main","table":"order_items","column":"qty","type":"INTEGER","nullable":true},
{"database":"shop","schema":"main","table":"stock","column":"order_id","type":"BIGINT","nullable":true},
{"database":"shop","schema":"main","table":"stock","column":"line_no","type":"INTEGER","nullable":true},
{"database":"shop","schema":"main","table":"stock","column":"warehouse","type":"VARCHAR","nullable":true}],
"tables": [
{"database":"shop","schema":"main","table":"orders","kind":"table","rows":1240000,"primary_key":["id"],"unique":[],"indexes":[["customer_id","placed"]]},
{"database":"shop","schema":"main","table":"customers","kind":"table","rows":2500,"primary_key":["id"],"unique":[["email"]],"indexes":null},
{"database":"shop","schema":"main","table":"order_items","kind":"table","rows":3,"primary_key":["order_id","line_no"],"unique":[],"indexes":null},
{"database":"shop","schema":"main","table":"stock","kind":"table","rows":0,"primary_key":null,"unique":[],"indexes":null},
{"database":"shop","schema":"main","table":"big_orders","kind":"view","rows":null,"primary_key":null,"unique":null,"indexes":null}]}"#;

    fn shop() -> Catalog {
        Catalog::from_json_text(SHOP).unwrap()
    }

    /// The hover at the `|` in `sql`.
    fn at(sql: &str) -> Option<String> {
        let offset = sql.find('|').expect("a | marks the cursor");
        let src = sql.replacen('|', "", 1);
        hover(&src, offset as u32, &shop()).map(|h| h.contents)
    }

    #[test]
    fn a_table_says_what_the_catalog_knows_of_it() {
        assert_eq!(
            at("SELECT * FROM ord|ers").unwrap(),
            "`shop.main.orders` — table, about 1.24 million rows\n\n\
             Primary key: `id`  \n\
             Indexes: (`customer_id`, `placed`)\n\n\
             ```text\n\
             id           BIGINT NOT NULL\n\
             customer_id  BIGINT NOT NULL\n\
             placed       TIMESTAMP NOT NULL\n\
             total        DECIMAL(10,2)\n\
             Order Note   VARCHAR\n\
             ```"
        );
        let customers = at("FROM orders o JOIN main.custo|mers c ON true").unwrap();
        assert!(
            customers.starts_with(
                "`shop.main.customers` — table, about 2.5 thousand rows\n\n\
                 Primary key: `id`  \nUnique: `email`\n\n"
            ),
            "{customers}"
        );
        // A view has no size and no keys.
        let view = at("SUMMARIZE big_|orders").unwrap();
        assert!(
            view.starts_with("`shop.main.big_orders` — view\n\n```text\n"),
            "{view}"
        );
        // A key of several columns, and the smallest sizes.
        let items = at("TRUNCATE order_items|").unwrap();
        assert!(
            items.starts_with(
                "`shop.main.order_items` — table, about 3 rows\n\n\
                 Primary key: (`order_id`, `line_no`)\n\n"
            ),
            "{items}"
        );
        assert!(
            at("FROM sto|ck")
                .unwrap()
                .starts_with("`shop.main.stock` — table, about 0 rows\n\n```")
        );
    }

    #[test]
    fn an_alias_is_the_table_it_stands_for() {
        let given = at("SELECT o.id FROM orders AS o|").unwrap();
        assert!(
            given.starts_with("`o` is `shop.main.orders` — table, about 1.24 million rows"),
            "{given}"
        );
        let used = at("SELECT o|.id FROM orders o").unwrap();
        assert_eq!(used, given);
        // The table's own name, used as one, is just the table.
        let own = at("SELECT orde|rs.id FROM orders").unwrap();
        assert!(own.starts_with("`shop.main.orders` — table"), "{own}");
    }

    #[test]
    fn a_column_says_its_type_and_its_keys() {
        assert_eq!(
            at("SELECT o.customer_|id FROM orders o").unwrap(),
            "`orders.customer_id` — `BIGINT`, NOT NULL\n\n\
             Column of table `shop.main.orders`. In the index (`customer_id`, `placed`)."
        );
        assert_eq!(
            at("SELECT ema|il FROM customers").unwrap(),
            "`customers.email` — `VARCHAR`, NOT NULL\n\n\
             Column of table `shop.main.customers`. Unique."
        );
        assert_eq!(
            at("SELECT 1 FROM order_items i WHERE i.line_no| = 1").unwrap(),
            "`order_items.line_no` — `INTEGER`, NOT NULL\n\n\
             Column of table `shop.main.order_items`. \
             Part of the primary key (`order_id`, `line_no`)."
        );
        assert_eq!(
            at("SELECT \"Order No|te\" FROM orders").unwrap(),
            "`orders.\"Order Note\"` — `VARCHAR`, nullable\n\n\
             Column of table `shop.main.orders`."
        );
        assert_eq!(
            at("SELECT tot|al FROM big_orders").unwrap(),
            "`big_orders.total` — `DECIMAL(10,2)`, nullable\n\n\
             Column of view `shop.main.big_orders`."
        );
        // Through the table's name, with its schema, and FROM-first.
        assert!(at("SELECT main.orders.pla|ced FROM main.orders").is_some());
        assert!(
            at("SELECT main.ord|ers.placed FROM main.orders")
                .unwrap()
                .starts_with("`shop.main.orders` — table")
        );
        assert!(at("FROM customers SELECT coun|try").is_some());
    }

    #[test]
    fn a_column_is_looked_for_in_the_query_it_is_in_first() {
        // `id` is in both tables, but each query has one.
        let inner =
            at("SELECT id FROM orders WHERE customer_id IN (SELECT i|d FROM customers)").unwrap();
        assert!(inner.starts_with("`customers.id`"), "{inner}");
        let outer =
            at("SELECT i|d FROM orders WHERE customer_id IN (SELECT id FROM customers)").unwrap();
        assert!(outer.starts_with("`orders.id`"), "{outer}");
        // A correlated name is found one query out.
        let correlated = at(
            "SELECT 1 FROM orders o WHERE EXISTS (SELECT 1 FROM customers c WHERE c.id = pla|ced)",
        )
        .unwrap();
        assert!(correlated.starts_with("`orders.placed`"), "{correlated}");
        // Each side of a UNION is its own query.
        let second = at("SELECT id FROM orders UNION ALL SELECT i|d FROM customers").unwrap();
        assert!(second.starts_with("`customers.id`"), "{second}");
    }

    #[test]
    fn nothing_is_said_of_what_could_be_two_things() {
        // In both tables.
        assert_eq!(
            at("SELECT i|d FROM orders o JOIN customers c ON true"),
            None
        );
        // Next to something whose columns are not known.
        assert_eq!(at("SELECT tot|al FROM orders, (SELECT 1 AS total) s"), None);
        assert_eq!(at("SELECT tot|al FROM orders, read_csv('x.csv')"), None);
        assert_eq!(
            at("SELECT tot|al FROM orders JOIN LATERAL (SELECT 1) l ON true"),
            None
        );
        assert_eq!(at("SELECT tot|al FROM orders, 'totals.csv'"), None);
        // An alias of the select list, or a lambda's parameter.
        assert_eq!(
            at("SELECT placed AS total FROM orders ORDER BY tot|al"),
            None
        );
        assert_eq!(at("SELECT id + 1 total FROM orders ORDER BY tot|al"), None);
        assert_eq!(
            at("SELECT list_transform([1], total -> tot|al + 1) FROM orders"),
            None
        );
        // Two relations of one name.
        assert_eq!(
            at("SELECT o|.id FROM orders o JOIN customers o ON true"),
            None
        );
    }

    #[test]
    fn nothing_is_said_of_what_the_catalog_does_not_have() {
        assert_eq!(at("SELECT * FROM ordr|s"), None);
        assert_eq!(at("SELECT o.nop|e FROM orders o"), None);
        assert_eq!(at("SELECT nop|e FROM orders"), None);
        assert_eq!(at("SELECT x.i|d FROM orders o"), None);
        // A CTE or a table of the file's own hides the catalog's.
        assert_eq!(
            at("WITH orders AS (SELECT 1 AS id) SELECT i|d FROM orders"),
            None
        );
        assert_eq!(
            at("WITH orders AS (SELECT 1 AS id) SELECT id FROM ord|ers"),
            None
        );
        assert_eq!(
            at("CREATE TABLE orders (id INT);\nSELECT id FROM ord|ers"),
            None
        );
        // Renamed columns are not the table's.
        assert_eq!(at("SELECT a.x| FROM orders AS a(x, y)"), None);
        assert_eq!(at("SELECT a| FROM orders AS a(x, y)"), None);
    }

    #[test]
    fn nothing_is_said_of_keywords_functions_strings_and_names_being_given() {
        assert_eq!(at("SEL|ECT id FROM orders"), None);
        assert_eq!(at("SELECT total|(1) FROM orders"), None);
        assert_eq!(at("SELECT 'tot|al' FROM orders"), None);
        // A file name is the editor's own hover, not this one.
        assert_eq!(at("SELECT * FROM 'ord|ers.csv'"), None);
        assert_eq!(at("SELECT * FROM read_csv('ord|ers.csv')"), None);
        assert_eq!(at("SELECT id -- tot|al\nFROM orders"), None);
        assert_eq!(at("SELECT id AS tot|al FROM orders"), None);
        assert_eq!(at("SELECT id::tot|al FROM orders"), None);
        assert_eq!(at("SELECT extract(tot|al FROM placed) FROM orders"), None);
        // The schema or the database in a name is not the table.
        assert_eq!(at("SELECT * FROM ma|in.orders"), None);
        // And `extract`'s FROM is not a table to look up.
        assert!(at("SELECT extract(year FROM placed), tot|al FROM orders").is_some());
        assert!(at("SELECT extract(year FROM pla|ced) FROM orders").is_some());
    }

    #[test]
    fn a_name_in_two_databases_is_not_guessed() {
        let two = Catalog::from_json_text(
            r#"[
            {"database":"a","schema":"main","table":"t","column":"x","type":"INT"},
            {"database":"b","schema":"main","table":"t","column":"x","type":"INT"}
            ]"#,
        )
        .unwrap();
        assert_eq!(hover("SELECT x FROM t", 14, &two), None);
        assert_eq!(hover("SELECT x FROM t", 7, &two), None);
        // Said which, it is one.
        let one = hover("SELECT x FROM a.t", 16, &two).unwrap();
        assert!(one.contents.starts_with("`a.main.t` — table"), "{one:?}");
        assert_eq!(one.span, Span::new(16, 17));
    }

    #[test]
    fn a_wide_table_lists_its_first_columns_and_counts_the_rest() {
        let rows: Vec<String> = (0..45)
            .map(|i| {
                format!(
                    r#"{{"database":"d","schema":"main","table":"wide","column":"c{i}","type":"INT"}}"#
                )
            })
            .collect();
        let cat = Catalog::from_json_text(&format!("[{}]", rows.join(","))).unwrap();
        let h = hover("FROM wide", 6, &cat).unwrap().contents;
        assert!(
            h.starts_with("`d.main.wide` — table\n\n```text\nc0   INT\n"),
            "{h}"
        );
        assert!(h.contains("\nc39  INT\n```"), "{h}");
        assert!(!h.contains("c40"), "{h}");
        assert!(h.ends_with("\n\n… 5 more columns (45 in all)"), "{h}");
    }
}
