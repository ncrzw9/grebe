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

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::OnceLock;

use grebe_syntax::grammar::{EXPRESSIONS, Expr, RULES};
use grebe_syntax::keyword::Keyword;
use grebe_syntax::matcher::{FlatNode, Statements, Step};
use grebe_syntax::token::{Token, TokenKind};

use crate::doc::{Doc, Printer};
use crate::{KeywordCase, Options, Semicolon};

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
    "SelectSampleClause",
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

/// The [`TIGHT_PAREN_RULES`] that are calls: a name, or a keyword that reads
/// as one (`CAST`, `COALESCE`, `ROLLUP`), and its arguments. These are the
/// parentheses `function_paren_space` sets off; the rest of the tight ones
/// (type modifiers, `OPERATOR(...)`) never are.
const CALL_PAREN_RULES: &[&str] = &[
    "FunctionExpressionArguments",
    "TableFunctionArguments",
    "MethodExpressionArguments",
    "MacroDefinition",
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
/// [`rule_table`] cannot see them; they have to be named here.
const KEYWORD_CATEGORY_RULES: &[&str] = &[
    "ReservedKeyword",
    "UnreservedKeyword",
    "ColumnNameKeyword",
    "FuncNameKeyword",
    "TypeNameKeyword",
    "BareLabelKeyword",
];

/// How a rule's node is laid out; see [`Lowerer::lower`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Generic,
    ChildrenOnLines,
    SelectInternal,
    SelectClause,
    KeywordList,
    KeywordExpr,
    /// `AND`/`OR`: breaks one operand per line.
    Chain,
    /// Operator chains break only when they are chains: two or more tails.
    /// A lone `a > 1` never splits across lines.
    LongChain,
    TableRef,
    Case,
    With,
    CteBody,
    From,
    Join,
    Update,
}

const CLAUSE: u32 = 1;
const STATEMENT: u32 = 1 << 1;
const TIGHT_PAREN: u32 = 1 << 2;
const TIGHT_BRACKET: u32 = 1 << 3;
const LOWER_NAME: u32 = 1 << 4;
/// Matches words as identifiers.
const IDENT: u32 = 1 << 5;
const TYPE: u32 = 1 << 6;
const SLICE: u32 = 1 << 7;
const PREFIX_OPERATOR: u32 = 1 << 8;
const COLUMN_LIST: u32 = 1 << 9;
const WITH_STATEMENT: u32 = 1 << 10;
const PARENTHESIS_EXPRESSION: u32 = 1 << 11;
const POSTFIX_OPERATOR: u32 = 1 << 12;
const QUESTION_MARK_PARAMETER: u32 = 1 << 13;
const PERCENT: u32 = 1 << 14;
const TARGET_LIST: u32 = 1 << 15;
const CASE_ARM: u32 = 1 << 16;
const WITH_CLAUSE: u32 = 1 << 17;
const TABLE_REF: u32 = 1 << 18;
const JOIN_QUALIFIER: u32 = 1 << 19;
const UPDATE_TARGET: u32 = 1 << 20;
const UPDATE_SET_LIST: u32 = 1 << 21;
const WHOLE_STATEMENT: u32 = 1 << 22;
const CALL_PAREN: u32 = 1 << 23;

/// The rule lists above and the rules [`Lowerer::lower`] has an opinion on,
/// as bits.
const RULE_FLAGS: &[(u32, &[&str])] = &[
    (CLAUSE, CLAUSE_RULES),
    (STATEMENT, STATEMENT_RULES),
    (TIGHT_PAREN, TIGHT_PAREN_RULES),
    (CALL_PAREN, CALL_PAREN_RULES),
    (TIGHT_BRACKET, TIGHT_BRACKET_RULES),
    (LOWER_NAME, LOWER_NAME_RULES),
    (IDENT, KEYWORD_CATEGORY_RULES),
    (TYPE, &["Type"]),
    (
        SLICE,
        &[
            "SliceExpression",
            "SliceBound",
            "EndSliceBound",
            "StepSliceBound",
            "MinusStepSliceBound",
        ],
    ),
    (
        PREFIX_OPERATOR,
        &[
            "MinusPrefixOperator",
            "PlusPrefixOperator",
            "TildePrefixOperator",
        ],
    ),
    (COLUMN_LIST, &["InsertColumnList", "ColumnAliases"]),
    (WITH_STATEMENT, &["WithStatement"]),
    (PARENTHESIS_EXPRESSION, &["ParenthesisExpression"]),
    (POSTFIX_OPERATOR, &["PostfixOperator"]),
    (QUESTION_MARK_PARAMETER, &["QuestionMarkNumberedParameter"]),
    (PERCENT, &["LimitExpression", "SamplePercentage"]),
    (TARGET_LIST, &["TargetList"]),
    (CASE_ARM, &["CaseWhenThen", "CaseElse"]),
    (WITH_CLAUSE, &["WithClause"]),
    (TABLE_REF, &["TableRef"]),
    (JOIN_QUALIFIER, &["JoinQualifier"]),
    (UPDATE_TARGET, &["UpdateTarget"]),
    (UPDATE_SET_LIST, &["UpdateSetClauseList"]),
    (WHOLE_STATEMENT, &["Statement"]),
];

