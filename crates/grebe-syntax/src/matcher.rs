//! Token-based packrat matcher over the compiled grammar.
//!
//! DuckDB's PEG engine is **not** cpp-peglib: it is DuckDB's own token-based
//! packrat matcher (`matcher*.cpp`, `parser_packrat.cpp`). This is a port of
//! that shape, not of a generic PEG library. It matches over non-trivia tokens
//! only; trivia is reattached to the tree afterwards so the CST stays lossless.

use std::collections::HashMap;

use crate::Span;
use crate::cst::{Node, NodeId, Tree};
use crate::grammar::{
    EXPRESSIONS, Expr, ExprId, FIRST_FLAGS, FIRST_LITERALS, FIRST_OF, LITERALS,
    RULE_ANY_OPERATOR_LITERAL, RULE_BARE_LABEL_KEYWORD, RULE_COLUMN_NAME_KEYWORD,
    RULE_END_OF_INPUT, RULE_FUNC_NAME_KEYWORD, RULE_NEWLINE_BEFORE, RULE_NUMBER_LITERAL,
    RULE_OPERATOR_LITERAL, RULE_PROGRAM, RULE_RESERVED_KEYWORD, RULE_STRING_LITERAL,
    RULE_TYPE_NAME_KEYWORD, RULE_UNRESERVED_KEYWORD, RULES, RuleId, classify_keyword,
};
use crate::keyword::Keyword;
use crate::token::{Token, TokenKind, tokenize};

const OPRUN: &[char] = &[
    '+', '-', '*', '/', '%', '^', '<', '>', '=', '~', '!', '@', '&', '|',
];

/// `NumberLiteralMatcher` in DuckDB 2.0: the tokenizer takes any run of
/// digits and dots, and the matcher then refuses a second dot or a dot in
/// the exponent, so `1.2.3` and `1..2` are syntax errors.
fn well_formed_number(text: &[u8]) -> bool {
    let mut has_dot = false;
    let mut exponent = false;
    // `0x1F` and `0b101`: digits only, and `e` there is a hex digit.
    if text.len() > 1 && text[0] == b'0' && matches!(text[1], b'x' | b'X' | b'b' | b'B') {
        return true;
    }
    for &b in text {
        match b {
            b'.' if has_dot || exponent => return false,
            b'.' => has_dot = true,
            b'e' | b'E' => exponent = true,
            _ => {}
        }
    }
    true
}

/// `OperatorMatcher::HasSpecialPrecedence` in DuckDB 2.0.
fn has_special_precedence(op: &[u8]) -> bool {
    matches!(
        op,
        b"=" | b"<"
            | b">"
            | b"~"
            | b"->"
            | b"<="
            | b">="
            | b"!="
            | b"=="
            | b"<>"
            | b"~~"
            | b"~*"
            | b"!~"
            | b"~~*"
            | b"~~~"
            | b"!~~"
            | b"!~*"
            | b"!~~*"
    )
}

pub struct Matcher<'a> {
    src: &'a [u8],
    match_tokens: Vec<(Token, usize)>, // non-trivia tokens and their original index
    /// Each match token's keyword categories, computed once: identifier and
    /// keyword rules ask about the same token many times during backtracking.
    keywords: Vec<Keyword>,
    /// Each match token as a FIRST set sees it: its kind as a `FIRST_*` bit,
    /// and which grammar literal its text is, if any.
    starts: Vec<(u8, u16)>,
    /// Packrat state for every `(rule, token position)` in the window
    /// `base..base + width`, at `rule * width + (token - base)`: [`UNSEEN`],
    /// [`ACTIVE`] (being matched; re-entering means left recursion),
    /// [`FAILED`], or a match: `HIT_BASE + i` for `hits[i]` while nodes are
    /// built as rules match, and `HIT_BASE` plus the token the match ended
    /// at otherwise, when there is no subtree to remember. A flat table, so
    /// a rule entry is an array read.
    ///
    /// The window is one top-level statement: `Program <- TopLevelStatement*`
    /// never backtracks into a statement it has matched, so when the next
    /// one starts, everything memoised so far is dead and the window, `hits`
    /// and `pool` start again (see [`Matcher::start_statement`]). Memory
    /// follows the largest statement, not the file: a table over the whole
    /// file took ~4 KB per token (1,000+ rules) and its subtree pool as much
    /// again, ~120 KB per line of typical SQL.
    memo: Vec<u32>,
    /// The window slots written since the window opened, so the next
    /// statement can reset those and nothing else. Up to [`TOUCHED_LIMIT`]:
    /// past that the list would rival the table (a statement of a million
    /// tokens writes ten million slots), so it is given up, `touched_all`
    /// is set, and the table is replaced instead of reset.
    touched: Vec<usize>,
    touched_all: bool,
    base: usize,
    width: usize,
    /// Entries outside the window (lookahead past a statement's `;`),
    /// keyed by `rule * positions + token`. Rare, and keeps the window an
    /// optimisation only: no position goes unmemoised.
    overflow: HashMap<usize, u32>,
    positions: usize,
    /// The `TopLevelStatement*` loop of `Program`, where windows begin.
    statements: Option<ExprId>,
    /// Successful matches made while building nodes: where each ended, and
    /// its subtree as a range of `pool`. Empty otherwise: a statement is
    /// matched without nodes first, and its matches are in `memo` whole.
    ///
    /// The subtree matters: a memo that stored only the end position would
    /// skip node construction on every hit, and expression rules are
    /// re-entered at the same position constantly through the precedence
    /// chain, so `a + 1 * 2` would collapse to a single leaf.
    hits: Vec<(usize, u32, u32)>,
    /// Every memoised subtree, back to back: storing one is an append and
    /// replaying one is a copy, with no allocation per entry.
    pool: Vec<RawNode>,
    far: usize,
    nodes: Vec<RawNode>,
    depth: u32,
    /// Accept/reject callers (`parse_check`, the differential gate) do not
    /// need a tree, and building one costs roughly half the throughput.
    build_nodes: bool,
    /// Set when [`Matcher::build`] could not follow the memo table (see
    /// there); the caller then matches again the slow way.
    build_failed: bool,
    /// Match and build in one pass, copying subtrees through `pool`. The
    /// fallback for a statement `build` gave up on.
    one_pass: bool,
}

