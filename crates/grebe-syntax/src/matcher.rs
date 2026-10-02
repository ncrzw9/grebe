//! Token-based packrat matcher over the compiled grammar.
//!
//! DuckDB's PEG engine is **not** cpp-peglib: it is DuckDB's own token-based
//! packrat matcher (`matcher*.cpp`, `parser_packrat.cpp`). This is a port of
//! that shape, not of a generic PEG library. It matches over non-trivia tokens
//! only; trivia is reattached to the tree afterwards so the CST stays lossless.

use crate::Span;
use crate::cst::{Node, NodeId, Tree};
use crate::grammar::{
    EXPRESSIONS, Expr, ExprId, RULE_COLUMN_NAME_KEYWORD, RULE_END_OF_INPUT, RULE_FUNC_NAME_KEYWORD,
    RULE_NEWLINE_BEFORE, RULE_NUMBER_LITERAL, RULE_OPERATOR_LITERAL, RULE_PROGRAM,
    RULE_RESERVED_KEYWORD, RULE_STRING_LITERAL, RULE_TYPE_NAME_KEYWORD, RULE_UNRESERVED_KEYWORD,
    RULES, RuleId, classify_keyword,
};
use crate::keyword::Keyword;
use crate::token::{Token, TokenKind, tokenize};

const OPRUN: &[char] = &[
    '+', '*', '/', '<', '>', '=', '~', '!', '@', '%', '^', '&', '|', '`',
];

pub struct Matcher<'a> {
    src: &'a [u8],
    match_tokens: Vec<(Token, usize)>, // non-trivia tokens and their original index
    /// Each match token's keyword categories, computed once: identifier and
    /// keyword rules ask about the same token many times during backtracking.
    keywords: Vec<Keyword>,
    /// Packrat state for every `(rule, token position)`, at
    /// `rule * positions + token`: [`UNSEEN`], [`ACTIVE`] (being matched;
    /// re-entering means left recursion), [`FAILED`], or `HIT_BASE + i` for
    /// `hits[i]`. One flat table replaces a hash map: a rule entry is an
    /// array read, and the table is allocated zeroed, which the allocator
    /// serves lazily for large inputs.
    memo: Vec<u32>,
    positions: usize,
    /// Successful matches: where each ended, and its subtree as a range of
    /// `pool`.
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

const UNSEEN: u32 = 0;
const ACTIVE: u32 = 1;
const FAILED: u32 = 2;
const HIT_BASE: u32 = 3;

