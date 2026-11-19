//! `textDocument/hover`: what the catalog knows of the name under the
//! cursor, in a line or two.
//!
//! - On a table or a view in a FROM (or after `JOIN`, `INTO`, `UPDATE`,
//!   `DESCRIBE`, ...), or on an alias of one: its full name, what kind it
//!   is, about how many rows it holds and its primary key on one line,
//!   then its first 20 columns with their types.
//! - On a name the catalog has in more than one schema or database: each
//!   place, with its kind and size (the first five, and how many more).
//! - On a column: one line, `table.column`, its type, NOT NULL, and
//!   whether it is a key, unique or indexed.
//! - On a macro the catalog lists: its name and kind.
//!
//! The statement is read off its tokens, as completion reads it (see
//! [`crate::completion`]), so a statement that does not parse still
//! answers. What is said has to be true:
//!
//! - a table name is not a CTE's or a table the file itself creates,
//!   alters or drops;
//! - `alias.column` has to name a relation of the query around it (or of a
//!   query around that one) whose table has the column;
//! - a column with nothing before it is looked for in the catalog tables
//!   of the query around it, and when none of them has it, in the query
//!   around that. Relations whose columns are not known (a subquery, a
//!   table function, a file) do not stop it: in a query that runs, a name
//!   one of the tables has is that table's. One that visibly has a column
//!   of the name does -- a CTE or a subquery whose select list names it,
//!   columns spelled out in an alias -- and then nothing is said. When two
//!   tables have it, both are listed. A name the select list also gives
//!   as an alias, or a lambda's parameter, is left alone.
//!
//! Anything else -- no catalog, a keyword, a built-in function, a string,
//! a name that is not there -- has no hover.

use grebe_rules::catalog::{Catalog, Column, FileContext, Function, Table, quote_ident};
use grebe_syntax::Span;
use grebe_syntax::keyword::Keyword;
use grebe_syntax::token::{Token, TokenKind};

use crate::completion::{
    Binding, Blocks, Scope, Target, is_punct, matching_paren, name_of, named_tables, scope_of,
    statement_at, tables_of, text, upper,
};

/// The most columns a table's hover lists.
const COLUMN_LIMIT: usize = 20;
/// The widest a column's name is padded to.
const NAME_WIDTH: usize = 32;
/// The most places, or tables with a column, one hover lists.
const PLACE_LIMIT: usize = 5;

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

/// `1240000` as `1.2M`: two digits are all an estimate is good for.
pub(crate) fn humanize(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    const UNITS: [(&str, f64); 4] = [("K", 1e3), ("M", 1e6), ("B", 1e9), ("T", 1e12)];
    let mut out = n.to_string();
    for (unit, scale) in UNITS {
        let v = n as f64 / scale;
        let shown = if v < 9.95 {
            format!("{v:.1}")
        } else {
            format!("{v:.0}")
        };
        out = format!("{}{unit}", shown.strip_suffix(".0").unwrap_or(&shown));
        if shown.parse::<f64>().is_ok_and(|r| r < 1000.0) {
            break;
        }
    }
    out
}

fn what(t: &Table) -> &'static str {
    if t.is_view { "view" } else { "table" }
}

/// `table · ~1.2M rows`, and ` · key: (id)` when `key` is asked for and
/// known.
fn summary(t: &Table, key: bool) -> String {
    let mut out = what(t).to_string();
    if let Some(n) = t.row_estimate() {
        out.push_str(&format!(
            " · ~{} row{}",
            humanize(n),
            if n == 1 { "" } else { "s" }
        ));
    }
    if key && !t.primary_key.is_empty() {
        let cols: Vec<String> = t.primary_key.iter().map(|k| quote_ident(k)).collect();
        out.push_str(&format!(" · key: ({})", cols.join(", ")));
    }
    out
}

/// The hover of a table.
fn table_markdown(t: &Table) -> String {
    let mut out = format!("{} — {}", code(&qualified(t)), summary(t, true));
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
    let more = t.columns.len() - shown.len();
    if more > 0 {
        out.push_str(&format!("… {more} more\n"));
    }
    out.push_str("```");
    out
}