/// Recursion ceiling for `evaluate_rule`.
///
/// A stack overflow in Rust ABORTS -- it is not a catchable error. Measured:
/// ~10 rule frames per paren level; without a ceiling, 1,000 nested parens
/// fit on the stack and 5,000 abort with SIGABRT. This ceiling turns that
/// abort into an ordinary parse failure, which is the difference between a
/// diagnostic and a crash on a hostile or generated file. At ~10 frames per
/// level it still admits a few hundred levels of nesting; the tests pin 200
/// as parsing and 1,000 as rejected.
const MAX_DEPTH: u32 = 10_000;

#[derive(Clone, Copy)]
enum Slot {
    Window(usize),
    Overflow(usize),
}

const UNSEEN: u32 = 0;
const ACTIVE: u32 = 1;
const FAILED: u32 = 2;
const HIT_BASE: u32 = 3;

/// A node as the matcher builds it. The arena is in post-order, so a node's
/// descendants are exactly the `size` nodes just before it, and a subtree is
/// a contiguous run that carries no ids. Memoising or replaying one is a
/// plain copy; parent and child links are derived once, after the match, by
/// [`link`].
///
/// A node covers the code tokens `start..end`, counted without trivia. A
/// node that matched nothing (an absent optional clause) has `start == end`,
/// at the token that follows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FlatNode {
    pub rule: RuleId,
    pub start: u32,
    pub end: u32,
    /// Number of descendants.
    pub size: u32,
}

type RawNode = FlatNode;

/// The byte span of a node over the code tokens `start..end`: from the first
/// token's start to the last token's end, and empty, at the next token (or
/// the end of the last one), for a node that matched nothing.
fn byte_span(tokens: &[(Token, usize)], start: usize, end: usize) -> Span {
    let start_byte = if start < tokens.len() {
        tokens[start].0.span.start
    } else if let Some(last) = tokens.last() {
        last.0.span.end
    } else {
        0
    };
    let end_byte = if end > start && end <= tokens.len() {
        tokens[end - 1].0.span.end
    } else {
        start_byte
    };
    Span::new(start_byte, end_byte)
}

// A FIRST set's flags (build.rs computes the sets and has the same bits).
const FIRST_NULLABLE: u8 = 1;
const FIRST_WORD: u8 = 1 << 1;
const FIRST_QUOTED_IDENT: u8 = 1 << 2;
const FIRST_STRING: u8 = 1 << 3;
const FIRST_NUMBER: u8 = 1 << 4;
const FIRST_OPERATOR: u8 = 1 << 5;
const NO_LITERAL: u16 = u16::MAX;

/// Which entry of `LITERALS` a token's text is, compared the way
/// `Expr::Literal` compares: ASCII case folded, nothing else.
fn literal_id(text: &[u8]) -> u16 {
    let mut buf = [0u8; 32];
    if text.len() > buf.len() {
        return NO_LITERAL; // longer than any grammar literal
    }
    let lower = &mut buf[..text.len()];
    lower.copy_from_slice(text);
    lower.make_ascii_lowercase();
    LITERALS
        .binary_search_by(|l| l.as_bytes().cmp(lower))
        .map_or(NO_LITERAL, |i| i as u16)
}

/// Largest memo table kept for the next parse on the thread: 64 MB.
const SPARE_MEMO_LIMIT: usize = 1 << 24;

/// Most slots listed in `Matcher::touched`: 8 MB of them.
const TOUCHED_LIMIT: usize = 1 << 20;

thread_local! {
    /// A memo table with every slot [`UNSEEN`], left by the last matcher.
    static SPARE_MEMO: std::cell::Cell<Vec<u32>> = const { std::cell::Cell::new(Vec::new()) };
}

impl Drop for Matcher<'_> {
    fn drop(&mut self) {
        if self.memo.len() > SPARE_MEMO_LIMIT || self.touched_all {
            return;
        }
        for &i in &self.touched {
            self.memo[i] = UNSEEN;
        }
        SPARE_MEMO.with(|m| m.set(std::mem::take(&mut self.memo)));
    }
}

