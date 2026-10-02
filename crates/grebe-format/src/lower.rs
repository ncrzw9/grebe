//! CST → `Doc`. This is where the formatting rules become code.
//!
//! # Shape of the approach
//!
//! The vendored grammar has 1,071 rules. Hand-writing a layout for each one
//! would be a second grammar, drifting from the first. Instead there is one
//! **generic** lowering that any node can take, and a short list of rules
//! with an opinion of their own:
//!
//! - **Generic:** a node's direct tokens and children, in source order, joined
//!   by the spacing rule in [`Lowerer::sep`]. Brackets become a group that
//!   indents its contents and breaks when it does not fit; commas become
//!   `,` + `Line`, so any comma list breaks one item per line inside whatever
//!   group contains it.
//! - **Clauses** (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, ...) start a line in
//!   the enclosing statement group and own a group for their body.
//! - **Statements** are not grouped by themselves. The caller groups them —
//!   the file does for a top-level statement, the bracket does for a
//!   subquery — so a subquery that lands on its own lines breaks its clauses
//!   too, instead of re-fitting them onto one line inside the parens.
//! - **`AND`/`OR` chains** break one condition per line, operator leading.
//! - **`CASE`** puts each `WHEN` on a line once it no longer fits.
//!
//! # What never changes
//!
//! The formatter never changes the token stream, only the space between
//! tokens, with three exceptions: case (keywords, function and type names);
//! a trailing comma that a list had, which is kept when the list is broken
//! (diff-minimal, and DuckDB accepts it) and dropped when it collapses to one
//! line, and never added; and a statement's terminating `;`, which is added
//! when missing, so every statement ends the same way.
//!
//! # Keyword case without a keyword table
//!
//! `data`, `value` and `name` are unreserved keywords and ordinary column
//! names. Whether a word is a keyword here is not a property of the word: it
//! is a property of how the grammar matched it. A word matched by an
//! identifier rule (the `AddRuleOverride` surface, plus the five keyword-
//! category rules `ColId` and friends are built from) is an identifier and
//! its case is meaning; a word matched by a `'LITERAL'` is a keyword and is
//! cased by the knob. That decision is read off the tree: a token exactly
//! covered by an identifier-rule node was matched as an identifier.

use std::cell::Cell;
use std::collections::HashMap;

use grebe_syntax::cst::{NodeId, Tree};
use grebe_syntax::grammar::{EXPRESSIONS, Expr, RULES, classify_keyword};
use grebe_syntax::keyword::Keyword;
use grebe_syntax::token::{Token, TokenKind};

use crate::doc::Doc;
use crate::{KeywordCase, Options};

/// Rules whose node starts a new line in the enclosing statement group.
const CLAUSE_RULES: &[&str] = &[
    "WhereClause",
    "GroupByClause",
    "HavingClause",
    "QualifyClause",
    "WindowClause",
    "OrderByClause",
    "LimitClause",
    "OffsetClause",
    "FetchClause",
    "FromClause",
    "SampleClause",
    "OnConflictClause",
    "ReturningClause",
    "DeleteUsingClause",
    "JoinClause",
    "JoinOrPivot",
    "TablePivotClause",
    "TableUnpivotClause",
    "SetopClause",
    "SetIntersectClause",
    "PivotOn",
    "PivotUsing",
    "PivotGroupByList",
    "IntoNameValues",
    "MergeIntoUsingClause",
    "JoinQualifier",
    "MergeMatch",
];

/// Rules that are a whole statement; a keyword before one (`AS`, `EXPLAIN`,
/// `INSERT INTO t`) is followed by a line break rather than a space.
const STATEMENT_RULES: &[&str] = &[
    "Statement",
    "SelectStatement",
    "SelectStatementInternal",
    "InsertValues",
    "SelectInsertValues",
    "DefaultValues",
];

/// Rules whose `(` hugs the word before it: calls, casts, type modifiers.
const TIGHT_PAREN_RULES: &[&str] = &[
    "FunctionExpressionArguments",
    "TableFunctionArguments",
    "MethodExpressionArguments",
    "TypeModifiers",
    "MacroDefinition",
    "FloatType",
    "BitType",
    "GeometryType",
    "MapType",
    "TupleType",
    "IntervalNumber",
    "ColIdTypeList",
    "CastExpression",
    "CoalesceExpression",
    "UnpackExpression",
    "TryExpression",
    "ColumnsExpression",
    "ExtractExpression",
    "NullIfExpression",
    "PositionExpression",
    "RowExpression",
    "SubstringExpression",
    "TrimExpression",
    "OverlayExpression",
    "GroupingExpression",
    "CubeOrRollupClause",
    "QualifiedOperator",
];