/// A list, one per line, of at most [`PLACE_LIMIT`] things, and how many
/// more there are.
fn lines(all: Vec<String>) -> String {
    let more = all.len().saturating_sub(PLACE_LIMIT);
    let mut shown: Vec<String> = all.into_iter().take(PLACE_LIMIT).collect();
    if more > 0 {
        shown.push(format!("… and {more} more"));
    }
    // Two spaces end a line without ending the paragraph.
    shown.join("  \n")
}

/// The hover of a name that is `tables`: one table, or each place of a
/// name the catalog has more than once.
fn describe(tables: &[&Table]) -> Option<String> {
    match tables {
        [] => None,
        [one] => Some(table_markdown(one)),
        _ => Some(lines(
            tables
                .iter()
                .map(|t| format!("{} — {}", code(&qualified(t)), summary(t, false)))
                .collect(),
        )),
    }
}

/// `BIGINT NOT NULL · key`: a column's type, whether it can be NULL, and
/// its place in the table's keys.
fn facts(t: &Table, c: &Column) -> String {
    let mut out = c.data_type.clone();
    if c.nullable == Some(false) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str("NOT NULL");
    }
    let holds = |key: &[String]| key.iter().any(|k| k.eq_ignore_ascii_case(&c.name));
    let mark = if holds(&t.primary_key) {
        Some("key")
    } else if t.unique.iter().any(|k| holds(k)) {
        Some("unique")
    } else if t.indexes.iter().any(|k| holds(k)) {
        Some("indexed")
    } else {
        None
    };
    if let Some(mark) = mark {
        if !out.is_empty() {
            out.push_str(" · ");
        }
        out.push_str(mark);
    }
    out
}

/// A table that has the column a name could be.
struct Hit<'a> {
    /// The relation of the query it is read through.
    binding: &'a Binding,
    table: &'a Table,
    column: &'a Column,
    /// How many places of the catalog the relation's name could be.
    places: usize,
}

/// The hover of a column `name` that is one of `hits`.
fn hits_markdown(name: &str, hits: &[Hit]) -> String {
    if let [hit] = hits {
        // Of a name in two schemas, which one has it.
        let table = if hit.places > 1 {
            qualified(hit.table)
        } else {
            quote_ident(&hit.table.name)
        };
        let head = code(&format!("{table}.{}", quote_ident(&hit.column.name)));
        let facts = facts(hit.table, hit.column);
        return if facts.is_empty() {
            head
        } else {
            format!("{head} {facts}")
        };
    }
    let all: Vec<String> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let relation = code(&quote_ident(&h.binding.name));
            let mut line = if i == 0 {
                format!("{} — in {relation}", code(&quote_ident(name)))
            } else {
                format!("in {relation}")
            };
            if h.places > 1 {
                line.push_str(&format!(" ({})", qualified(h.table)));
            } else if !h.table.name.eq_ignore_ascii_case(&h.binding.name) {
                line.push_str(&format!(" ({})", quote_ident(&h.table.name)));
            }
            let facts = facts(h.table, h.column);
            if !facts.is_empty() {
                line.push(' ');
                line.push_str(&facts);
            }
            line
        })
        .collect();
    lines(all)
}