fn layout_of(rule: &str) -> Layout {
    match rule {
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
        | "WindowFrameContents" => Layout::ChildrenOnLines,
        "SelectStatementInternal" => Layout::SelectInternal,
        "SelectClause" => Layout::SelectClause,
        "GroupByClause"
        | "OrderByClause"
        | "WindowClause"
        | "ValuesClause"
        | "ReturningClause"
        | "PivotUsing"
        | "DeleteUsingClause"
        | "PivotOn"
        | "UpdateSetClauseList"
        | "InsertColumnList" => Layout::KeywordList,
        "WhereClause" | "HavingClause" | "QualifyClause" | "OnClause" => Layout::KeywordExpr,
        "LogicalAndExpression" | "LogicalOrExpression" => Layout::Chain,
        "InfixOtherOperatorExpression" | "AdditiveExpression" | "MultiplicativeExpression" => {
            Layout::LongChain
        }
        // In a comma list of table references each reference keeps its
        // joins on its own line until they do not fit; at clause level
        // (`from` below) the joins are laid out by the statement.
        "TableRef" => Layout::TableRef,
        "CaseExpression" => Layout::Case,
        "WithClause" => Layout::With,
        "CTESelectBody" | "CTEDMLBody" => Layout::CteBody,
        "FromClause" => Layout::From,
        "RegularJoinClause" | "JoinByClause" | "NearestJoinAliased" | "NearestJoinBare" => {
            Layout::Join
        }
        "UpdateStatement" => Layout::Update,
        _ => Layout::Generic,
    }
}

#[derive(Clone, Copy)]
struct RuleInfo {
    layout: Layout,
    flags: u32,
}

/// What the formatter knows about each grammar rule, by rule id. Worked out
/// once per process: a node is asked what it is several times over, and
/// comparing rule names each time was a fifth of lowering.
fn rule_table() -> &'static [RuleInfo] {
    static TABLE: OnceLock<Vec<RuleInfo>> = OnceLock::new();
    TABLE.get_or_init(|| {
        RULES
            .iter()
            .map(|r| {
                let mut flags = 0;
                for (flag, names) in RULE_FLAGS {
                    if names.contains(&r.name) {
                        flags |= flag;
                    }
                }
                let identifier = match &EXPRESSIONS[r.expr.0 as usize] {
                    Expr::Identifier(_) => true,
                    Expr::Choice(arms) => arms
                        .iter()
                        .any(|a| matches!(EXPRESSIONS[a.0 as usize], Expr::Identifier(_))),
                    _ => false,
                };
                if identifier {
                    flags |= IDENT;
                }
                RuleInfo {
                    layout: layout_of(r.name),
                    flags,
                }
            })
            .collect()
    })
}

/// A code token, by its index among the file's code tokens, or a node, by
/// its index among the statement's nodes.
#[derive(Clone, Copy)]
enum Item {
    Tok(usize),
    Node(usize),
}

#[derive(Clone, Copy)]
struct Comment<'a> {
    text: &'a str,
    line: bool,
}

/// Comment attachment, kept up one statement at a time.
///
/// A comment on the same line as the code before it trails that code; any
/// other comment leads the next code token. Comments after the last code
/// token, on later lines, belong to the file.
///
/// The scan runs over the file's tokens once, in step with the statements:
/// for each statement it goes on through the trivia after the statement's
/// last token, which is where a comment trailing the `;` is, and what it
/// has not placed by then leads the next statement.
struct Comments<'a> {
    /// By code token, for the tokens of the statement being lowered.
    leading: HashMap<usize, Vec<Comment<'a>>>,
    trailing: HashMap<usize, Vec<Comment<'a>>>,
    /// The next token to look at, among all tokens and among code tokens.
    cursor: usize,
    next_code: usize,
    last_code: Option<usize>,
    newline_since: bool,
    pending: Vec<Comment<'a>>,
}

impl<'a> Comments<'a> {
    fn new() -> Self {
        Self {
            leading: HashMap::new(),
            trailing: HashMap::new(),
            cursor: 0,
            next_code: 0,
            last_code: None,
            newline_since: true,
            pending: Vec::new(),
        }
    }

    /// Attach the comments of the statement whose code tokens end before
    /// `end`, forgetting the previous statement's.
    fn attach(&mut self, src: &'a str, tokens: &[Token], end: usize) {
        self.leading.clear();
        self.trailing.clear();
        while let Some(t) = tokens.get(self.cursor) {
            let text = &src[t.span.start as usize..t.span.end as usize];
            match t.kind {
                TokenKind::Whitespace => {
                    if text.contains('\n') {
                        self.newline_since = true;
                    }
                }
                TokenKind::LineComment | TokenKind::BlockComment => {
                    let c = Comment {
                        text: text.trim_end(),
                        line: t.kind == TokenKind::LineComment,
                    };
                    match self.last_code {
                        Some(k) if !self.newline_since => {
                            self.trailing.entry(k).or_default().push(c);
                        }
                        _ => self.pending.push(c),
                    }
                }
                _ => {
                    let i = self.next_code;
                    if i >= end {
                        break; // the next statement's first token
                    }
                    // `x -- c` then `, y`: the comma ends x's line, and the
                    // comment goes after it -- where the next run will find
                    // it. Left trailing `x`, its line break would split up
                    // whatever `x` closes (`[1, 2, 3]`, a CASE) on this run
                    // and not on the next.
                    if text == "," {
                        if let Some(k) = self.last_code {
                            if let Some(cs) = self.trailing.get_mut(&k) {
                                let (line, block): (Vec<_>, Vec<_>) =
                                    std::mem::take(cs).into_iter().partition(|c| c.line);
                                *cs = block;
                                if !line.is_empty() {
                                    self.trailing.entry(i).or_default().extend(line);
                                }
                            }
                        }
                    }
                    if !self.pending.is_empty() {
                        self.leading.insert(i, std::mem::take(&mut self.pending));
                    }
                    self.last_code = Some(i);
                    self.newline_since = false;
                    self.next_code += 1;
                }
            }
            self.cursor += 1;
        }
    }