/// Rules whose `[` hugs what precedes it: indexing, slicing, array types.
const TIGHT_BRACKET_RULES: &[&str] = &[
    "SliceExpression",
    "SquareBracketsArray",
    "ArrayKeywordWithBounds",
];

/// Naming rules whose identifiers are lower-cased: function and type names.
const LOWER_NAME_RULES: &[&str] = &[
    "FunctionName",
    "TableFunctionName",
    "ReservedFunctionName",
    "TypeName",
    "ReservedTypeName",
];

/// Rules implemented natively in the matcher that match a *word* as an
/// identifier of some keyword category. Their compiled expression is a
/// placeholder rather than an `Expr::Identifier`, so the expression scan in
/// [`Lowerer::new`] cannot see them; they have to be named here.
const KEYWORD_CATEGORY_RULES: &[&str] = &[
    "ReservedKeyword",
    "UnreservedKeyword",
    "ColumnNameKeyword",
    "FuncNameKeyword",
    "TypeNameKeyword",
];

#[derive(Clone, Copy)]
enum Item {
    Tok(usize),
    Node(NodeId),
}

struct Comment {
    text: String,
    line: bool,
}

/// One `TopLevelStatement`: its statement, if it has one, and the comments
/// carried by any `;` the layout drops (a bare one, or the extras in `;;`).
/// Those comments belong to whatever comes next, which is where re-formatting
/// the output will find them.
struct TopLevel {
    stmt: Option<Doc>,
    dropped: Vec<Doc>,
}

pub(crate) struct Lowerer<'a> {
    tree: &'a Tree,
    src: &'a str,
    opts: &'a Options,
    /// Token index → the node that owns it directly.
    owner: Vec<Option<NodeId>>,
    by_start: HashMap<u32, usize>,
    by_end: HashMap<u32, usize>,
    leading: HashMap<usize, Vec<Comment>>,
    trailing: HashMap<usize, Vec<Comment>>,
    file_trailing: Vec<Comment>,
    /// Rule id → matches words as identifiers.
    ident_rule: Vec<bool>,
    /// Node id → sits under a `Type` node.
    in_type: Vec<bool>,
    /// The last code token of the statement being lowered. A list's trailing
    /// comma there would print as `,;`, so it is dropped even when the list
    /// is broken.
    stmt_end: Cell<Option<usize>>,
}

impl<'a> Lowerer<'a> {
    pub(crate) fn new(tree: &'a Tree, src: &'a str, opts: &'a Options) -> Self {
        let toks = &tree.tokens;
        let mut by_start = HashMap::new();
        let mut by_end = HashMap::new();
        for (i, t) in toks.iter().enumerate() {
            if !t.kind.is_trivia() {
                by_start.insert(t.span.start, i);
                by_end.insert(t.span.end, i);
            }
        }
        let mut owner = vec![None; toks.len()];
        for (n, node) in tree.nodes.iter().enumerate() {
            for t in &node.tokens {
                if let Some(&i) = by_start.get(&t.span.start) {
                    owner[i] = Some(NodeId(n as u32));
                }
            }
        }

        let ident_rule = RULES
            .iter()
            .map(|r| {
                if KEYWORD_CATEGORY_RULES.contains(&r.name) {
                    return true;
                }
                match &EXPRESSIONS[r.expr.0 as usize] {
                    Expr::Identifier(_) => true,
                    Expr::Choice(arms) => arms
                        .iter()
                        .any(|a| matches!(EXPRESSIONS[a.0 as usize], Expr::Identifier(_))),
                    _ => false,
                }
            })
            .collect();

        let mut in_type = vec![false; tree.nodes.len()];
        for id in tree.walk() {
            let parent_in = tree.parent(id).is_some_and(|p| in_type[p.0 as usize]);
            in_type[id.0 as usize] = parent_in || tree.rule_name(id) == "Type";
        }

        let mut me = Self {
            tree,
            src,
            opts,
            owner,
            by_start,
            by_end,
            leading: HashMap::new(),
            trailing: HashMap::new(),
            file_trailing: Vec::new(),
            ident_rule,
            in_type,
            stmt_end: Cell::new(None),
        };
        me.attach_comments();
        me
    }