/// The hover of a macro the catalog lists.
fn function_markdown(f: &Function) -> String {
    let kind = if f.kind.is_empty() {
        "function".to_string()
    } else {
        f.kind.replace('_', " ")
    };
    format!("{} — {kind}", code(&f.name))
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

    /// The catalog tables in reach of token `at` that have a column
    /// `name`: those of the innermost query that has one. Empty when none
    /// has, or when a relation that is not the catalog's visibly has a
    /// column of that name too.
    fn column(&self, name: &str, at: usize) -> Vec<Hit<'a>> {
        let named = |cols: &[String]| cols.iter().any(|c| c.eq_ignore_ascii_case(name));
        let insert = upper(self.src, self.t.first()) == "INSERT";
        for block in self.blocks.blocks_at(self.src, self.t, at) {
            let mut hits = Vec::new();
            for b in self
                .scope
                .bindings
                .iter()
                .filter(|b| self.blocks.member(b.at, &block))
            {
                // What `INSERT INTO t` writes is only named in its list of
                // columns; the rest of the statement reads other tables.
                if b.written && insert && self.blocks.parent(at) != Some(b.name_tokens.1) {
                    continue;
                }
                match &b.target {
                    Target::Columns(cols) => {
                        if named(cols) {
                            return Vec::new();
                        }
                    }
                    Target::Unknown => {
                        if self.subquery_names(b.at, name) {
                            return Vec::new();
                        }
                    }
                    Target::Named(parts) => {
                        let last = parts.last().map(|p| p.to_ascii_lowercase());
                        if let (1, Some((cols, body))) = (parts.len(), self.scope.cte(&parts[0])) {
                            let has = match cols {
                                Some(cols) => named(cols),
                                None => self.select_names(body, name),
                            };
                            if has {
                                return Vec::new();
                            }
                            continue;
                        }
                        if let Some(local) = last.and_then(|l| self.file.local.get(&l)) {
                            if local.columns.as_deref().is_some_and(named) {
                                return Vec::new();
                            }
                            continue;
                        }
                        let tables = named_tables(parts, self.scope, self.file, self.catalog);
                        for table in &tables {
                            if let Some(column) = table.column(name) {
                                hits.push(Hit {
                                    binding: b,
                                    table,
                                    column,
                                    places: tables.len(),
                                });
                            }
                        }
                    }
                }
            }
            if !hits.is_empty() {
                return hits;
            }
        }
        Vec::new()
    }

    /// Does the subquery the relation at token `at` is (`(SELECT ...)`,
    /// `LATERAL (SELECT ...)`) name a column `name` in its select list?
    fn subquery_names(&self, at: usize, name: &str) -> bool {
        let open = if upper(self.src, self.t.get(at)) == "LATERAL" {
            at + 1
        } else {
            at
        };
        is_punct(self.src, self.t.get(open), "(") && self.select_names(open, name)
    }

    /// Does the query in the parentheses at token `open` name a column
    /// `name` in its (first) select list: as a column, `x.name`, or as an
    /// alias? A `*` names nothing here.
    fn select_names(&self, open: usize, name: &str) -> bool {
        let Some(close) = matching_paren(self.src, self.t, open) else {
            return false;
        };
        let direct: Vec<usize> = (open + 1..close)
            .filter(|&i| self.blocks.parent(i) == Some(open))
            .collect();
        let Some(select) = direct
            .iter()
            .position(|&i| upper(self.src, self.t.get(i)) == "SELECT")
        else {
            return false;
        };
        let mut item: Vec<usize> = Vec::new();
        let mut items: Vec<Vec<usize>> = Vec::new();
        for &i in &direct[select + 1..] {
            let tok = &self.t[i];
            if is_punct(self.src, Some(tok), ",") {
                items.push(std::mem::take(&mut item));
                continue;
            }
            let word = upper(self.src, Some(tok));
            if matches!(
                word.as_str(),
                "FROM"
                    | "WHERE"
                    | "GROUP"
                    | "HAVING"
                    | "ORDER"
                    | "LIMIT"
                    | "OFFSET"
                    | "QUALIFY"
                    | "WINDOW"
                    | "UNION"
                    | "EXCEPT"
                    | "INTERSECT"
                    | "INTO"
            ) {
                break;
            }
            if item.is_empty() && matches!(word.as_str(), "DISTINCT" | "ALL") {
                continue;
            }
            item.push(i);
        }
        items.push(item);
        items.iter().any(|item| {
            let Some(&last) = item.last() else {
                return false;
            };
            let tok = &self.t[last];
            let reserved = tok.kind == TokenKind::Word
                && grebe_syntax::grammar::classify_keyword(text(self.src, tok))
                    .has(Keyword::RESERVED);
            if reserved || !name_of(self.src, tok).is_some_and(|n| n.eq_ignore_ascii_case(name)) {
                return false;
            }
            // The name alone, `x.name`, `... AS name`, `expr name`; not the
            // last operand of `a + name` or `x::name`.
            match item.len().checked_sub(2).map(|p| &self.t[item[p]]) {
                None => true,
                Some(prev) if prev.kind == TokenKind::Punct => {
                    matches!(text(self.src, prev), "." | ")" | "]")
                }
                Some(_) => true,
            }
        })
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
                if class.has(Keyword::RESERVED) {
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
            && grebe_syntax::grammar::classify_keyword(text(src, tok)).has(Keyword::RESERVED)
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
            return describe(&tables_of(b, &scope, &file, catalog)).and_then(answer);
        }
        if b.name_tokens.0 <= k && k < b.name_tokens.1 {
            if k + 1 != b.name_tokens.1 {
                return None;
            }
            return describe(&tables_of(b, &scope, &file, catalog)).and_then(answer);
        }
    }
    // A function: a macro the catalog lists, and nothing for a built-in.
    // Nor for a name before a string (`DATE '2026-01-01'`).
    if part == last && is_punct(src, t.get(hi + 1), "(") {
        let name = &chain[last];
        let f = catalog
            .functions()
            .find(|f| f.name.eq_ignore_ascii_case(name))?;
        return answer(function_markdown(f));
    }
    if part == last && t.get(hi + 1).is_some_and(|n| n.kind == TokenKind::String) {
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
        return describe(&named_tables(&chain, &scope, &file, catalog)).and_then(answer);
    }

    if part > 0 {
        // `relation.column`; or the table in `schema.table.column`.
        if let Some(b) = reading.relation(&chain[..part], k) {
            let tables = tables_of(b, &scope, &file, catalog);
            let hits: Vec<Hit> = tables
                .iter()
                .filter_map(|table| {
                    table.column(&chain[part]).map(|column| Hit {
                        binding: b,
                        table,
                        column,
                        places: tables.len(),
                    })
                })
                .collect();
            if hits.is_empty() {
                return None;
            }
            return answer(hits_markdown(&chain[part], &hits));
        }
        if part < last {
            let b = reading.relation(&chain[..=part], k)?;
            return describe(&tables_of(b, &scope, &file, catalog)).and_then(answer);
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
    let hits = if shadowed {
        Vec::new()
    } else {
        reading.column(name, k)
    };
    match (relation, hits.is_empty()) {
        (Some(b), true) if last > 0 || !shadowed => {
            describe(&tables_of(b, &scope, &file, catalog)).and_then(answer)
        }
        (None, false) => answer(hits_markdown(name, &hits)),
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
            "`shop.main.orders` — table · ~1.2M rows · key: (id)\n\n\
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
            customers
                .starts_with("`shop.main.customers` — table · ~2.5K rows · key: (id)\n\n```text\n"),
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
                "`shop.main.order_items` — table · ~3 rows · key: (order_id, line_no)\n\n"
            ),
            "{items}"
        );
        assert!(
            at("FROM sto|ck")
                .unwrap()
                .starts_with("`shop.main.stock` — table · ~0 rows\n\n```")
        );
    }

    #[test]
    fn sizes_are_short() {
        for (n, text) in [
            (0, "0"),
            (999, "999"),
            (1000, "1K"),
            (2500, "2.5K"),
            (9_960, "10K"),
            (999_999, "1M"),
            (1_240_000, "1.2M"),
            (3_400_000, "3.4M"),
            (12_000_000_000, "12B"),
            (5_000_000_000_000_000, "5000T"),
        ] {
            assert_eq!(humanize(n), text, "{n}");
        }
    }

    #[test]
    fn an_alias_is_the_table_it_stands_for() {
        let given = at("SELECT o.id FROM orders AS o|").unwrap();
        assert!(
            given.starts_with("`shop.main.orders` — table · ~1.2M rows · key: (id)\n"),
            "{given}"
        );
        let used = at("SELECT o|.id FROM orders o").unwrap();
        assert_eq!(used, given);
        // The table's own name, used as one, is just the table.
        let own = at("SELECT orde|rs.id FROM orders").unwrap();
        assert_eq!(own, given);
    }

    #[test]
    fn a_column_is_one_line_of_its_type_and_its_keys() {
        assert_eq!(
            at("SELECT o.customer_|id FROM orders o").unwrap(),
            "`orders.customer_id` BIGINT NOT NULL · indexed"
        );
        assert_eq!(
            at("SELECT ema|il FROM customers").unwrap(),
            "`customers.email` VARCHAR NOT NULL · unique"
        );
        assert_eq!(
            at("SELECT 1 FROM order_items i WHERE i.line_no| = 1").unwrap(),
            "`order_items.line_no` INTEGER NOT NULL · key"
        );
        assert_eq!(
            at("SELECT \"Order No|te\" FROM orders").unwrap(),
            "`orders.\"Order Note\"` VARCHAR"
        );
        assert_eq!(
            at("SELECT tot|al FROM big_orders").unwrap(),
            "`big_orders.total` DECIMAL(10,2)"
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
        // What `INSERT INTO` writes is in its column list only.
        assert_eq!(
            at("INSERT INTO stock (ware|house) SELECT sku FROM order_items").unwrap(),
            "`stock.warehouse` VARCHAR"
        );
        assert_eq!(
            at("INSERT INTO stock SELECT order_i|d, line_no, sku FROM order_items").unwrap(),
            "`order_items.order_id` BIGINT NOT NULL · key"
        );
        assert_eq!(
            at("UPDATE stock SET warehouse = 'x' WHERE order_i|d = 1").unwrap(),
            "`stock.order_id` BIGINT"
        );
    }

    #[test]
    fn a_column_two_tables_have_is_listed_for_each() {
        assert_eq!(
            at("SELECT i|d FROM orders o JOIN customers c ON true").unwrap(),
            "`id` — in `o` (orders) BIGINT NOT NULL · key  \n\
             in `c` (customers) BIGINT NOT NULL · key"
        );
        // A table joined to itself, under its own name and another.
        assert_eq!(
            at("SELECT plac|ed FROM orders JOIN orders b ON true").unwrap(),
            "`placed` — in `orders` TIMESTAMP NOT NULL · indexed  \n\
             in `b` (orders) TIMESTAMP NOT NULL · indexed"
        );
    }

    #[test]
    fn a_relation_whose_columns_are_not_known_does_not_stop_a_column() {
        let total = Some("`orders.total` DECIMAL(10,2)".to_string());
        for sql in [
            "SELECT tot|al FROM orders, read_csv('x.csv')",
            "SELECT tot|al FROM orders, 'totals.csv'",
            "SELECT tot|al FROM orders JOIN LATERAL (SELECT 1) l ON true",
            "SELECT tot|al FROM orders, (SELECT 1 AS other) s",
            "SELECT tot|al FROM orders, (SELECT total + 1 FROM stock) s",
            "WITH w AS (SELECT 1 AS other) SELECT tot|al FROM orders, w",
            "SELECT tot|al FROM orders, range(3) r(i)",
            "SELECT tot|al FROM orders, unknown_table",
            "CREATE TABLE made AS SELECT 1;\nSELECT tot|al FROM orders, made",
        ] {
            assert_eq!(at(sql), total, "{sql}");
        }
    }

    #[test]
    fn a_relation_that_visibly_has_the_column_too_stops_it() {
        for sql in [
            "WITH w AS (SELECT 1 AS total) SELECT tot|al FROM orders, w",
            "WITH w AS (SELECT o.total FROM orders o) SELECT tot|al FROM orders, w",
            "WITH w AS (SELECT DISTINCT total FROM orders) SELECT tot|al FROM orders, w",
            "WITH w AS (SELECT count(*) total FROM orders) SELECT tot|al FROM orders, w",
            "WITH w(total) AS (SELECT 1) SELECT tot|al FROM orders, w",
            "SELECT tot|al FROM orders, (SELECT 1 AS total) s",
            "SELECT tot|al FROM orders, (SELECT 1 AS x, \"total\" FROM stock) s",
            "SELECT tot|al FROM orders JOIN LATERAL (SELECT 1 AS total) l ON true",
            "SELECT tot|al FROM orders, range(3) r(total)",
            "CREATE TABLE made (total INT);\nSELECT tot|al FROM orders, made",
        ] {
            assert_eq!(at(sql), None, "{sql}");
        }
    }

    #[test]
    fn nothing_is_said_of_what_could_be_two_things() {
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
        assert_eq!(at("SELECT upp|er(email) FROM customers"), None);
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
    fn a_macro_of_the_catalog_says_what_kind_it_is() {
        let cat = Catalog::from_json_text(
            r#"{"columns": [
            {"database":"d","schema":"main","table":"t","column":"x","type":"INT"}],
            "functions": [{"name":"add1","kind":"macro"},{"name":"series","kind":"table_macro"}]}"#,
        )
        .unwrap();
        let at = |sql: &str| {
            let offset = sql.find('|').unwrap();
            hover(&sql.replacen('|', "", 1), offset as u32, &cat).map(|h| h.contents)
        };
        assert_eq!(at("SELECT add|1(x) FROM t").unwrap(), "`add1` — macro");
        assert_eq!(at("SELECT ADD1|(x) FROM t").unwrap(), "`add1` — macro");
        assert_eq!(at("FROM ser|ies(3)").unwrap(), "`series` — table macro");
        // A built-in, or a macro's name used as something else.
        assert_eq!(at("SELECT upp|er(x) FROM t"), None);
        assert_eq!(at("SELECT add|1 FROM t"), None);
    }

    #[test]
    fn a_name_in_two_schemas_lists_each_place() {
        let mut rows = vec![
            r#"{"database":"pg","schema":"public","table":"orders","column":"id","type":"INTEGER","nullable":false}"#.to_string(),
            r#"{"database":"pg","schema":"staging","table":"orders","column":"id","type":"INTEGER","kind":"view"}"#.to_string(),
            r#"{"database":"pg","schema":"staging","table":"orders","column":"note","type":"VARCHAR","kind":"view"}"#.to_string(),
        ];
        for s in 0..7 {
            rows.push(format!(
                r#"{{"database":"pg","schema":"s{s}","table":"events","column":"x","type":"INT"}}"#
            ));
        }
        let cat = Catalog::from_json_text(&format!(
            r#"{{"columns": [{}], "tables": [
            {{"database":"pg","schema":"public","table":"orders","kind":"table","rows":1240000,"primary_key":["id"]}}]}}"#,
            rows.join(",")
        ))
        .unwrap();
        let at = |sql: &str| {
            let offset = sql.find('|').unwrap();
            hover(&sql.replacen('|', "", 1), offset as u32, &cat).map(|h| h.contents)
        };
        assert_eq!(
            at("SELECT id FROM ord|ers o").unwrap(),
            "`pg.public.orders` — table · ~1.2M rows  \n`pg.staging.orders` — view"
        );
        assert_eq!(
            at("SELECT o|.id FROM orders o").unwrap(),
            at("SELECT id FROM ord|ers o").unwrap()
        );
        assert_eq!(
            at("DESCRIBE eve|nts").unwrap(),
            "`pg.s0.events` — table  \n`pg.s1.events` — table  \n`pg.s2.events` — table  \n\
             `pg.s3.events` — table  \n`pg.s4.events` — table  \n… and 2 more"
        );
        // A column of it: each place that has it.
        assert_eq!(
            at("SELECT i|d FROM orders o").unwrap(),
            "`id` — in `o` (pg.public.orders) INTEGER NOT NULL · key  \n\
             in `o` (pg.staging.orders) INTEGER"
        );
        assert_eq!(
            at("SELECT o.i|d FROM orders o").unwrap(),
            at("SELECT i|d FROM orders o").unwrap()
        );
        assert_eq!(
            at("SELECT not|e FROM orders").unwrap(),
            "`pg.staging.orders.note` VARCHAR"
        );
        // Said which, it is one.
        assert_eq!(
            at("SELECT i|d FROM public.orders").unwrap(),
            "`orders.id` INTEGER NOT NULL · key"
        );
        let one = at("SELECT id FROM staging.ord|ers").unwrap();
        assert!(one.starts_with("`pg.staging.orders` — view\n\n"), "{one}");
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
        assert!(h.ends_with("\nc19  INT\n… 25 more\n```"), "{h}");
        assert!(!h.contains("c20"), "{h}");
    }
}