impl<'a> Matcher<'a> {
    fn new(src: &'a str, tokens: &[Token], build_nodes: bool) -> Self {
        let match_tokens: Vec<(Token, usize)> = tokens
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, t)| !t.kind.is_trivia())
            .map(|(i, t)| (t, i))
            .collect();
        let keywords = match_tokens
            .iter()
            .map(|(t, _)| {
                if t.kind == TokenKind::Word {
                    classify_keyword(
                        std::str::from_utf8(t.span.slice(src.as_bytes())).unwrap_or(""),
                    )
                } else {
                    Keyword::NONE
                }
            })
            .collect();
        let starts = match_tokens
            .iter()
            .map(|(t, _)| {
                let class = match t.kind {
                    TokenKind::Word => FIRST_WORD,
                    TokenKind::QuotedIdent => FIRST_QUOTED_IDENT,
                    TokenKind::String => FIRST_STRING,
                    TokenKind::Number => FIRST_NUMBER,
                    TokenKind::Operator => FIRST_OPERATOR,
                    _ => 0,
                };
                (class, literal_id(t.span.slice(src.as_bytes())))
            })
            .collect();
        let match_tokens_len = match_tokens.len();
        Matcher {
            src: src.as_bytes(),
            match_tokens,
            keywords,
            starts,
            // The last parse on this thread left its table behind, all
            // slots unseen again: a file of one query would otherwise zero
            // a table of its own.
            memo: SPARE_MEMO.with(|m| m.take()),
            touched: Vec::new(),
            touched_all: false,
            base: 0,
            width: 0,
            overflow: HashMap::new(),
            positions: match_tokens_len + 1,
            statements: match EXPRESSIONS[RULES[RULE_PROGRAM.0 as usize].expr.0 as usize] {
                Expr::ZeroOrMore(_) => Some(RULES[RULE_PROGRAM.0 as usize].expr),
                _ => None,
            },
            hits: Vec::new(),
            pool: Vec::new(),
            far: 0,
            nodes: Vec::new(),
            depth: 0,
            build_nodes,
            build_failed: false,
            one_pass: false,
        }
    }

    /// Forget everything memoised and open a window for the top-level
    /// statement at `at`: its tokens through the first `;` after it, plus
    /// the position past that.
    fn start_statement(&mut self, at: usize) {
        let end = self.match_tokens[at..]
            .iter()
            .position(|(t, _)| t.span.slice(self.src) == b";")
            .map_or(self.match_tokens.len(), |i| at + i + 1);
        self.base = at;
        self.width = end - at + 1;
        let needed = RULES.len() * self.width;
        if needed > self.memo.len() || self.touched_all {
            // A fresh zeroed table, not a resized one: the allocator hands
            // out zeroed pages lazily, so only the slots this statement
            // touches cost anything -- a file with no `;` is one window
            // over the whole file.
            self.memo = Vec::new(); // the old table goes before the new one comes
            self.memo = vec![UNSEEN; needed];
        } else {
            // The table is reused: a statement writes a few slots per token
            // out of a thousand, so undoing those is far cheaper than
            // zeroing a table per statement, which was an eighth of the
            // time to lint a file of ordinary queries.
            for &i in &self.touched {
                self.memo[i] = UNSEEN;
            }
        }
        self.touched.clear();
        self.touched_all = false;
        self.overflow.clear();
        self.hits.clear();
        self.pool.clear();
    }

    fn slot(&self, rule_id: RuleId, token_idx: usize) -> Slot {
        let off = token_idx.wrapping_sub(self.base);
        if off < self.width {
            Slot::Window(rule_id.0 as usize * self.width + off)
        } else {
            Slot::Overflow(rule_id.0 as usize * self.positions + token_idx)
        }
    }

    fn memo_get(&self, slot: Slot) -> u32 {
        match slot {
            Slot::Window(i) => self.memo[i],
            Slot::Overflow(k) => self.overflow.get(&k).copied().unwrap_or(UNSEEN),
        }
    }

    fn memo_set(&mut self, slot: Slot, v: u32) {
        match slot {
            Slot::Window(i) => self.memo[i] = v,
            Slot::Overflow(k) => {
                self.overflow.insert(k, v);
            }
        }
    }

    fn evaluate_rule(&mut self, rule_id: RuleId, token_idx: usize) -> Option<usize> {
        let slot = self.slot(rule_id, token_idx);

        // Return ANY cached result, success included. Returning only on a
        // cached failure would re-evaluate every successful rule from scratch,
        // defeating packrat entirely.
        match self.memo_get(slot) {
            UNSEEN => {}
            ACTIVE | FAILED => return None, // ACTIVE: guard left recursion
            hit if !self.build_nodes => {
                let end = (hit - HIT_BASE) as usize;
                if end > self.far {
                    self.far = end;
                }
                return Some(end);
            }
            hit => {
                let (end, offset, len) = self.hits[(hit - HIT_BASE) as usize];
                let (offset, len) = (offset as usize, len as usize);
                self.nodes
                    .extend_from_slice(&self.pool[offset..offset + len]);
                if end > self.far {
                    self.far = end;
                }
                return Some(end);
            }
        }
        if self.depth >= MAX_DEPTH {
            return None;
        }
        if let Slot::Window(i) = slot {
            if self.touched.len() < TOUCHED_LIMIT {
                self.touched.push(i);
            } else {
                self.touched_all = true;
            }
        }
        self.memo_set(slot, ACTIVE);
        self.depth += 1;

        let mut body_start = self.nodes.len();
        let res = match rule_id {
            RULE_STRING_LITERAL => {
                if token_idx < self.match_tokens.len() {
                    let t = self.match_tokens[token_idx].0;
                    if t.kind == TokenKind::String {
                        Some(token_idx + 1)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            RULE_NUMBER_LITERAL => {
                if token_idx < self.match_tokens.len() {
                    let t = self.match_tokens[token_idx].0;
                    if t.kind == TokenKind::Number && well_formed_number(t.span.slice(self.src)) {
                        Some(token_idx + 1)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            // Zero-width guard: succeeds iff the gap before this token
            // contains a newline. Adjacent string literals concatenate in SQL,
            // but only across a NEWLINE -- a rule DuckDB inherits from
            // Postgres. `SELECT 'a' 'b'` is a syntax error; the same two
            // literals on separate lines are one value. The vendored grammar
            // has no rule for this at all, so without it every such statement
            // would be a false parse error on valid SQL.
            //
            // It has to live here rather than in the grammar because it is a
            // question about the trivia BETWEEN two tokens, which a PEG shape
            // cannot ask. Consuming nothing is what keeps it usable inside an
            // ordinary grammar sequence: `StringLiteral (NewlineBefore
            // StringLiteral)*` still matches every literal with the real
            // `StringLiteral` rule, so the CST keeps proper `StringLiteral`
            // nodes -- which rules depend on (MOD016 reads string literals to
            // find CTE names referenced by `stack('cte', ...)`).
            RULE_NEWLINE_BEFORE => {
                if token_idx == 0 || token_idx >= self.match_tokens.len() {
                    None
                } else {
                    let prev = self.match_tokens[token_idx - 1].0.span;
                    let next_start = self.match_tokens[token_idx].0.span.start as usize;
                    let gap = &self.src[prev.end as usize..next_start];
                    // A continuation line is a plain single-quoted string
                    // (no `E`/`X`/`B` prefix) after a newline, with no block
                    // comment in between, and a dollar-quoted string is
                    // never continued.
                    if gap.contains(&b'\n')
                        && !gap.windows(2).any(|w| w == b"/*")
                        && self.src[next_start] == b'\''
                        && self.src[prev.end as usize - 1] == b'\''
                    {
                        Some(token_idx) // zero-width
                    } else {
                        None
                    }
                }
            }
            // `OperatorMatcher` in DuckDB: any operator token. The generic
            // form leaves out the operators that have a precedence level of
            // their own in the grammar (comparisons, `->`, the LIKE family),
            // so `a = b` is a comparison and never a custom operator.
            RULE_OPERATOR_LITERAL | RULE_ANY_OPERATOR_LITERAL => {
                if token_idx < self.match_tokens.len() {
                    let t = self.match_tokens[token_idx].0;
                    let text = t.span.slice(self.src);
                    if t.kind == TokenKind::Operator
                        && text.iter().all(|&b| OPRUN.contains(&(b as char)))
                        && (rule_id == RULE_ANY_OPERATOR_LITERAL || !has_special_precedence(text))
                    {
                        Some(token_idx + 1)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            RULE_END_OF_INPUT => {
                if token_idx == self.match_tokens.len() {
                    Some(token_idx)
                } else {
                    None
                }
            }
            RULE_RESERVED_KEYWORD
            | RULE_UNRESERVED_KEYWORD
            | RULE_COLUMN_NAME_KEYWORD
            | RULE_FUNC_NAME_KEYWORD
            | RULE_BARE_LABEL_KEYWORD
            | RULE_TYPE_NAME_KEYWORD => {
                if token_idx < self.match_tokens.len() {
                    let t = self.match_tokens[token_idx].0;
                    if t.kind == TokenKind::Word {
                        let kw = self.keywords[token_idx];
                        let is_match = match rule_id {
                            RULE_RESERVED_KEYWORD => kw.has(Keyword::RESERVED),
                            RULE_UNRESERVED_KEYWORD => kw.has(Keyword::UNRESERVED),
                            RULE_COLUMN_NAME_KEYWORD => kw.has(Keyword::COLUMN_NAME),
                            RULE_FUNC_NAME_KEYWORD => kw.has(Keyword::FUNC_NAME),
                            RULE_BARE_LABEL_KEYWORD => kw.has(Keyword::BARE_LABEL),
                            _ => kw.has(Keyword::TYPE_NAME),
                        };
                        if is_match { Some(token_idx + 1) } else { None }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => {
                let expr_id = RULES[rule_id.0 as usize].expr;
                let start_nodes = self.nodes.len();
                body_start = start_nodes;
                let r = self.evaluate(expr_id, token_idx);
                if r.is_none() {
                    self.nodes.truncate(start_nodes);
                }
                r
            }
        };

        self.depth -= 1;

        if let Some(end_idx) = res {
            if !self.build_nodes {
                self.memo_set(slot, HIT_BASE + end_idx as u32);
                return res;
            }
            let size = (self.nodes.len() - body_start) as u32;
            self.nodes.push(RawNode {
                rule: rule_id,
                start: token_idx as u32,
                end: end_idx as u32,
                size,
            });
            // Program is matched once and never replayed: copying the
            // whole tree into the pool would only double it.
            if rule_id == RULE_PROGRAM {
                self.record_hit(slot, end_idx, 0, 0);
                return res;
            }
            // Memoise the whole subtree this rule produced so a later hit at
            // the same position replays it instead of re-matching.
            let offset = self.pool.len();
            self.pool.extend_from_slice(&self.nodes[body_start..]);
            let len = self.pool.len() - offset;
            self.record_hit(slot, end_idx, offset, len);
        } else {
            self.memo_set(slot, FAILED);
        }

        res
    }

    /// Emit the nodes of `expr_id` matched at `token_idx`, and return where
    /// the match ends. Only called for a match the memo table already
    /// holds: every rule under it was matched while the statement was
    /// recognised, so each rule is entered once and no alternative is tried
    /// for longer than a table lookup per rule it names. Nothing is copied.
    ///
    /// (Building nodes while matching has to keep each rule's subtree so a
    /// later memo hit can replay it. A value nested `d` deep is re-entered
    /// at every precedence level on the way down, and its subtree was copied
    /// each time: 400 nested parentheses took 460 ms, and one long statement
    /// held gigabytes of copies.)
    fn build(&mut self, expr_id: ExprId, token_idx: usize) -> usize {
        let expr = &EXPRESSIONS[expr_id.0 as usize];
        match expr {
            Expr::Literal(_) | Expr::Identifier(_) => token_idx + 1,
            Expr::Rule(rule_id) => self.build_rule(*rule_id, token_idx),
            Expr::Seq(items) => {
                let mut curr = token_idx;
                for &item in *items {
                    curr = self.build(item, curr);
                }
                curr
            }
            Expr::Choice(items) => {
                for &item in *items {
                    if self.evaluate(item, token_idx).is_some() {
                        return self.build(item, token_idx);
                    }
                }
                self.build_failed = true;
                token_idx
            }
            Expr::Optional(item) => {
                if self.evaluate(*item, token_idx).is_some() {
                    self.build(*item, token_idx)
                } else {
                    token_idx
                }
            }
            Expr::ZeroOrMore(item) | Expr::OneOrMore(item) => {
                let mut curr = token_idx;
                if matches!(expr, Expr::OneOrMore(_)) {
                    curr = self.build(*item, curr);
                }
                while curr < self.match_tokens.len() {
                    match self.evaluate(*item, curr) {
                        // A zero-width repetition ends the loop and leaves
                        // no nodes, as in `evaluate`.
                        Some(next) if next != curr => {
                            self.build(*item, curr);
                            curr = next;
                        }
                        _ => break,
                    }
                }
                curr
            }
            Expr::Not(_) | Expr::And(_) | Expr::EndOfInput => token_idx,
        }
    }

    fn build_rule(&mut self, rule_id: RuleId, token_idx: usize) -> usize {
        let slot = self.slot(rule_id, token_idx);
        let hit = self.memo_get(slot);
        if hit < HIT_BASE {
            self.build_failed = true;
            return token_idx;
        }
        // `build` follows a match made without nodes, so the entry is the
        // end itself.
        let end_idx = (hit - HIT_BASE) as usize;
        let body_start = self.nodes.len();
        let native = matches!(
            rule_id,
            RULE_STRING_LITERAL
                | RULE_NUMBER_LITERAL
                | RULE_NEWLINE_BEFORE
                | RULE_OPERATOR_LITERAL
                | RULE_ANY_OPERATOR_LITERAL
                | RULE_END_OF_INPUT
                | RULE_RESERVED_KEYWORD
                | RULE_UNRESERVED_KEYWORD
                | RULE_COLUMN_NAME_KEYWORD
                | RULE_FUNC_NAME_KEYWORD
                | RULE_BARE_LABEL_KEYWORD
                | RULE_TYPE_NAME_KEYWORD
        );
        if !native {
            // While the rule's own body is walked the rule is active, as it
            // was when the body was matched: a re-entry at this position
            // fails, which is how left recursion is refused.
            self.memo_set(slot, ACTIVE);
            let end = self.build(RULES[rule_id.0 as usize].expr, token_idx);
            self.memo_set(slot, hit);
            if end != end_idx {
                // The walk disagrees with the table. Not seen on any
                // corpus; the caller starts over in one pass.
                self.build_failed = true;
            }
        }
        let size = (self.nodes.len() - body_start) as u32;
        self.nodes.push(RawNode {
            rule: rule_id,
            start: token_idx as u32,
            end: end_idx as u32,
            size,
        });
        end_idx
    }

    fn record_hit(&mut self, slot: Slot, end: usize, offset: usize, len: usize) {
        self.memo_set(slot, HIT_BASE + self.hits.len() as u32);
        self.hits.push((end, offset as u32, len as u32));
    }

    /// Can a match of `expr_id` begin at `token_idx`? `false` is certain;
    /// `true` means it has to be tried.
    #[inline]
    fn can_start(&self, expr_id: ExprId, token_idx: usize) -> bool {
        let set = FIRST_OF[expr_id.0 as usize] as usize;
        let flags = FIRST_FLAGS[set];
        if flags & FIRST_NULLABLE != 0 {
            return true;
        }
        let Some(&(class, literal)) = self.starts.get(token_idx) else {
            return false; // end of input, and the expression needs a token
        };
        flags & class != 0
            || (literal != NO_LITERAL
                && FIRST_LITERALS[set][literal as usize >> 6] >> (literal & 63) & 1 != 0)
    }

    fn evaluate(&mut self, expr_id: ExprId, token_idx: usize) -> Option<usize> {
        if !self.can_start(expr_id, token_idx) {
            return None;
        }
        let expr = &EXPRESSIONS[expr_id.0 as usize];
        match expr {
            Expr::Literal(lit) => {
                if token_idx < self.match_tokens.len() {
                    let t = self.match_tokens[token_idx].0;
                    // Grammar literals are ASCII keywords and punctuation, and
                    // DuckDB folds keyword case in ASCII only, so a byte-wise
                    // ASCII comparison is both exact and allocation-free.
                    if t.span.slice(self.src).eq_ignore_ascii_case(lit.as_bytes()) {
                        let next = token_idx + 1;
                        if next > self.far {
                            self.far = next;
                        }
                        return Some(next);
                    }
                }
                None
            }
            Expr::Rule(rule_id) => {
                let r = self.evaluate_rule(*rule_id, token_idx);
                if let Some(end_idx) = r {
                    if end_idx > self.far {
                        self.far = end_idx;
                    }
                }
                r
            }
            Expr::Identifier(category) => {
                if token_idx < self.match_tokens.len() {
                    let t = self.match_tokens[token_idx].0;
                    // `""` is a zero-length delimited identifier: an error.
                    if t.kind == TokenKind::QuotedIdent && t.span.end - t.span.start > 2 {
                        let next = token_idx + 1;
                        if next > self.far {
                            self.far = next;
                        }
                        return Some(next);
                    }
                    if t.kind == TokenKind::Word {
                        let kw = self.keywords[token_idx];
                        if kw.matches(*category) {
                            let next = token_idx + 1;
                            if next > self.far {
                                self.far = next;
                            }
                            return Some(next);
                        }
                    }
                }
                None
            }
            Expr::Seq(items) => {
                let mut curr = token_idx;
                for &item in *items {
                    {
                        let next = self.evaluate(item, curr)?;
                        curr = next;
                    }
                }
                Some(curr)
            }
            Expr::Choice(items) => {
                for &item in *items {
                    let start_nodes = self.nodes.len();
                    if let Some(next) = self.evaluate(item, token_idx) {
                        return Some(next);
                    }
                    self.nodes.truncate(start_nodes);
                }
                None
            }
            Expr::Optional(item) => {
                let start_nodes = self.nodes.len();
                if let Some(next) = self.evaluate(*item, token_idx) {
                    Some(next)
                } else {
                    self.nodes.truncate(start_nodes);
                    Some(token_idx)
                }
            }
            Expr::ZeroOrMore(item) => {
                let top = self.statements == Some(expr_id);
                let mut curr = token_idx;
                while curr < self.match_tokens.len() {
                    if top {
                        self.start_statement(curr);
                        if self.build_nodes && !self.one_pass {
                            // Two passes over one statement: match it
                            // without nodes, which fills the memo table,
                            // then walk the match once to emit its nodes.
                            self.build_nodes = false;
                            let matched = self.evaluate(*item, curr);
                            let next = match matched {
                                Some(next) if next != curr => {
                                    self.build(*item, curr);
                                    Some(next)
                                }
                                _ => None,
                            };
                            self.build_nodes = true;
                            match next {
                                Some(next) => {
                                    curr = next;
                                    continue;
                                }
                                None => break,
                            }
                        }
                    }
                    let start_nodes = self.nodes.len();
                    if let Some(next) = self.evaluate(*item, curr) {
                        if next == curr {
                            self.nodes.truncate(start_nodes); // zero-width: drop its nodes
                            break;
                        }
                        curr = next;
                    } else {
                        self.nodes.truncate(start_nodes);
                        break;
                    }
                }
                Some(curr)
            }
            Expr::OneOrMore(item) => {
                if let Some(first) = self.evaluate(*item, token_idx) {
                    let mut curr = first;
                    while curr < self.match_tokens.len() {
                        let start_nodes = self.nodes.len();
                        if let Some(next) = self.evaluate(*item, curr) {
                            if next == curr {
                                self.nodes.truncate(start_nodes); // zero-width
                                break;
                            }
                            curr = next;
                        } else {
                            self.nodes.truncate(start_nodes);
                            break;
                        }
                    }
                    Some(curr)
                } else {
                    None
                }
            }
            Expr::Not(item) => {
                let start_nodes = self.nodes.len();
                let r = self.evaluate(*item, token_idx);
                self.nodes.truncate(start_nodes);
                if r.is_none() { Some(token_idx) } else { None }
            }
            Expr::And(item) => {
                let start_nodes = self.nodes.len();
                let r = self.evaluate(*item, token_idx);
                self.nodes.truncate(start_nodes);
                if r.is_some() { Some(token_idx) } else { None }
            }
            Expr::EndOfInput => {
                if token_idx == self.match_tokens.len() {
                    Some(token_idx)
                } else {
                    None
                }
            }
        }
    }
}

/// What [`Statements::advance`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// A top-level statement matched; [`Statements::nodes`] holds its tree.
    Statement,
    /// The input does not parse from here on. [`parse`] returns `None` for
    /// the same input.
    Rejected,
    /// Every token has been matched.
    End,
}

/// A file parsed one top-level statement at a time.
///
/// [`parse`] builds the tree of the whole file, linked and indexed, which
/// costs a few hundred bytes of tree per byte of SQL. A consumer that works
/// through a file statement by statement and keeps nothing between them
/// (the formatter) does not need the file's tree at once, nor the links: it
/// reads each statement's nodes as the matcher made them, in post-order,
/// and they are gone when the next statement is matched. Memory follows the
/// largest statement.
///
/// The nodes of the statements, one after the other, are the nodes [`parse`]
/// links into its arena, in the same order, without the `Program` root.
pub struct Statements<'a> {
    matcher: Matcher<'a>,
    tokens: Vec<Token>,
    at: usize,
}

impl<'a> Statements<'a> {
    #[must_use]
    pub fn new(src: &'a str) -> Self {
        let tokens = tokenize(src);
        let mut matcher = Matcher::new(src, &tokens, true);
        // `Program` is one level of the recursion [`parse`] counts against
        // `MAX_DEPTH`; a statement is as deep here as it is there.
        matcher.depth = 1;
        Statements {
            matcher,
            tokens,
            at: 0,
        }
    }

    /// Every token of the file, trivia included.
    #[must_use]
    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    /// The code tokens, which node ranges count, each with its index in
    /// [`Statements::tokens`].
    #[must_use]
    pub fn code_tokens(&self) -> &[(Token, usize)] {
        &self.matcher.match_tokens
    }

    /// The keyword categories of a code token, [`classify_keyword`] of its
    /// text: `Keyword::NONE` for anything but a word.
    #[must_use]
    pub fn keyword(&self, code_token: usize) -> Keyword {
        self.matcher.keywords[code_token]
    }

    /// The nodes of the statement the last [`Statements::advance`] matched,
    /// in post-order: a node's descendants are the `size` nodes before it,
    /// and the last node is the `TopLevelStatement`.
    #[must_use]
    pub fn nodes(&self) -> &[FlatNode] {
        &self.matcher.nodes
    }

    /// Match the next top-level statement.
    pub fn advance(&mut self) -> Step {
        let m = &mut self.matcher;
        m.nodes.clear();
        let at = self.at;
        if at >= m.match_tokens.len() {
            return Step::End;
        }
        let Some(Expr::ZeroOrMore(item)) =
            m.statements.map(|loop_id| &EXPRESSIONS[loop_id.0 as usize])
        else {
            return Step::Rejected;
        };
        let item = *item;
        // The two passes `evaluate` makes over a statement of `Program`:
        // match without nodes, then walk the match to emit them.
        m.start_statement(at);
        m.build_nodes = false;
        m.build_failed = false;
        let matched = m.evaluate(item, at);
        let next = match matched {
            Some(next) if next != at => {
                m.build(item, at);
                next
            }
            _ => {
                m.build_nodes = true;
                return Step::Rejected;
            }
        };
        m.build_nodes = true;
        if m.build_failed {
            // The walk lost the match (see `Matcher::build`): match and
            // build this statement in one pass instead.
            m.build_failed = false;
            m.nodes.clear();
            m.start_statement(at);
            m.one_pass = true;
            let again = m.evaluate(item, at);
            m.one_pass = false;
            if again != Some(next) {
                m.nodes.clear();
                return Step::Rejected;
            }
        }
        self.at = next;
        Step::Statement
    }
}

/// Parse the source code to construct a lossless concrete syntax tree (CST).
#[must_use]
pub fn parse(src: &str) -> Option<Tree> {
    let tokens = tokenize(src);
    let mut matcher = Matcher::new(src, &tokens, true);

    let mut end_idx = matcher.evaluate_rule(RULE_PROGRAM, 0)?;
    if matcher.build_failed {
        matcher = Matcher::new(src, &tokens, true);
        matcher.one_pass = true;
        end_idx = matcher.evaluate_rule(RULE_PROGRAM, 0)?;
    }
    if end_idx != matcher.match_tokens.len() || matcher.nodes.is_empty() {
        return None;
    }
    let mut nodes = link(&matcher.nodes, &matcher.match_tokens);
    let root_idx = nodes.len() - 1;

    // Node spans are derived from NON-trivia tokens, so leading and
    // trailing trivia (a file-header comment, a trailing newline) fall
    // outside every node. Stretch the root to the whole buffer so the
    // tree is genuinely lossless -- otherwise the formatter silently
    // drops a leading comment.
    nodes[root_idx].span = crate::Span::new(0, src.len() as u32);

    // Reattach every token, trivia included, to the innermost node whose
    // span contains it, walking down from the root. A file of thousands of
    // statements gives the root thousands of children, and lists (`VALUES`
    // rows, `IN` lists) can be as wide, so a linear scan per token would be
    // quadratic; wide nodes are searched through a sorted index instead.
    let mut wide: Vec<Option<Vec<NodeId>>> = vec![None; nodes.len()];
    for t in &tokens {
        let mut at = root_idx;
        while let Some(child) = containing_child(&nodes, &mut wide, at, t.span) {
            at = child;
        }
        nodes[at].tokens.push(*t);
    }

    Some(Tree::new(nodes, tokens, NodeId(root_idx as u32)))
}

/// The child of `parent` whose span contains `span`, if any.
///
/// Children are in match order, which is source order except for zero-width
/// nodes that attached to an ancestor (see [`link`]); a zero-width node never
/// contains a token, and the non-empty children never overlap, so at most
/// one child qualifies. Narrow nodes are scanned; a wide node gets an index of
/// its non-empty children, built once and binary-searched by start.
fn containing_child(
    nodes: &[Node],
    wide: &mut [Option<Vec<NodeId>>],
    parent: usize,
    span: Span,
) -> Option<usize> {
    const SCAN_LIMIT: usize = 16;
    let contains = |c: NodeId| {
        let s = nodes[c.0 as usize].span;
        span.start >= s.start && span.end <= s.end
    };
    let children = &nodes[parent].children;
    if children.len() <= SCAN_LIMIT {
        return children
            .iter()
            .copied()
            .find(|&c| contains(c))
            .map(|c| c.0 as usize);
    }
    let index = wide[parent].get_or_insert_with(|| {
        children
            .iter()
            .copied()
            .filter(|&c| {
                let s = nodes[c.0 as usize].span;
                s.start < s.end
            })
            .collect()
    });
    let after = index.partition_point(|&c| nodes[c.0 as usize].span.start <= span.start);
    let candidate = *index[..after].last()?;
    contains(candidate).then_some(candidate.0 as usize)
}

/// Turns the matcher's post-order arena into linked [`Node`]s. Walking in
/// order, every finished subtree root waits on a stack until the node that
/// spans it arrives: a node adopts the roots in its last `size` slots, in
/// source order. One pass, no search.
///
/// A root whose span lies outside the adopting node's is left for an
/// ancestor instead. That only happens to zero-width nodes (an optional
/// clause that matched nothing) placed at the next token, just past the
/// node that matched them; they attach to the nearest ancestor that
/// contains that position.
fn link(raw: &[RawNode], tokens: &[(Token, usize)]) -> Vec<Node> {
    let mut nodes: Vec<Node> = raw
        .iter()
        .map(|r| Node {
            rule: r.rule,
            span: byte_span(tokens, r.start as usize, r.end as usize),
            parent: None,
            children: Vec::new(),
            tokens: Vec::new(),
        })
        .collect();
    let mut roots: Vec<usize> = Vec::new();
    for (i, r) in raw.iter().enumerate() {
        let first = i - r.size as usize;
        let split = roots.partition_point(|&c| c < first);
        let mut children = Vec::new();
        let mut outside = Vec::new();
        let outer = nodes[i].span;
        for c in roots.drain(split..) {
            let span = nodes[c].span;
            if span.start >= outer.start && span.end <= outer.end {
                nodes[c].parent = Some(NodeId(i as u32));
                children.push(NodeId(c as u32));
            } else {
                outside.push(c);
            }
        }
        nodes[i].children = children;
        roots.extend(outside);
        roots.push(i);
    }
    nodes
}

/// Accept/reject only, no tree: what the differential test against a real
/// DuckDB engine compares. Returns (is_ok, far_token_idx).
#[must_use]
pub fn parse_check(src: &str) -> (bool, usize) {
    let tokens = tokenize(src);
    let mut matcher = Matcher::new(src, &tokens, false);

    let ok = if let Some(end_idx) = matcher.evaluate_rule(RULE_PROGRAM, 0) {
        end_idx == matcher.match_tokens.len()
    } else {
        false
    };
    (ok, matcher.far)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition token attachment must meet: the smallest node whose
    /// span contains the token, the earliest in the arena on a tie.
    fn brute_force_owner(tree: &Tree, t: &Token) -> usize {
        let mut best = (u32::MAX, usize::MAX);
        for (i, n) in tree.nodes.iter().enumerate() {
            if t.span.start >= n.span.start && t.span.end <= n.span.end && n.span.len() < best.0 {
                best = (n.span.len(), i);
            }
        }
        best.1
    }

    #[test]
    fn tokens_attach_to_their_innermost_node_in_wide_trees() {
        // A wide root (many statements), a wide VALUES list, and ON CONFLICT,
        // whose zero-width modifiers attach out of source order.
        let rows: Vec<String> = (0..60).map(|i| format!("({i}, 'x{i}')")).collect();
        let mut src = format!("INSERT INTO t (a, b) VALUES {};\n", rows.join(", "));
        for i in 0..120 {
            src.push_str(&format!(
                "INSERT INTO t (j, i) VALUES ({i}, 1) ON CONFLICT DO UPDATE SET j = EXCLUDED.j; -- n{i}\n"
            ));
        }
        let tree = parse(&src).expect("parses");
        let mut owner = vec![usize::MAX; tree.tokens.len()];
        for (i, n) in tree.nodes.iter().enumerate() {
            for t in &n.tokens {
                let k = tree.tokens.iter().position(|x| x.span == t.span).unwrap();
                owner[k] = i;
            }
        }
        for (k, t) in tree.tokens.iter().enumerate() {
            assert_eq!(
                owner[k],
                brute_force_owner(&tree, t),
                "token {k} at {:?}",
                t.span
            );
        }
    }

    /// Memo, subtree pool and hits are per top-level statement: after a
    /// file of many statements they hold one statement's worth, not the
    /// file's. (A table over the whole file took ~120 KB per line.)
    #[test]
    fn memo_is_sized_to_a_statement_not_the_file() {
        let one =
            "SELECT a, sum(b) FILTER (WHERE c > 1) AS s FROM t WHERE d IN (1, 2) GROUP BY a;\n";
        let src = one.repeat(2_000);
        let tokens = tokenize(&src);
        let mut m = Matcher::new(&src, &tokens, true);
        let end = m.evaluate_rule(RULE_PROGRAM, 0).expect("parses");
        assert_eq!(end, m.match_tokens.len());
        let per_statement = m.match_tokens.len() / 2_000;
        assert!(m.width <= per_statement + 1, "window {} tokens", m.width);
        assert_eq!(m.memo.len(), RULES.len() * m.width);
        assert!(
            m.pool.len() < 20 * per_statement * 30,
            "pool {} nodes",
            m.pool.len()
        );
        // And the tree is the whole file's.
        let tree = parse(&src).expect("parses");
        assert_eq!(tree.find("SelectStatement").len(), 2_000);
    }

    /// `Statements` against `parse`, which is the definition: the same
    /// verdict, the same nodes in the same order, and for every node that
    /// covers a token, the parent `link` gives it is the node whose `size`
    /// reaches back over it.
    fn assert_statements_match_parse(src: &str) {
        let tree = parse(src);
        let mut statements = Statements::new(src);
        let mut flat: Vec<(RuleId, Span)> = Vec::new();
        let mut parents: Vec<Option<usize>> = Vec::new();
        let mut rejected = false;
        loop {
            match statements.advance() {
                Step::End => break,
                Step::Rejected => {
                    rejected = true;
                    break;
                }
                Step::Statement => {
                    let base = flat.len();
                    let nodes = statements.nodes();
                    let code = statements.code_tokens();
                    assert_eq!(
                        RULES[nodes.last().unwrap().rule.0 as usize].name,
                        "TopLevelStatement"
                    );
                    let mut parent = vec![None; nodes.len()];
                    for (i, n) in nodes.iter().enumerate() {
                        flat.push((n.rule, byte_span(code, n.start as usize, n.end as usize)));
                        let first = i - n.size as usize;
                        let mut c = i;
                        while c > first {
                            c -= 1;
                            parent[c] = Some(base + i);
                            c -= nodes[c].size as usize;
                        }
                    }
                    parents.extend(parent);
                }
            }
        }
        let Some(tree) = tree else {
            assert!(rejected, "parse rejects, Statements does not: {src:?}");
            return;
        };
        assert!(!rejected, "Statements rejects, parse does not: {src:?}");
        // Everything but the `Program` root, which is last.
        let linked = &tree.nodes[..tree.nodes.len() - 1];
        assert_eq!(flat.len(), linked.len(), "{src:?}");
        let root = tree.root();
        for (i, n) in linked.iter().enumerate() {
            // The root's span is stretched over the file's trivia; no
            // statement node is the root.
            assert_eq!(flat[i], (n.rule, n.span), "node {i} of {src:?}");
            if n.span.is_empty() {
                continue;
            }
            let want = n.parent.filter(|&p| p != root).map(|p| p.0 as usize);
            assert_eq!(parents[i], want, "parent of node {i} of {src:?}");
        }
    }

    #[test]
    fn statements_yield_the_nodes_parse_links() {
        for src in [
            "",
            "-- only a comment\n",
            ";",
            "SELECT 1",
            "SELECT 1;;; SELECT 2; -- tail\n",
            "SELECT a + 1 * 2, count(*) FILTER (WHERE x) FROM t WHERE d IN (1, 2) GROUP BY ALL;",
            "FROM t SELECT a, b WHERE a > 1 ORDER BY 1 LIMIT 3; FROM u;",
            "INSERT INTO t (j, i) VALUES (1, 1) ON CONFLICT DO UPDATE SET j = EXCLUDED.j;\nSELECT 2",
            "WITH a AS (SELECT 1 AS x), b AS (FROM a) SELECT * FROM b UNION ALL SELECT 2;",
            "SELECT 1; THIS IS NOT SQL; SELECT 2;",
            "SELECT (;",
            "SELECT 'a'\n'b', $$x$$, e'\\n';",
        ] {
            assert_statements_match_parse(src);
        }
        let rows: Vec<String> = (0..40).map(|i| format!("({i}, 'x{i}')")).collect();
        assert_statements_match_parse(&format!(
            "INSERT INTO t VALUES {}; SELECT * FROM t WHERE a IN ({});",
            rows.join(", "),
            (0..40)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        // Every statement of the fixtures, alone and as the file it is in.
        for fixture in [
            include_str!("../tests/fixtures/duckdb20.sql"),
            include_str!("../tests/fixtures/known-divergences.sql"),
        ] {
            assert_statements_match_parse(fixture);
            let mut parsing = String::new();
            for span in crate::token::split_statements(fixture) {
                let stmt = &fixture[span.start as usize..span.end as usize];
                assert_statements_match_parse(stmt);
                if parse(stmt).is_some() {
                    parsing.push_str(stmt);
                    parsing.push('\n');
                }
            }
            assert!(parse(&parsing).is_some());
            assert_statements_match_parse(&parsing);
        }
    }

    /// DuckDB rejects a string, quoted identifier, dollar-quote or block
    /// comment with no closing delimiter, so the statement must not parse:
    /// otherwise it is linted and formatted as if it ended somewhere.
    #[test]
    fn unclosed_strings_and_comments_do_not_parse() {
        for src in [
            "SELECT 'abc",
            "SELECT e'abc\\'",
            "SELECT \"abc",
            "SELECT $$abc",
            "SELECT $t$abc$$",
            "SELECT 1 /* never closed",
            "SELECT 1 /* a /* b */ still open",
            "SELECT count(*) FROM t WHERE x = 'oops;\nSELECT 2;",
        ] {
            assert!(parse(src).is_none(), "parsed: {src:?}");
        }
        for src in [
            "SELECT 'it''s', e'a\\'b', \"a\"\"b\", $$x$$, $t$a$$b$t$",
            "SELECT /* a /* b */ c */ 1",
        ] {
            assert!(parse(src).is_some(), "did not parse: {src:?}");
        }
    }
}
