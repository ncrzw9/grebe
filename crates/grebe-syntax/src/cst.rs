//! The lossless concrete syntax tree.
//!
//! One tree feeds both the linter and the formatter. Clause-level structure
//! (the WHERE clause, a GROUP BY's extent — what most diagnostics point at)
//! comes from here: DuckDB's own AST carries no source location for clause or
//! statement nodes, so that structure cannot be borrowed from the engine.
//!
//! # Why a CST and not the engine's AST
//!
//! Lint rules read this tree rather than `json_serialize_sql` output, and get
//! strictly better information, because the serialized AST is lossy in
//! measurable ways:
//!
//! - `GROUP BY ALL` is desugared away entirely
//! - `ORDER BY ALL` desugars to `COLUMNS(*)`
//! - `PIVOT` is rewritten into something that is not what the user typed
//! - EXCLUDE lists reorder through serialize/deserialize on the 2.0 preview,
//!   which would make AST-driven autofixes unsafe
//! - 2.0 renamed `FUNCTION.arguments`, breaking every AST consumer
//!
//! A tree that never leaves the source text has none of these failure modes.
//!
//! # Structure
//!
//! An arena of index-addressed nodes, each carrying a [`Span`]. Not
//! `Rc<RefCell<Node>>` — shared mutable pointers would dodge lifetimes rather
//! than model the data; the arena is also what makes spans free and the LSP
//! layer cheap.
//!
//! Losslessness is a test, not a claim: every byte of the source is reachable
//! from exactly one node, trivia included, and re-concatenating the tree in
//! source order reproduces the input exactly. Trivia (whitespace, comments) is
//! attached to the smallest enclosing node rather than discarded. The test must
//! be falsifiable: an assertion that cannot fail proves nothing and hides real
//! extent defects.

use crate::Span;
use crate::grammar::RuleId;
use crate::token::Token;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct NodeId(pub u32);

#[derive(Clone, Debug)]
pub struct Node {
    /// Which grammar rule produced this node.
    pub rule: RuleId,
    /// Source extent, trivia included.
    pub span: Span,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// Tokens owned directly by this node rather than by a child.
    pub tokens: Vec<Token>,
}

/// A parsed source file: the arena, plus the token stream it was built over.
#[derive(Clone, Debug)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub tokens: Vec<Token>,
    pub root: NodeId,
}

impl Tree {
    /// The root node — always the `Program` rule.
    #[must_use]
    pub fn root(&self) -> NodeId {
        self.root
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// The grammar rule that produced this node, e.g. `"SelectClause"`.
    ///
    /// Rule names come from the vendored grammar, so they are the names in
    /// `vendor/grammar/statements/*.gram` verbatim. Detectors match on these.
    #[must_use]
    pub fn rule_name(&self, id: NodeId) -> &'static str {
        crate::grammar::RULES[self.node(id).rule.0 as usize].name
    }