    /// The comments no statement took: those after the last code token on
    /// later lines, or all of them in a file with no code.
    fn file_trailing(mut self, src: &'a str, tokens: &[Token]) -> Vec<Comment<'a>> {
        self.attach(src, tokens, usize::MAX);
        self.pending
    }
}

/// One `TopLevelStatement`: its statement, if it has one, and the comments
/// carried by any `;` the layout drops (a bare one, or the extras in `;;`).
/// Those comments belong to whatever comes next, which is where re-formatting
/// the output will find them.
struct TopLevel<'a> {
    stmt: Option<Doc<'a>>,
    dropped: Vec<Doc<'a>>,
}

/// The per-statement tables, kept from one statement to the next so a file
/// of small statements does not allocate them each time.
#[derive(Default)]
struct Tables {
    owner: Vec<u32>,
    parent: Vec<u32>,
    in_type: Vec<bool>,
}

const NO_NODE: u32 = u32::MAX;

const SPACE: Doc<'static> = Doc::Text(Cow::Borrowed(" "));

/// Lowers one top-level statement.
///
/// The statement's tree is the matcher's own: nodes in post-order, each
/// covering a range of code tokens, a node's descendants the `size` nodes
/// before it. Nothing is linked. A node's children are found by stepping
/// back over their subtrees, and the tokens between them are its own.
struct Lowerer<'s, 'a> {
    nodes: &'s [FlatNode],
    /// The file's code tokens; a node's range indexes these.
    code: &'s [(Token, usize)],
    src: &'a str,
    opts: &'s Options,
    rules: &'static [RuleInfo],
    /// For the keyword categories of words, which the matcher has worked
    /// out already.
    keywords: &'s Statements<'a>,
    comments: &'s Comments<'a>,
    /// The statement's first code token; `owner` starts there.
    base: usize,
    /// Code token → the node that owns it directly.
    owner: Vec<u32>,
    parent: Vec<u32>,
    /// Node → sits under a `Type` node.
    in_type: Vec<bool>,
    /// The last code token of the statement being lowered. A list's trailing
    /// comma there would print as `,;`, so it is dropped even when the list
    /// is broken.
    stmt_end: Cell<Option<usize>>,
}

impl<'s, 'a> Lowerer<'s, 'a> {
    fn new(
        parser: &'s Statements<'a>,
        src: &'a str,
        opts: &'s Options,
        comments: &'s Comments<'a>,
        tables: Tables,
    ) -> Self {
        let nodes = parser.nodes();
        let rules = rule_table();
        let Tables {
            mut owner,
            mut parent,
            mut in_type,
        } = tables;
        let (base, end) = nodes
            .last()
            .map_or((0, 0), |root| (root.start as usize, root.end as usize));
        owner.clear();
        owner.resize(end - base, NO_NODE);
        parent.clear();
        parent.resize(nodes.len(), NO_NODE);
        // Children come before their parent, so by the time a node is
        // reached its children have claimed their tokens and the rest of
        // its range is its own.
        for (i, node) in nodes.iter().enumerate() {
            let first = i - node.size as usize;
            let mut hi = node.end as usize;
            let mut c = i;
            while c > first {
                c -= 1;
                let child = nodes[c];
                parent[c] = i as u32;
                if child.start < child.end {
                    for slot in &mut owner[child.end as usize - base..hi - base] {
                        *slot = i as u32;
                    }
                    hi = child.start as usize;
                }
                c -= child.size as usize;
            }
            if (node.start as usize) < hi {
                for slot in &mut owner[node.start as usize - base..hi - base] {
                    *slot = i as u32;
                }
            }
        }
        in_type.clear();
        in_type.resize(nodes.len(), false);
        for i in (0..nodes.len()).rev() {
            let up = parent[i];
            in_type[i] = rules[nodes[i].rule.0 as usize].flags & TYPE != 0
                || (up != NO_NODE && in_type[up as usize]);
        }
        Self {
            nodes,
            code: parser.code_tokens(),
            src,
            opts,
            rules,
            keywords: parser,
            comments,
            base,
            owner,
            parent,
            in_type,
            stmt_end: Cell::new(None),
        }
    }

    fn into_tables(self) -> Tables {
        Tables {
            owner: self.owner,
            parent: self.parent,
            in_type: self.in_type,
        }
    }

    fn tok(&self, i: usize) -> &Token {
        &self.code[i].0
    }