    /// A comment on the same line as the code before it trails that code;
    /// any other comment leads the next code token. Comments after the last
    /// code token, on later lines, belong to the file.
    fn attach_comments(&mut self) {
        let mut last_code: Option<usize> = None;
        let mut newline_since = true;
        let mut pending: Vec<Comment> = Vec::new();
        for (i, t) in self.tree.tokens.iter().enumerate() {
            match t.kind {
                TokenKind::Whitespace => {
                    if self.text(t).contains('\n') {
                        newline_since = true;
                    }
                }
                TokenKind::LineComment | TokenKind::BlockComment => {
                    let c = Comment {
                        text: self.text(t).trim_end().to_string(),
                        line: t.kind == TokenKind::LineComment,
                    };
                    match last_code {
                        Some(k) if !newline_since => self.trailing.entry(k).or_default().push(c),
                        _ => pending.push(c),
                    }
                }
                _ => {
                    if !pending.is_empty() {
                        self.leading.insert(i, std::mem::take(&mut pending));
                    }
                    last_code = Some(i);
                    newline_since = false;
                }
            }
        }
        self.file_trailing = pending;
    }

    fn text(&self, t: &Token) -> &'a str {
        &self.src[t.span.start as usize..t.span.end as usize]
    }

    fn tok(&self, i: usize) -> &Token {
        &self.tree.tokens[i]
    }

    fn rule(&self, id: NodeId) -> &'static str {
        self.tree.rule_name(id)
    }

    fn owner_rule(&self, i: usize) -> &'static str {
        self.owner[i].map_or("", |n| self.rule(n))
    }

    // ----- items -----------------------------------------------------------

    /// A node's direct code tokens and non-empty children, in source order.
    fn items(&self, id: NodeId) -> Vec<Item> {
        let node = self.tree.node(id);
        let mut out: Vec<(u32, Item)> = Vec::new();
        for t in &node.tokens {
            if t.kind.is_trivia() {
                continue;
            }
            if let Some(&i) = self.by_start.get(&t.span.start) {
                out.push((t.span.start, Item::Tok(i)));
            }
        }
        for &c in &node.children {
            let sp = self.tree.node(c).span;
            if !sp.is_empty() {
                out.push((sp.start, Item::Node(c)));
            }
        }
        out.sort_by_key(|(s, _)| *s);
        out.into_iter().map(|(_, it)| it).collect()
    }

    fn first_tok(&self, it: Item) -> usize {
        match it {
            Item::Tok(i) => i,
            Item::Node(n) => self.by_start[&self.tree.node(n).span.start],
        }
    }

    fn last_tok(&self, it: Item) -> usize {
        match it {
            Item::Tok(i) => i,
            Item::Node(n) => self.by_end[&self.tree.node(n).span.end],
        }
    }

    fn item_text(&self, it: Item) -> &'a str {
        match it {
            Item::Tok(i) => self.text(self.tok(i)),
            Item::Node(n) => self.tree.text(n, self.src),
        }
    }

    fn is_tok(&self, it: Item, s: &str) -> bool {
        matches!(it, Item::Tok(_)) && self.item_text(it) == s
    }

    // ----- tokens ----------------------------------------------------------

    /// Is this word token an identifier, by how the grammar matched it?
    fn matched_as_identifier(&self, i: usize) -> bool {
        self.exact_cover(i)
            .into_iter()
            .any(|n| self.ident_rule[self.tree.node(n).rule.0 as usize])
    }

    /// The chain of nodes whose span is exactly this token's, innermost first.
    fn exact_cover(&self, i: usize) -> Vec<NodeId> {
        let sp = self.tok(i).span;
        let mut out = Vec::new();
        let mut cur = self.owner[i];
        while let Some(n) = cur {
            if self.tree.node(n).span != sp {
                break;
            }
            out.push(n);
            cur = self.tree.parent(n);
        }
        out
    }

    fn cased(&self, i: usize) -> String {
        let t = self.tok(i);
        let raw = self.text(t);
        if t.kind != TokenKind::Word {
            return raw.to_string();
        }
        if self.matched_as_identifier(i) {
            let lower_name = self
                .exact_cover(i)
                .into_iter()
                .any(|n| LOWER_NAME_RULES.contains(&self.rule(n)));
            if lower_name {
                return raw.to_ascii_lowercase();
            }
            return raw.to_string();
        }
        if classify_keyword(raw) == Keyword::NONE {
            return raw.to_string();
        }
        // Type names are lower even when the grammar spells them as keyword
        // literals (`integer`, `timestamp with time zone`).
        let under_type = self.owner[i].is_some_and(|n| self.in_type[n.0 as usize]);
        if under_type {
            return raw.to_ascii_lowercase();
        }
        match self.opts.keyword_case {
            KeywordCase::Upper => raw.to_ascii_uppercase(),
            KeywordCase::Lower => raw.to_ascii_lowercase(),
        }
    }

    fn comments_leading(&self, i: usize, parts: &mut Vec<Doc>) {
        if let Some(cs) = self.leading.get(&i) {
            for c in cs {
                if c.line {
                    parts.push(Doc::OwnLine);
                    parts.push(Doc::text(c.text.clone()));
                    parts.push(Doc::OwnLine);
                } else {
                    parts.push(Doc::text(c.text.clone()));
                    parts.push(Doc::text(" "));
                }
            }
        }
    }

    fn comments_trailing(&self, i: usize, parts: &mut Vec<Doc>) {
        if let Some(cs) = self.trailing.get(&i) {
            for c in cs {
                if c.line {
                    parts.push(Doc::LineSuffix(format!(" {}", c.text)));
                    parts.push(Doc::BreakParent);
                } else {
                    parts.push(Doc::text(format!(" {}", c.text)));
                }
            }
        }
    }

    /// A token with its comments, with the given text in place of its own.
    fn tok_doc_as(&self, i: usize, text: Doc) -> Doc {
        let mut parts = Vec::new();
        self.comments_leading(i, &mut parts);
        parts.push(text);
        self.comments_trailing(i, &mut parts);
        if parts.len() == 1 {
            parts.pop().unwrap_or(Doc::Nil)
        } else {
            Doc::concat(parts)
        }
    }

    fn tok_doc(&self, i: usize) -> Doc {
        self.tok_doc_as(i, Doc::text(self.cased(i)))
    }

    // ----- spacing ---------------------------------------------------------

    /// What goes between two adjacent items.
    fn sep(&self, left: Item, right: Item) -> Doc {
        if let Item::Node(n) = right {
            let r = self.rule(n);
            if CLAUSE_RULES.contains(&r) {
                return Doc::Line;
            }
            if STATEMENT_RULES.contains(&r) {
                return Doc::Line;
            }
        }
        let li = self.last_tok(left);
        let ri = self.first_tok(right);
        // Two string literals separated by a newline are one literal to the
        // engine (`NewlineBefore`); on one line they are a syntax error. The
        // newline is grammar, not layout, so it is kept.
        if self.tok(li).kind == TokenKind::String && self.tok(ri).kind == TokenKind::String {
            return Doc::HardLine;
        }
        let l = self.text(self.tok(li));
        let r = self.text(self.tok(ri));

        match r {
            "," | ";" | ")" | "]" | "}" | "." | "::" | ":" => return Doc::Nil,
            _ => {}
        }
        match l {
            "(" | "[" | "{" | "." | "::" | "$" | "#" => return Doc::Nil,
            ":" => {
                let slice = matches!(
                    self.owner_rule(li),
                    "SliceExpression" | "SliceBound" | "EndSliceBound" | "StepSliceBound"
                );
                return if slice { Doc::Nil } else { Doc::text(" ") };
            }
            _ => {}
        }
        if r == "(" {
            // `WITH r(n) AS (...)` hugs; `INSERT INTO t (a, b)` does not.
            let cte_columns = self.owner_rule(ri) == "InsertColumnList"
                && self.owner[ri]
                    .and_then(|n| self.tree.parent(n))
                    .is_some_and(|p| self.rule(p) == "WithStatement");
            return if cte_columns || TIGHT_PAREN_RULES.contains(&self.owner_rule(ri)) {
                Doc::Nil
            } else {
                Doc::text(" ")
            };
        }
        if r == "[" {
            return if TIGHT_BRACKET_RULES.contains(&self.owner_rule(ri)) {
                Doc::Nil
            } else {
                Doc::text(" ")
            };
        }
        if matches!(
            self.owner_rule(li),
            "MinusPrefixOperator" | "PlusPrefixOperator" | "TildePrefixOperator"
        ) {
            return Doc::Nil;
        }
        if r == "!" && self.owner_rule(ri) == "PostfixOperator" {
            return Doc::Nil;
        }
        if l == "?" && self.owner_rule(li) == "QuestionMarkNumberedParameter" {
            return Doc::Nil;
        }
        if r == "%" && matches!(self.owner_rule(ri), "LimitExpression" | "SamplePercentage") {
            return Doc::Nil;
        }
        Doc::text(" ")
    }

    // ----- generic ---------------------------------------------------------

    fn lower_item(&self, it: Item) -> Doc {
        match it {
            Item::Tok(i) => self.tok_doc(i),
            Item::Node(n) => self.lower(n),
        }
    }

    /// Lay out a run of items: brackets group and indent, commas break,
    /// everything else is joined by [`Self::sep`].
    fn seq(&self, items: &[Item]) -> Doc {
        let mut parts: Vec<Doc> = Vec::new();
        let mut prev: Option<Item> = None;
        let mut i = 0;
        while i < items.len() {
            let it = items[i];
            if let Some(p) = prev {
                // A comma already pushed its `Line`; nothing else goes before
                // one, either.
                if !self.is_tok(it, ",") && !self.is_tok(p, ",") {
                    parts.push(self.sep(p, it));
                }
            }
            if let Item::Tok(ti) = it {
                let text = self.text(self.tok(ti));
                if let Some(close) = matching_close(text) {
                    if let Some(j) = self.find_close(items, i, text, close) {
                        let inner = self.seq(&items[i + 1..j]);
                        let open = self.tok_doc(ti);
                        let close_doc = if let Item::Tok(cj) = items[j] {
                            self.tok_doc(cj)
                        } else {
                            Doc::Nil
                        };
                        let empty = i + 1 == j;
                        parts.push(if empty {
                            Doc::concat(vec![open, close_doc])
                        } else {
                            Doc::group(Doc::concat(vec![
                                open,
                                Doc::indent(Doc::concat(vec![Doc::SoftLine, inner])),
                                Doc::SoftLine,
                                close_doc,
                            ]))
                        });
                        prev = Some(items[j]);
                        i = j + 1;
                        continue;
                    }
                }
                // Commas trail, never lead: leading-comma style exists to dodge
                // engines that reject a trailing comma, and DuckDB accepts one.
                if text == "," {
                    let trailing = i + 1 >= items.len();
                    if trailing {
                        let at_statement_end = self.stmt_end.get() == Some(ti);
                        let comma = if at_statement_end {
                            Doc::Nil
                        } else {
                            Doc::if_break(Doc::text(","), Doc::Nil)
                        };
                        parts.push(self.tok_doc_as(ti, comma));
                    } else {
                        parts.push(self.tok_doc(ti));
                        parts.push(Doc::Line);
                    }
                    prev = Some(it);
                    i += 1;
                    continue;
                }
            }
            parts.push(self.lower_item(it));
            prev = Some(it);
            i += 1;
        }
        Doc::concat(parts)
    }

    fn find_close(&self, items: &[Item], open_at: usize, open: &str, close: &str) -> Option<usize> {
        let mut depth = 0usize;
        for (j, it) in items.iter().enumerate().skip(open_at) {
            if let Item::Tok(_) = it {
                let t = self.item_text(*it);
                if t == open {
                    depth += 1;
                } else if t == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
            }
        }
        None
    }

    fn generic(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        self.seq(&items)
    }

    // ----- rules with an opinion ---------------------------------------------

    pub(crate) fn lower(&self, id: NodeId) -> Doc {
        match self.rule(id) {
            "SimpleSelect"
            | "SelectFromClause"
            | "FromSelectClause"
            | "ResultModifiers"
            | "LimitOffsetClause"
            | "OffsetLimitClause"
            | "OffsetFetchClause"
            | "SelectSetOpChain"
            | "SelectSetOpChainTail"
            | "IntersectChain"
            | "IntersectChainTail"
            // A window spec inside `OVER (...)` stays inline until it does
            // not fit; then `PARTITION BY`, `ORDER BY` and the frame each
            // take a line.
            | "WindowFrameContents" => self.children_on_lines(id),
            "SelectStatementInternal" => self.select_internal(id),
            "SelectClause" => self.select_clause(id),
            "GroupByClause"
            | "OrderByClause"
            | "WindowClause"
            | "ValuesClause"
            | "ReturningClause"
            | "PivotUsing"
            | "DeleteUsingClause"
            | "PivotOn"
            | "UpdateSetElementList"
            | "InsertColumnList" => self.keyword_list_clause(id),
            "WhereClause" | "HavingClause" | "QualifyClause" | "OnClause" => {
                self.keyword_expr_clause(id)
            }
            "LogicalAndExpression" | "LogicalOrExpression" => self.chain(id),
            // Operator chains break only when they are chains: two or more
            // tails. A lone `a > 1` never splits across lines.
            "OtherOperatorExpression" | "AdditiveExpression" | "MultiplicativeExpression"
                if self.items(id).len() >= 3 =>
            {
                self.chain(id)
            }
            // In a comma list of table references each reference keeps its
            // joins on its own line until they do not fit; at clause level
            // (`from` below) the joins are laid out by the statement.
            "TableRef" => Doc::group(self.generic(id)),
            "CaseExpression" => self.case(id),
            "WithClause" => self.with(id),
            "CTESelectBody" | "CTEDMLBody" => self.cte_body(id),
            "FromClause" => self.from(id),
            "RegularJoinClause" | "JoinByClause" | "NearestJoinAliased" | "NearestJoinBare" => {
                self.join(id)
            }
            "UpdateStatement" => self.update(id),
            _ => self.generic(id),
        }
    }

    /// Children joined by `Line`; direct tokens (rare here) by a space.
    fn children_on_lines(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut prev: Option<Item> = None;
        for it in items {
            if let Some(p) = prev {
                let both_nodes = matches!(p, Item::Node(_)) && matches!(it, Item::Node(_));
                parts.push(if both_nodes {
                    Doc::Line
                } else {
                    self.sep(p, it)
                });
            }
            parts.push(self.lower_item(it));
            prev = Some(it);
        }
        Doc::concat(parts)
    }

    /// `WITH ... <blank line> SELECT ... ORDER BY ...`. The blank line appears
    /// only when the statement is broken, separating the CTEs from the main
    /// query; a statement that fits stays on one line.
    fn select_internal(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut prev: Option<Item> = None;
        for it in items {
            if let Some(p) = prev {
                parts.push(Doc::Line);
                if let Item::Node(n) = p {
                    if self.rule(n) == "WithClause" {
                        parts.push(Doc::if_break(Doc::HardLine, Doc::Nil));
                    }
                }
            }
            parts.push(self.lower_item(it));
            prev = Some(it);
        }
        Doc::concat(parts)
    }

    /// Leading keyword tokens, then the rest in an indented group that
    /// breaks one item per line: `SELECT`, `GROUP BY`, `ORDER BY`, `VALUES`.
    fn keyword_list_clause(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut head = Vec::new();
        let mut k = 0;
        while k < items.len() {
            match items[k] {
                Item::Tok(i) if !matches!(self.text(self.tok(i)), "(" | "[" | "{" | ",") => {
                    if k > 0 {
                        head.push(Doc::text(" "));
                    }
                    head.push(self.tok_doc(i));
                    k += 1;
                }
                _ => break,
            }
        }
        if k == items.len() {
            return Doc::concat(head);
        }
        let body = self.seq(&items[k..]);
        if head.is_empty() {
            return body;
        }
        head.push(Doc::group(Doc::indent(Doc::concat(vec![Doc::Line, body]))));
        Doc::concat(head)
    }

    /// `SELECT [DISTINCT ...] <list>`: the distinct clause stays on the
    /// keyword's line, the target list gets the indented group.
    fn select_clause(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut head = Vec::new();
        let mut rest = Vec::new();
        for it in items {
            let is_list = matches!(it, Item::Node(n) if self.rule(n) == "TargetList");
            if is_list || !rest.is_empty() {
                rest.push(it);
            } else {
                if !head.is_empty() {
                    head.push(Doc::text(" "));
                }
                head.push(self.lower_item(it));
            }
        }
        if rest.is_empty() {
            return Doc::concat(head);
        }
        let body = self.seq(&rest);
        head.push(Doc::group(Doc::indent(Doc::concat(vec![Doc::Line, body]))));
        Doc::concat(head)
    }

    /// `WHERE <expr>`: the keyword, a space, and the expression in its own
    /// group so an `AND` chain breaks under the keyword.
    fn keyword_expr_clause(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut k = 0;
        while k < items.len() {
            if let Item::Tok(i) = items[k] {
                if k > 0 {
                    parts.push(Doc::text(" "));
                }
                parts.push(self.tok_doc(i));
                k += 1;
            } else {
                break;
            }
        }
        if k < items.len() {
            parts.push(Doc::text(" "));
            parts.push(Doc::group(self.seq(&items[k..])));
        }
        Doc::concat(parts)
    }

    /// `a AND b AND c` → one condition per line, operator leading, once it
    /// no longer fits.
    fn chain(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        if items.len() < 2 {
            return self.generic(id);
        }
        let mut parts = vec![self.lower_item(items[0])];
        let mut tail = Vec::new();
        for &it in &items[1..] {
            // Each tail is `'AND' <operand>`; lay it out as `Line AND operand`.
            let Item::Node(n) = it else {
                tail.push(Doc::text(" "));
                tail.push(self.lower_item(it));
                continue;
            };
            tail.push(Doc::Line);
            tail.push(self.generic(n));
        }
        parts.push(Doc::indent(Doc::concat(tail)));
        Doc::group(Doc::concat(parts))
    }

    /// `CASE [x] WHEN ... THEN ... [ELSE ...] END`.
    fn case(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut arms = Vec::new();
        let mut end = Doc::Nil;
        for (k, &it) in items.iter().enumerate() {
            match it {
                Item::Tok(i) => {
                    let t = self.text(self.tok(i)).to_ascii_uppercase();
                    if t == "END" {
                        end = self.tok_doc(i);
                    } else {
                        parts.push(self.tok_doc(i));
                    }
                }
                Item::Node(n) => {
                    let r = self.rule(n);
                    if r == "CaseWhenThen" || r == "CaseElse" {
                        arms.push(Doc::Line);
                        arms.push(self.lower(n));
                    } else {
                        // The operand of a simple CASE.
                        if k > 0 {
                            parts.push(Doc::text(" "));
                        }
                        parts.push(self.lower(n));
                    }
                }
            }
        }
        parts.push(Doc::indent(Doc::concat(arms)));
        parts.push(Doc::Line);
        parts.push(end);
        Doc::group(Doc::concat(parts))
    }

    /// `WITH [RECURSIVE] a AS (...),\nb AS (...)` — one CTE per line at
    /// clause level once the statement is broken.
    fn with(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut seen_cte = false;
        let mut prev: Option<Item> = None;
        for it in items {
            let is_cte = matches!(it, Item::Node(n) if self.rule(n) == "WithStatement");
            if self.is_tok(it, ",") {
                let Item::Tok(i) = it else { unreachable!() };
                parts.push(self.tok_doc(i));
                prev = Some(it);
                continue;
            }
            if let Some(p) = prev {
                if is_cte && seen_cte {
                    parts.push(Doc::Line);
                } else {
                    parts.push(self.sep(p, it));
                }
            }
            if is_cte {
                seen_cte = true;
            }
            parts.push(self.lower_item(it));
            prev = Some(it);
        }
        Doc::concat(parts)
    }

    /// A CTE body is `(` ... `)` whose lines belong to the *statement's*
    /// group rather than to a group of their own: when the `WITH` statement
    /// is broken, every CTE body is broken with it, so a query never reads
    /// as a row of one-line CTEs followed by a multi-line main query.
    fn cte_body(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let (Some(Item::Tok(open)), Some(Item::Tok(close))) = (items.first(), items.last()) else {
            return self.generic(id);
        };
        let inner = self.seq(&items[1..items.len() - 1]);
        Doc::concat(vec![
            self.tok_doc(*open),
            Doc::indent(Doc::concat(vec![Doc::SoftLine, inner])),
            Doc::SoftLine,
            self.tok_doc(*close),
        ])
    }

    /// `FROM t JOIN u ON ...`: a single table reference keeps its joins at
    /// clause level; a comma list of references gets the list treatment.
    fn from(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let refs: Vec<NodeId> = items
            .iter()
            .filter_map(|it| match it {
                Item::Node(n) if self.rule(*n) == "TableRef" => Some(*n),
                _ => None,
            })
            .collect();
        if refs.len() != 1 {
            return self.keyword_list_clause(id);
        }
        let mut parts = Vec::new();
        for it in &items {
            match it {
                Item::Tok(i) => parts.push(self.tok_doc(*i)),
                Item::Node(n) => {
                    parts.push(Doc::text(" "));
                    parts.push(self.generic(*n));
                }
            }
        }
        Doc::concat(parts)
    }

    /// `[ASOF] [LEFT] JOIN t <qualifier>`: the `ON`/`USING` trails on the
    /// same line until it no longer fits, then indents under its join.
    fn join(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut prev: Option<Item> = None;
        for it in items {
            let qualifier = matches!(it, Item::Node(n) if self.rule(n) == "JoinQualifier");
            if qualifier {
                let Item::Node(n) = it else { unreachable!() };
                parts.push(Doc::group(Doc::indent(Doc::concat(vec![
                    Doc::Line,
                    self.lower(n),
                ]))));
            } else {
                if let Some(p) = prev {
                    parts.push(self.sep(p, it));
                }
                parts.push(self.lower_item(it));
            }
            prev = Some(it);
        }
        Doc::concat(parts)
    }

    /// `UPDATE t SET a = 1, b = 2 FROM ... WHERE ...`: `SET` starts a line
    /// and owns the assignment list.
    fn update(&self, id: NodeId) -> Doc {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut prev: Option<Item> = None;
        for it in items {
            match it {
                Item::Node(n) if self.rule(n) == "UpdateTarget" => {
                    if prev.is_some() {
                        parts.push(Doc::text(" "));
                    }
                    // `<table> [alias] SET` — put SET on its own line.
                    let inner = self.items(n);
                    let target = self.first_child_items(&inner);
                    let mut tparts = Vec::new();
                    let mut tprev: Option<Item> = None;
                    for &ti in &target {
                        if self.is_tok(ti, "SET") || self.item_text(ti).eq_ignore_ascii_case("set")
                        {
                            tparts.push(Doc::Line);
                        } else if let Some(p) = tprev {
                            tparts.push(self.sep(p, ti));
                        }
                        tparts.push(self.lower_item(ti));
                        tprev = Some(ti);
                    }
                    parts.push(Doc::concat(tparts));
                }
                Item::Node(n) if self.rule(n) == "UpdateSetClause" => {
                    parts.push(Doc::group(Doc::indent(Doc::concat(vec![
                        Doc::Line,
                        self.generic(n),
                    ]))));
                }
                _ => {
                    if let Some(p) = prev {
                        parts.push(self.sep(p, it));
                    }
                    parts.push(self.lower_item(it));
                }
            }
            prev = Some(it);
        }
        Doc::concat(parts)
    }

    /// Flatten one level: `UpdateTarget` wraps `BaseTableSet` or
    /// `BaseTableAliasSet`, whose items are what the layout wants.
    fn first_child_items(&self, items: &[Item]) -> Vec<Item> {
        if let [Item::Node(n)] = items {
            return self.items(*n);
        }
        items.to_vec()
    }

    // ----- the file --------------------------------------------------------

    /// The whole program: statements separated by one blank line, each
    /// terminated by `;`, file-trailing comments at the end. Blank lines in
    /// the input are not preserved; vertical space is the layout's to decide.
    ///
    /// An empty statement (a bare `;`) is dropped, and the comments it
    /// carried are laid out exactly as a second formatting pass will see them: as
    /// leading comments of the statement that follows, or as file-trailing
    /// comments if nothing does. Anything else is not a fixed point.
    pub(crate) fn program(&self) -> Doc {
        let root = self.tree.root();
        let mut stmts: Vec<Doc> = Vec::new();
        let mut pending: Vec<Doc> = Vec::new();
        for &tls in self.tree.children(root) {
            let TopLevel { stmt, dropped } = self.top_level(tls);
            if let Some(d) = stmt {
                if pending.is_empty() {
                    stmts.push(d);
                } else {
                    pending.push(d);
                    stmts.push(Doc::concat(std::mem::take(&mut pending)));
                }
            }
            pending.extend(dropped);
        }
        let mut parts = Vec::new();
        for (k, d) in stmts.into_iter().enumerate() {
            if k > 0 {
                parts.push(Doc::HardLine);
                parts.push(Doc::HardLine);
            }
            parts.push(d);
        }
        // Comments after the last statement: from a trailing bare `;`, and
        // from the end of the file. One per line, no blank line before.
        if !pending.is_empty() {
            parts.push(Doc::concat(pending));
        }
        for c in &self.file_trailing {
            parts.push(Doc::OwnLine);
            parts.push(Doc::text(c.text.clone()));
        }
        Doc::concat(parts)
    }

    fn top_level(&self, tls: NodeId) -> TopLevel {
        let items = self.items(tls);
        let stmt = items.iter().find_map(|it| match it {
            Item::Node(n) => Some(*n),
            Item::Tok(_) => None,
        });
        let semis: Vec<usize> = items
            .iter()
            .filter_map(|it| match it {
                Item::Tok(i) if self.text(self.tok(*i)) == ";" => Some(*i),
                _ => None,
            })
            .collect();
        let mut out = TopLevel {
            stmt: None,
            dropped: Vec::new(),
        };
        let mut kept_semi = 0;
        if let Some(s) = stmt {
            self.stmt_end
                .set(self.by_end.get(&self.tree.node(s).span.end).copied());
            let mut parts = vec![Doc::group(self.lower(s))];
            self.stmt_end.set(None);
            match semis.first() {
                Some(&i) => {
                    parts.push(self.tok_doc(i));
                    kept_semi = 1;
                }
                None => parts.push(Doc::text(";")),
            }
            out.stmt = Some(Doc::concat(parts));
        }
        for &i in semis.iter().skip(kept_semi) {
            let d = self.dropped_token_comments(i);
            if d != Doc::Concat(Vec::new()) {
                out.dropped.push(d);
            }
        }
        out
    }

    /// The comments of a token the layout drops (a bare `;`), every one of
    /// them on its own line: with the token gone there is nothing left for
    /// a trailing comment to trail.
    fn dropped_token_comments(&self, i: usize) -> Doc {
        let mut parts = Vec::new();
        let all = self
            .leading
            .get(&i)
            .into_iter()
            .chain(self.trailing.get(&i))
            .flatten();
        for c in all {
            parts.push(Doc::OwnLine);
            parts.push(Doc::text(c.text.clone()));
            if c.line {
                parts.push(Doc::OwnLine);
            } else {
                parts.push(Doc::text(" "));
            }
        }
        Doc::concat(parts)
    }
}

fn matching_close(open: &str) -> Option<&'static str> {
    match open {
        "(" => Some(")"),
        "[" => Some("]"),
        "{" => Some("}"),
        _ => None,
    }
}