    /// Source text this node covers, trivia included.
    #[must_use]
    pub fn text<'s>(&self, id: NodeId, src: &'s str) -> &'s str {
        let sp = self.node(id).span;
        &src[sp.start as usize..sp.end as usize]
    }

    #[must_use]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).parent
    }

    #[must_use]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.node(id).children
    }

    /// Every node, in pre-order from the root.
    #[must_use]
    pub fn walk(&self) -> Vec<NodeId> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            out.push(id);
            // reversed so children come out left-to-right
            for &c in self.children(id).iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Pre-order descendants of `id`, excluding `id` itself.
    #[must_use]
    pub fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.children(id).iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            out.push(n);
            for &c in self.children(n).iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Every node produced by the named grammar rule, in pre-order.
    ///
    /// The workhorse for detectors: `tree.find("GroupByClause")`.
    #[must_use]
    pub fn find(&self, rule: &str) -> Vec<NodeId> {
        self.walk()
            .into_iter()
            .filter(|&id| self.rule_name(id) == rule)
            .collect()
    }

    /// Nearest ancestor produced by the named rule, if any.
    #[must_use]
    pub fn ancestor(&self, id: NodeId, rule: &str) -> Option<NodeId> {
        let mut cur = self.parent(id);
        while let Some(n) = cur {
            if self.rule_name(n) == rule {
                return Some(n);
            }
            cur = self.parent(n);
        }
        None
    }

    /// The non-trivia tokens this node owns directly.
    #[must_use]
    pub fn code_tokens(&self, id: NodeId) -> Vec<Token> {
        self.node(id)
            .tokens
            .iter()
            .filter(|t| !t.kind.is_trivia())
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::matcher::parse;

    fn t(sql: &str) -> crate::cst::Tree {
        parse(sql).unwrap_or_else(|| panic!("failed to parse: {sql}"))
    }

    #[test]
    fn root_covers_the_whole_input() {
        for sql in [
            "SELECT 1",
            "SELECT a, b FROM t WHERE x = 1",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "-- leading comment\nSELECT 1",
        ] {
            let tree = t(sql);
            let sp = tree.node(tree.root()).span;
            assert_eq!(sp.start, 0, "root does not start at 0: {sql}");
            assert_eq!(sp.end as usize, sql.len(), "root does not reach EOF: {sql}");
        }
    }

    #[test]
    fn children_nest_inside_their_parent() {
        let sql = "SELECT a, b FROM t WHERE x = 1 GROUP BY ALL";
        let tree = t(sql);
        for id in tree.walk() {
            let p = tree.node(id).span;
            for &c in tree.children(id) {
                let c = tree.node(c).span;
                assert!(
                    c.start >= p.start && c.end <= p.end,
                    "child {c:?} escapes parent {p:?}"
                );
            }
        }
    }

    #[test]
    fn siblings_do_not_overlap() {
        let tree = t("SELECT a, b, c FROM t");
        for id in tree.walk() {
            let kids: Vec<_> = tree
                .children(id)
                .iter()
                .map(|&c| tree.node(c).span)
                .collect();
            for w in kids.windows(2) {
                assert!(
                    w[0].end <= w[1].start,
                    "siblings overlap: {:?} {:?}",
                    w[0],
                    w[1]
                );
            }
        }
    }

    #[test]
    fn find_locates_grammar_rules_by_name() {
        let tree = t("SELECT a FROM t GROUP BY ALL");
        assert_eq!(tree.find("GroupByClause").len(), 1);
        assert_eq!(tree.find("SelectClause").len(), 1);
        assert!(tree.find("NoSuchRule").is_empty());
    }

    #[test]
    fn find_reaches_into_subqueries() {
        // A detector must see nested scopes, not just the outermost statement.
        let tree = t("SELECT * FROM (SELECT a FROM t GROUP BY ALL) s");
        assert_eq!(tree.find("GroupByClause").len(), 1);
    }

    #[test]
    fn text_round_trips_through_spans() {
        let sql = "SELECT a, b FROM t";
        let tree = t(sql);
        let sel = tree.find("SelectClause");
        assert_eq!(tree.text(sel[0], sql), "SELECT a, b");
        let from = tree.find("FromClause");
        assert_eq!(tree.text(from[0], sql), "FROM t");
    }

    #[test]
    fn ancestor_walks_up_to_the_named_rule() {
        let sql = "SELECT a, b FROM t";
        let tree = t(sql);
        let targets = tree.find("AliasedExpression");
        assert!(!targets.is_empty());
        let owner = tree.ancestor(targets[0], "SelectClause");
        assert!(
            owner.is_some(),
            "AliasedExpression has no SelectClause ancestor"
        );
    }

    #[test]
    fn every_target_list_item_is_reachable() {
        let tree = t("SELECT a, b, c FROM t");
        let list = tree.find("TargetList");
        assert_eq!(list.len(), 1);
        let items: Vec<_> = tree
            .descendants(list[0])
            .into_iter()
            .filter(|&n| tree.rule_name(n) == "AliasedExpression")
            .collect();
        assert_eq!(items.len(), 3, "expected 3 select-list items");
    }

    #[test]
    fn pathological_nesting_rejects_instead_of_aborting() {
        // A Rust stack overflow ABORTS -- it is not catchable. Without
        // MAX_DEPTH, 5,000 nested parens kill the process with SIGABRT, which
        // a differential harness would score as a clean reject.
        // Mirrors the CLI, which runs the parser on a 256 MB stack so the
        // "never aborts" promise does not depend on build profile.
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(|| {
                for n in [1_000usize, 5_000, 20_000] {
                    let sql = format!("SELECT {}1{}", "(".repeat(n), ")".repeat(n));
                    assert!(parse(&sql).is_none(), "expected a reject at depth {n}");
                }
                // Realistic depth must still parse.
                let sql = format!("SELECT {}1{}", "(".repeat(200), ")".repeat(200));
                assert!(parse(&sql).is_some(), "200-deep nesting should parse");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn expressions_keep_their_internal_structure() {
        // A memo that stored only a rule's END POSITION would skip node
        // construction on every cache hit, and expression rules are re-entered
        // at the same position constantly through the precedence chain. The
        // tree would silently collapse -- `a + 1 * 2` into a single leaf --
        // and no MOD rule could inspect an expression. The memo carries the
        // subtree and replays it; this pins that.
        let tree = t("SELECT a + 1 * 2 FROM t");
        assert!(
            tree.walk().len() > 40,
            "expression structure collapsed: only {} nodes",
            tree.walk().len()
        );

        // The productions MOD001 (count-star) needs must be reachable.
        let tree = t("SELECT count(*) FROM t");
        let names: Vec<_> = tree.walk().into_iter().map(|n| tree.rule_name(n)).collect();
        assert!(
            names.iter().any(|n| n.contains("Function")),
            "no Function production in tree: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("Star")),
            "no Star production in tree"
        );
    }

    #[test]
    fn non_ascii_identifiers_do_not_panic() {
        // `à` is 0xC3 0xA0, and 0xA0 read as a Latin-1 char is NBSP: the
        // tokenizer must not split it mid-character as whitespace.
        for sql in [
            "SELECT à FROM t",
            "SELECT café FROM t",
            "SELECT \"naïve\" FROM t",
        ] {
            let tree = t(sql);
            assert_eq!(tree.node(tree.root()).span.end as usize, sql.len());
        }
    }
}