/// A node as the matcher builds it. The arena is in post-order, so a node's
/// descendants are exactly the `size` nodes just before it, and a subtree is
/// a contiguous run that carries no ids. Memoising or replaying one is a
/// plain copy; parent and child links are derived once, after the match, by
/// [`link`].
#[derive(Clone, Copy)]
struct RawNode {
    rule: RuleId,
    span: Span,
    /// Number of descendants.
    size: u32,
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
        let match_tokens_len = match_tokens.len();
        Matcher {
            src: src.as_bytes(),
            match_tokens,
            keywords,
            memo: vec![UNSEEN; RULES.len() * (match_tokens_len + 1)],
            positions: match_tokens_len + 1,
            hits: Vec::new(),
            pool: Vec::new(),
            far: 0,
            nodes: Vec::new(),
            depth: 0,
            build_nodes,
        }
    }

    fn evaluate_rule(&mut self, rule_id: RuleId, token_idx: usize) -> Option<usize> {
        let slot = rule_id.0 as usize * self.positions + token_idx;

        // Return ANY cached result, success included. Returning only on a
        // cached failure would re-evaluate every successful rule from scratch,
        // defeating packrat entirely.
        match self.memo[slot] {
            UNSEEN => {}
            ACTIVE | FAILED => return None, // ACTIVE: guard left recursion
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
        self.memo[slot] = ACTIVE;
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
                    if t.kind == TokenKind::Number {
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
                    let prev_end = self.match_tokens[token_idx - 1].0.span.end as usize;
                    let next_start = self.match_tokens[token_idx].0.span.start as usize;
                    if self.src[prev_end..next_start].contains(&b'\n') {
                        Some(token_idx) // zero-width
                    } else {
                        None
                    }
                }
            }
            RULE_OPERATOR_LITERAL => {
                if token_idx < self.match_tokens.len() {
                    let t = self.match_tokens[token_idx].0;
                    if t.kind == TokenKind::Operator
                        && t.span
                            .slice(self.src)
                            .iter()
                            .all(|&b| OPRUN.contains(&(b as char)))
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
                self.record_hit(slot, end_idx, 0, 0);
                return res;
            }
            let start_byte = if token_idx < self.match_tokens.len() {
                self.match_tokens[token_idx].0.span.start
            } else if !self.match_tokens.is_empty() {
                self.match_tokens.last().unwrap().0.span.end
            } else {
                0
            };
            let end_byte = if end_idx > token_idx && end_idx <= self.match_tokens.len() {
                self.match_tokens[end_idx - 1].0.span.end
            } else {
                start_byte
            };

            let size = (self.nodes.len() - body_start) as u32;
            self.nodes.push(RawNode {
                rule: rule_id,
                span: Span::new(start_byte, end_byte),
                size,
            });
            // Memoise the whole subtree this rule produced so a later hit at
            // the same position replays it instead of re-matching.
            let offset = self.pool.len();
            self.pool.extend_from_slice(&self.nodes[body_start..]);
            let len = self.pool.len() - offset;
            self.record_hit(slot, end_idx, offset, len);
        } else {
            self.memo[slot] = FAILED;
        }

        res
    }

    fn record_hit(&mut self, slot: usize, end: usize, offset: usize, len: usize) {
        self.memo[slot] = HIT_BASE + self.hits.len() as u32;
        self.hits.push((end, offset as u32, len as u32));
    }

    fn evaluate(&mut self, expr_id: ExprId, token_idx: usize) -> Option<usize> {
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
                    if t.kind == TokenKind::QuotedIdent {
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
                let mut curr = token_idx;
                while curr < self.match_tokens.len() {
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

/// Parse the source code to construct a lossless concrete syntax tree (CST).
#[must_use]
pub fn parse(src: &str) -> Option<Tree> {
    let tokens = tokenize(src);
    let mut matcher = Matcher::new(src, &tokens, true);

    let end_idx = matcher.evaluate_rule(RULE_PROGRAM, 0)?;
    if end_idx != matcher.match_tokens.len() || matcher.nodes.is_empty() {
        return None;
    }
    let mut nodes = link(&matcher.nodes);
    let root_idx = nodes.len() - 1;

    // Node spans are derived from NON-trivia tokens, so leading and
    // trailing trivia (a file-header comment, a trailing newline) fall
    // outside every node. Stretch the root to the whole buffer so the
    // tree is genuinely lossless -- otherwise the formatter silently
    // drops a leading comment.
    nodes[root_idx].span = crate::Span::new(0, src.len() as u32);

    // Reattach every token, trivia included, to the innermost node whose
    // span contains it. Siblings never overlap, so that node is found by
    // walking down from the root, never by scanning the arena.
    for t in &tokens {
        let mut at = root_idx;
        'descend: loop {
            for &c in &nodes[at].children {
                let span = nodes[c.0 as usize].span;
                if t.span.start >= span.start && t.span.end <= span.end {
                    at = c.0 as usize;
                    continue 'descend;
                }
            }
            break;
        }
        nodes[at].tokens.push(*t);
    }

    Some(Tree {
        nodes,
        tokens,
        root: NodeId(root_idx as u32),
    })
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
fn link(raw: &[RawNode]) -> Vec<Node> {
    let mut nodes: Vec<Node> = raw
        .iter()
        .map(|r| Node {
            rule: r.rule,
            span: r.span,
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
        for c in roots.drain(split..) {
            let span = raw[c].span;
            if span.start >= r.span.start && span.end <= r.span.end {
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