    fn text(&self, i: usize) -> &'a str {
        let span = self.code[i].0.span;
        &self.src[span.start as usize..span.end as usize]
    }

    fn info(&self, n: usize) -> RuleInfo {
        self.rules[self.nodes[n].rule.0 as usize]
    }

    fn is(&self, n: usize, flag: u32) -> bool {
        self.info(n).flags & flag != 0
    }

    /// Is the node that owns this token one of the flagged rules?
    fn owner_is(&self, i: usize, flag: u32) -> bool {
        self.is(self.owner[i - self.base] as usize, flag)
    }

    fn parent(&self, n: usize) -> Option<usize> {
        match self.parent[n] {
            NO_NODE => None,
            p => Some(p as usize),
        }
    }

    // ----- items -----------------------------------------------------------

    /// A node's direct code tokens and non-empty children, in source order.
    fn items(&self, id: usize) -> Vec<Item> {
        let node = self.nodes[id];
        let first = id - node.size as usize;
        // Collected from the back, which is the way the children are found.
        let mut out = Vec::new();
        let mut hi = node.end as usize;
        let mut c = id;
        while c > first {
            c -= 1;
            let child = self.nodes[c];
            if child.start < child.end {
                out.extend((child.end as usize..hi).rev().map(Item::Tok));
                out.push(Item::Node(c));
                hi = child.start as usize;
            }
            c -= child.size as usize;
        }
        out.extend((node.start as usize..hi).rev().map(Item::Tok));
        out.reverse();
        out
    }

    /// The node's only item, when that is a node covering all of it: a unit
    /// production, of which an expression has one per precedence level.
    fn only_child(&self, id: usize) -> Option<usize> {
        let node = self.nodes[id];
        if node.size == 0 || node.start == node.end {
            return None;
        }
        let last = self.nodes[id - 1];
        (last.start == node.start && last.end == node.end).then_some(id - 1)
    }

    fn first_tok(&self, it: Item) -> usize {
        match it {
            Item::Tok(i) => i,
            Item::Node(n) => self.nodes[n].start as usize,
        }
    }

    fn last_tok(&self, it: Item) -> usize {
        match it {
            Item::Tok(i) => i,
            Item::Node(n) => self.nodes[n].end as usize - 1,
        }
    }

    fn item_text(&self, it: Item) -> &'a str {
        match it {
            Item::Tok(i) => self.text(i),
            Item::Node(_) => {
                let start = self.tok(self.first_tok(it)).span.start;
                let end = self.tok(self.last_tok(it)).span.end;
                &self.src[start as usize..end as usize]
            }
        }
    }

    fn is_tok(&self, it: Item, s: &str) -> bool {
        matches!(it, Item::Tok(_)) && self.item_text(it) == s
    }

    // ----- tokens ----------------------------------------------------------

    /// Do any of the nodes whose extent is exactly this token's -- the
    /// chain from its owner up -- belong to a flagged rule?
    fn exact_cover_is(&self, i: usize, flag: u32) -> bool {
        let mut cur = Some(self.owner[i - self.base] as usize);
        while let Some(n) = cur {
            let node = self.nodes[n];
            if node.start as usize != i || node.end as usize != i + 1 {
                break;
            }
            if self.is(n, flag) {
                return true;
            }
            cur = self.parent(n);
        }
        false
    }

    /// Is this token a keyword, by how the grammar matched it? A word the
    /// grammar took as an identifier is not, whatever it spells.
    fn is_keyword(&self, i: usize) -> bool {
        self.tok(i).kind == TokenKind::Word
            && self.keywords.keyword(i) != Keyword::NONE
            && !self.exact_cover_is(i, IDENT)
    }

    fn cased(&self, i: usize) -> Cow<'a, str> {
        let raw = self.text(i);
        if self.tok(i).kind != TokenKind::Word {
            return Cow::Borrowed(raw);
        }
        // Is this word an identifier, by how the grammar matched it?
        if self.exact_cover_is(i, IDENT) {
            if self.exact_cover_is(i, LOWER_NAME) {
                return lower_case(raw);
            }
            return Cow::Borrowed(raw);
        }
        if self.keywords.keyword(i) == Keyword::NONE {
            return Cow::Borrowed(raw);
        }
        // Type names are lower even when the grammar spells them as keyword
        // literals (`integer`, `timestamp with time zone`).
        if self.in_type[self.owner[i - self.base] as usize] {
            return lower_case(raw);
        }
        match self.opts.keyword_case {
            KeywordCase::Upper => upper_case(raw),
            KeywordCase::Lower => lower_case(raw),
        }
    }

    fn comments_leading(&self, i: usize, parts: &mut Vec<Doc<'a>>) {
        if let Some(cs) = self.comments.leading.get(&i) {
            for c in cs {
                if c.line {
                    parts.push(Doc::OwnLine);
                    parts.push(Doc::text(c.text));
                    parts.push(Doc::OwnLine);
                } else {
                    parts.push(Doc::text(c.text));
                    parts.push(Doc::text(" "));
                }
            }
        }
    }

    fn comments_trailing(&self, i: usize, parts: &mut Vec<Doc<'a>>) {
        if let Some(cs) = self.comments.trailing.get(&i) {
            for c in cs {
                if c.line && self.stmt_end.get() == Some(i) {
                    // After the statement's last token: it goes after the
                    // `;`, which is where the next run finds it, so it must
                    // not break the statement's layout either -- the next
                    // run would see a comment trailing `;` and keep it flat.
                    parts.push(Doc::LineSuffix(format!(" {}", c.text)));
                } else if c.line {
                    parts.push(Doc::LineSuffix(format!(" {}", c.text)));
                    parts.push(Doc::BreakParent);
                } else {
                    parts.push(Doc::text(format!(" {}", c.text)));
                }
            }
        }
    }

    /// A token with its comments, with the given text in place of its own.
    fn tok_doc_as(&self, i: usize, text: Doc<'a>) -> Doc<'a> {
        // Nearly every token has no comment, and is just its text.
        if self.comments.leading.is_empty() && self.comments.trailing.is_empty() {
            return text;
        }
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

    fn tok_doc(&self, i: usize) -> Doc<'a> {
        self.tok_doc_as(i, Doc::Text(self.cased(i)))
    }

    // ----- spacing ---------------------------------------------------------

    /// What goes between two adjacent items.
    fn sep(&self, left: Item, right: Item) -> Doc<'a> {
        if let Item::Node(n) = right {
            if self.is(n, CLAUSE | STATEMENT) {
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
        let l = self.text(li);
        let r = self.text(ri);

        match r {
            "," | ";" | ")" | "]" | "}" | "." | "::" | ":" => return Doc::Nil,
            _ => {}
        }
        match l {
            "(" | "[" | "{" | "." | "::" | "$" | "#" => return Doc::Nil,
            ":" => {
                return if self.owner_is(li, SLICE) {
                    Doc::Nil
                } else {
                    Doc::text(" ")
                };
            }
            _ => {}
        }
        if r == "(" && self.owner_is(li, PREFIX_OPERATOR) {
            return Doc::Nil; // `-(a + b)`, like `-x`
        }
        if r == "(" {
            // `WITH r(n) AS (...)` hugs; `INSERT INTO t (a, b)` does not.
            let cte_columns = self.owner_is(ri, COLUMN_LIST)
                && self
                    .parent(self.owner[ri - self.base] as usize)
                    .is_some_and(|p| self.is(p, WITH_STATEMENT));
            if cte_columns {
                return Doc::Nil;
            }
            if self.owner_is(ri, CALL_PAREN) {
                return if self.opts.function_paren_space {
                    Doc::text(" ")
                } else {
                    Doc::Nil
                };
            }
            if self.owner_is(ri, TIGHT_PAREN) {
                return Doc::Nil;
            }
            // `IN (1, 2)`, `OVER (...)`, `AS (...)`: any other parenthesis
            // that follows a keyword.
            if !self.opts.keyword_paren_space && self.is_keyword(li) {
                return Doc::Nil;
            }
            return Doc::text(" ");
        }
        if r == "[" {
            return if self.owner_is(ri, TIGHT_BRACKET) {
                Doc::Nil
            } else {
                Doc::text(" ")
            };
        }
        if self.owner_is(li, PREFIX_OPERATOR) {
            // `-x` hugs, but `- -1` must not: `--1` starts a comment and
            // `~-1` is a different operator. DuckDB reads any run of these
            // characters as one operator token.
            return if r.starts_with(|c: char| "+-*/<>=~!@%^&|`#".contains(c)) {
                Doc::text(" ")
            } else {
                Doc::Nil
            };
        }
        if r == "!" && self.owner_is(ri, POSTFIX_OPERATOR) {
            return Doc::Nil;
        }
        if l == "?" && self.owner_is(li, QUESTION_MARK_PARAMETER) {
            return Doc::Nil;
        }
        if r == "%" && self.owner_is(ri, PERCENT) {
            return Doc::Nil;
        }
        Doc::text(" ")
    }

    /// Does `right` open with a parenthesis that hugs the keyword `left`
    /// ends with? Only with `keyword_paren_space` off. The layouts that put
    /// a space or a line after a clause keyword themselves, rather than
    /// through [`Self::sep`], ask this first.
    fn hugs_keyword(&self, left: Item, right: Item) -> bool {
        !self.opts.keyword_paren_space
            && self.text(self.first_tok(right)) == "("
            && self.is_keyword(self.last_tok(left))
    }

    /// The gap after a clause keyword: `flat` (a space, or a `Line` that
    /// is one until it breaks), or the same without the space when the
    /// keyword's parenthesis hugs it.
    fn after_keyword(&self, left: Option<Item>, right: Item, flat: Doc<'a>) -> Doc<'a> {
        match left {
            Some(left) if self.hugs_keyword(left, right) => match flat {
                Doc::Line => Doc::SoftLine,
                _ => Doc::Nil,
            },
            _ => flat,
        }
    }

    /// [`Self::sep`], left out when it is nothing: most of a `Doc` would
    /// otherwise be `Nil`s between tokens that touch.
    fn push_sep(&self, parts: &mut Vec<Doc<'a>>, left: Item, right: Item) {
        let sep = self.sep(left, right);
        if !matches!(sep, Doc::Nil) {
            parts.push(sep);
        }
    }

    // ----- generic ---------------------------------------------------------

    fn lower_item(&self, it: Item) -> Doc<'a> {
        match it {
            Item::Tok(i) => self.tok_doc(i),
            Item::Node(n) => self.lower(n),
        }
    }

    /// Lay out a run of items: brackets group and indent, commas break,
    /// everything else is joined by [`Self::sep`].
    fn seq(&self, items: &[Item]) -> Doc<'a> {
        let mut parts: Vec<Doc<'a>> = Vec::new();
        let mut prev: Option<Item> = None;
        let mut i = 0;
        while i < items.len() {
            let it = items[i];
            if let Some(p) = prev {
                // A comma already pushed its `Line`; nothing else goes before
                // one, either.
                if !self.is_tok(it, ",") && !self.is_tok(p, ",") {
                    self.push_sep(&mut parts, p, it);
                }
            }
            if let Item::Tok(ti) = it {
                let text = self.text(ti);
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
                        // `(42,)` is a one-element tuple and `(42)` is 42:
                        // the only comma of a parenthesised expression is
                        // not a trailing comma, it is what makes the tuple.
                        let makes_tuple = self.owner_is(ti, PARENTHESIS_EXPRESSION)
                            && items.iter().filter(|&&x| self.is_tok(x, ",")).count() == 1;
                        let comma = if at_statement_end {
                            Doc::Nil
                        } else if makes_tuple {
                            Doc::text(",")
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
        one_or_concat(parts)
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

    fn generic(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        self.seq(&items)
    }

    // ----- rules with an opinion ---------------------------------------------

    fn lower(&self, mut id: usize) -> Doc<'a> {
        // A node that is nothing but one child lays out as that child. An
        // expression is a dozen such nodes deep at every leaf; stepping
        // through them here, rather than lowering each to a `Doc` holding
        // the next, is most of what keeps a `Doc` near the size of its text.
        while matches!(
            self.info(id).layout,
            Layout::Generic | Layout::Chain | Layout::LongChain
        ) {
            match self.only_child(id) {
                Some(child) => id = child,
                None => break,
            }
        }
        match self.info(id).layout {
            Layout::ChildrenOnLines => self.children_on_lines(id),
            Layout::SelectInternal => self.select_internal(id),
            Layout::SelectClause => self.select_clause(id),
            Layout::KeywordList => self.keyword_list_clause(id),
            Layout::KeywordExpr => self.keyword_expr_clause(id),
            Layout::Chain => self.chain(self.items(id)),
            Layout::LongChain => {
                let items = self.items(id);
                if items.len() >= 3 {
                    self.chain(items)
                } else {
                    self.seq(&items)
                }
            }
            Layout::TableRef => Doc::group(self.generic(id)),
            Layout::Case => self.case(id),
            Layout::With => self.with(id),
            Layout::CteBody => self.cte_body(id),
            Layout::From => self.from(id),
            Layout::Join => self.join(id),
            Layout::Update => self.update(id),
            Layout::Generic => self.generic(id),
        }
    }

    /// Children joined by `Line`; direct tokens (rare here) by a space.
    fn children_on_lines(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut prev: Option<Item> = None;
        for it in items {
            if let Some(p) = prev {
                let both_nodes = matches!(p, Item::Node(_)) && matches!(it, Item::Node(_));
                if both_nodes {
                    parts.push(Doc::Line);
                } else {
                    self.push_sep(&mut parts, p, it);
                }
            }
            parts.push(self.lower_item(it));
            prev = Some(it);
        }
        one_or_concat(parts)
    }

    /// `WITH ... <blank line> SELECT ... ORDER BY ...`. The blank line appears
    /// only when the statement is broken, separating the CTEs from the main
    /// query; a statement that fits stays on one line.
    fn select_internal(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut prev: Option<Item> = None;
        for it in items {
            if let Some(p) = prev {
                parts.push(Doc::Line);
                if let Item::Node(n) = p {
                    if self.is(n, WITH_CLAUSE) {
                        parts.push(Doc::if_break(Doc::HardLine, Doc::Nil));
                    }
                }
            }
            parts.push(self.lower_item(it));
            prev = Some(it);
        }
        one_or_concat(parts)
    }

    /// Leading keyword tokens, then the rest in an indented group that
    /// breaks one item per line: `SELECT`, `GROUP BY`, `ORDER BY`, `VALUES`.
    fn keyword_list_clause(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let mut head = Vec::new();
        let mut k = 0;
        while k < items.len() {
            match items[k] {
                Item::Tok(i) if !matches!(self.text(i), "(" | "[" | "{" | ",") => {
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
        let gap = self.after_keyword(Some(items[k - 1]), items[k], Doc::Line);
        head.push(Doc::group(Doc::indent(Doc::concat(vec![gap, body]))));
        Doc::concat(head)
    }

    /// `SELECT [DISTINCT ...] <list>`: the distinct clause stays on the
    /// keyword's line, the target list gets the indented group.
    fn select_clause(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let mut head = Vec::new();
        let mut rest = Vec::new();
        let mut last_head: Option<Item> = None;
        for it in items {
            let is_list = matches!(it, Item::Node(n) if self.is(n, TARGET_LIST));
            if is_list || !rest.is_empty() {
                rest.push(it);
            } else {
                if !head.is_empty() {
                    head.push(Doc::text(" "));
                }
                head.push(self.lower_item(it));
                last_head = Some(it);
            }
        }
        if rest.is_empty() {
            return Doc::concat(head);
        }
        let body = self.seq(&rest);
        let gap = self.after_keyword(last_head, rest[0], Doc::Line);
        head.push(Doc::group(Doc::indent(Doc::concat(vec![gap, body]))));
        Doc::concat(head)
    }

    /// `WHERE <expr>`: the keyword, a space, and the expression in its own
    /// group so an `AND` chain breaks under the keyword.
    fn keyword_expr_clause(&self, id: usize) -> Doc<'a> {
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
            let gap = self.after_keyword(k.checked_sub(1).map(|p| items[p]), items[k], SPACE);
            if !matches!(gap, Doc::Nil) {
                parts.push(gap);
            }
            parts.push(Doc::group(self.seq(&items[k..])));
        }
        Doc::concat(parts)
    }

    /// `a AND b AND c` → one condition per line, operator leading, once it
    /// no longer fits.
    fn chain(&self, items: Vec<Item>) -> Doc<'a> {
        if items.len() < 2 {
            return self.seq(&items);
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
    fn case(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut arms = Vec::new();
        let mut end = Doc::Nil;
        for (k, &it) in items.iter().enumerate() {
            match it {
                Item::Tok(i) => {
                    if self.text(i).eq_ignore_ascii_case("END") {
                        end = self.tok_doc(i);
                    } else {
                        parts.push(self.tok_doc(i));
                    }
                }
                Item::Node(n) => {
                    if self.is(n, CASE_ARM) {
                        arms.push(Doc::Line);
                        arms.push(self.lower(n));
                    } else {
                        // The operand of a simple CASE.
                        if k > 0 && !self.hugs_keyword(items[k - 1], it) {
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
    fn with(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut seen_cte = false;
        let mut prev: Option<Item> = None;
        for it in items {
            let is_cte = matches!(it, Item::Node(n) if self.is(n, WITH_STATEMENT));
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
                    self.push_sep(&mut parts, p, it);
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
    fn cte_body(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let (Some(Item::Tok(open)), Some(Item::Tok(close))) = (items.first(), items.last()) else {
            return self.seq(&items);
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
    fn from(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let refs = items
            .iter()
            .filter(|it| matches!(it, Item::Node(n) if self.is(*n, TABLE_REF)))
            .count();
        if refs != 1 {
            return self.keyword_list_clause(id);
        }
        let mut parts = Vec::new();
        for (k, it) in items.iter().enumerate() {
            match it {
                Item::Tok(i) => parts.push(self.tok_doc(*i)),
                Item::Node(n) => {
                    if k == 0 || !self.hugs_keyword(items[k - 1], *it) {
                        parts.push(Doc::text(" "));
                    }
                    parts.push(self.generic(*n));
                }
            }
        }
        Doc::concat(parts)
    }

    /// `[ASOF] [LEFT] JOIN t <qualifier>`: the `ON`/`USING` trails on the
    /// same line until it no longer fits, then indents under its join.
    fn join(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut prev: Option<Item> = None;
        for it in items {
            match it {
                Item::Node(n) if self.is(n, JOIN_QUALIFIER) => {
                    parts.push(Doc::group(Doc::indent(Doc::concat(vec![
                        Doc::Line,
                        self.lower(n),
                    ]))));
                }
                _ => {
                    if let Some(p) = prev {
                        self.push_sep(&mut parts, p, it);
                    }
                    parts.push(self.lower_item(it));
                }
            }
            prev = Some(it);
        }
        Doc::concat(parts)
    }

    /// `UPDATE t SET a = 1, b = 2 FROM ... WHERE ...`: `SET` starts a line
    /// and owns the assignment list.
    fn update(&self, id: usize) -> Doc<'a> {
        let items = self.items(id);
        let mut parts = Vec::new();
        let mut prev: Option<Item> = None;
        for it in items {
            match it {
                Item::Node(n) if self.is(n, UPDATE_TARGET) => {
                    if prev.is_some() {
                        parts.push(Doc::text(" "));
                    }
                    // `<table> [alias] SET` — put SET on its own line.
                    let inner = self.items(n);
                    let target = self.first_child_items(inner);
                    let mut tparts = Vec::new();
                    let mut tprev: Option<Item> = None;
                    for &ti in &target {
                        if self.is_tok(ti, "SET") || self.item_text(ti).eq_ignore_ascii_case("set")
                        {
                            tparts.push(Doc::Line);
                        } else if let Some(p) = tprev {
                            self.push_sep(&mut tparts, p, ti);
                        }
                        tparts.push(self.lower_item(ti));
                        tprev = Some(ti);
                    }
                    parts.push(Doc::concat(tparts));
                }
                Item::Node(n) if self.is(n, UPDATE_SET_LIST) => {
                    parts.push(Doc::group(Doc::indent(Doc::concat(vec![
                        Doc::Line,
                        self.keyword_list_clause(n),
                    ]))));
                }
                _ => {
                    if let Some(p) = prev {
                        self.push_sep(&mut parts, p, it);
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
    fn first_child_items(&self, items: Vec<Item>) -> Vec<Item> {
        if let [Item::Node(n)] = items[..] {
            return self.items(n);
        }
        items
    }

    // ----- the statement ---------------------------------------------------

    /// Does the statement hold a `Statement`, or is it a bare `;`?
    fn has_statement(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| self.rules[n.rule.0 as usize].flags & WHOLE_STATEMENT != 0)
    }

    fn top_level(&self) -> TopLevel<'a> {
        let mut out = TopLevel {
            stmt: None,
            dropped: Vec::new(),
        };
        let Some(tls) = self.nodes.len().checked_sub(1) else {
            return out;
        };
        let items = self.items(tls);
        let stmt = items.iter().find_map(|it| match it {
            Item::Node(n) => Some(*n),
            Item::Tok(_) => None,
        });
        let semis: Vec<usize> = items
            .iter()
            .filter_map(|it| match it {
                Item::Tok(i) if self.text(*i) == ";" => Some(*i),
                _ => None,
            })
            .collect();
        let mut kept_semi = 0;
        if let Some(s) = stmt {
            self.stmt_end.set(Some(self.nodes[s].end as usize - 1));
            let mut parts = vec![Doc::group(self.lower(s))];
            self.stmt_end.set(None);
            let semi = semis.first().copied();
            // A `;` that comments lead already stands apart from the
            // statement, on the comment's line or the one after it.
            let led = semi.is_some_and(|i| self.comments.leading.contains_key(&i));
            let text = match self.opts.semicolon {
                Semicolon::Spaced if !led => " ;",
                Semicolon::Newline => {
                    parts.push(Doc::HardLine);
                    ";"
                }
                Semicolon::Attached | Semicolon::Spaced => ";",
            };
            match semi {
                Some(i) => {
                    parts.push(self.tok_doc_as(i, Doc::text(text)));
                    kept_semi = 1;
                }
                None => parts.push(Doc::text(text)),
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
    fn dropped_token_comments(&self, i: usize) -> Doc<'a> {
        let mut parts = Vec::new();
        let all = self
            .comments
            .leading
            .get(&i)
            .into_iter()
            .chain(self.comments.trailing.get(&i))
            .flatten();
        for c in all {
            parts.push(Doc::OwnLine);
            parts.push(Doc::text(c.text));
            if c.line {
                parts.push(Doc::OwnLine);
            } else {
                parts.push(Doc::text(" "));
            }
        }
        Doc::concat(parts)
    }
}

// ----- the file ------------------------------------------------------------

/// A file that parsed, formatted.
pub(crate) struct Formatted {
    /// As printed: no newline at the end.
    pub(crate) text: String,
    /// Whether the file holds a statement at all, rather than only comments
    /// and bare `;`.
    pub(crate) has_statement: bool,
}

/// The whole program: statements separated by one blank line, each
/// terminated by `;`, file-trailing comments at the end. Blank lines in
/// the input are not preserved; vertical space is the layout's to decide.
///
/// An empty statement (a bare `;`) is dropped, and the comments it
/// carried are laid out exactly as a second formatting pass will see them: as
/// leading comments of the statement that follows, or as file-trailing
/// comments if nothing does. Anything else is not a fixed point.
///
/// The file is parsed, lowered and printed one top-level statement at a
/// time, so what is held at once is the output and one statement's tree and
/// `Doc`, not the file's. `None` when a statement does not parse.
pub(crate) fn format_statements(src: &str, opts: &Options) -> Option<Formatted> {
    let mut parser = Statements::new(src);
    let mut printer = Printer::new(opts.inline_threshold, opts.indent_size);
    let mut comments = Comments::new();
    let mut tables = Tables::default();
    let mut pending: Vec<Doc<'_>> = Vec::new();
    let mut printed = false;
    let mut has_statement = false;
    loop {
        match parser.advance() {
            Step::End => break,
            Step::Rejected => return None,
            Step::Statement => {}
        }
        let end = parser.nodes().last().map_or(0, |root| root.end as usize);
        comments.attach(src, parser.tokens(), end);
        let lowerer = Lowerer::new(&parser, src, opts, &comments, std::mem::take(&mut tables));
        has_statement |= lowerer.has_statement();
        let TopLevel { stmt, dropped } = lowerer.top_level();
        tables = lowerer.into_tables();
        if let Some(doc) = stmt {
            if printed {
                printer.hard_line();
                printer.hard_line();
            }
            for d in pending.drain(..) {
                printer.print(&d);
            }
            printer.print(&doc);
            printed = true;
        }
        pending.extend(dropped);
    }
    // Comments after the last statement: from a trailing bare `;`, and
    // from the end of the file. One per line, no blank line before.
    for d in &pending {
        printer.print(d);
    }
    for c in comments.file_trailing(src, parser.tokens()) {
        printer.print(&Doc::OwnLine);
        printer.print(&Doc::text(c.text));
    }
    Some(Formatted {
        text: printer.finish(),
        has_statement,
    })
}

/// Lower-cased, borrowed when it already is.
fn lower_case(raw: &str) -> Cow<'_, str> {
    if raw.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(raw.to_ascii_lowercase())
    } else {
        Cow::Borrowed(raw)
    }
}

fn upper_case(raw: &str) -> Cow<'_, str> {
    if raw.bytes().any(|b| b.is_ascii_lowercase()) {
        Cow::Owned(raw.to_ascii_uppercase())
    } else {
        Cow::Borrowed(raw)
    }
}

/// A run of parts as one `Doc`: the part itself when there is only one.
fn one_or_concat(mut parts: Vec<Doc<'_>>) -> Doc<'_> {
    if parts.len() == 1 {
        parts.pop().unwrap_or(Doc::Nil)
    } else {
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
