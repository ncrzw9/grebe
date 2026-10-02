//! The compiled grammar: expression trees for every vendored rule.
//!
//! `build.rs` reads `vendor/grammar/**` and emits static tables that this module
//! re-exports. No `.gram` text is read at runtime.
//!
//! # Meta-syntax of the `.gram` files (snapshot `ce512b8`)
//!
//! Small enough to state completely:
//!
//! ```text
//! Rule       <- Name '<-' Expr          rule definition
//! Macro(D)   <- ...                     parameterised rule; D is substituted
//! a b c                                 sequence
//! a / b                                 ordered choice
//! (a b)                                 group
//! a?  a*  a+                            optional / zero-or-more / one-or-more
//! !a  &a                                negative / positive lookahead
//! 'LITERAL'                             literal (keywords, punctuation)
//! ```
//!
//! Two macros are defined in `statements/common.gram` and used throughout:
//!
//! ```text
//! List(D)   <- D (',' D)* ','?          note the trailing comma — friendly SQL
//! Parens(D) <- '(' D ')'
//! ```
//!
//! Entry point: `Program <- TopLevelStatement*`, where
//! `TopLevelStatement <- Statement? (';'+ / EndOfInput)`. `Statement` is a
//! 35-arm ordered choice.
//!
//! # Invariants `build.rs` enforces at compile time
//!
//! DuckDB's own `scripts/parser/inline_grammar.py` validates the same set, and
//! builds the vendored grammar (1,071 rules + 2 macros) with zero errors. Each
//! is a hard build failure here:
//!
//! - no duplicate rule or macro names
//! - no reference to an undefined rule or macro
//! - no unused rule (excluding the entry point, the override set and
//!   `%`-prefixed directives)
//! - macro arity always matches: the grammar syntax allows exactly one
//!   parameter per macro and one argument per call

/// A node in a rule's expression tree.
///
/// Index-addressed into a flat arena rather than boxed — the same choice the CST
/// makes, for the same reason (no `Rc<RefCell<_>>` trees).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExprId(pub u32);

/// A grammar rule, addressed by index into the compiled table.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct RuleId(pub u32);

#[derive(Clone, Debug)]
pub enum Expr {
    /// A literal keyword or punctuation string.
    Literal(&'static str),
    /// Invoke another rule.
    Rule(RuleId),
    /// An identifier rule with a keyword-category override applied.
    Identifier(crate::keyword::Category),
    Seq(&'static [ExprId]),
    /// Ordered choice — first match wins, no backtracking past a committed arm.
    Choice(&'static [ExprId]),
    Optional(ExprId),
    ZeroOrMore(ExprId),
    OneOrMore(ExprId),
    /// Real PEG semantics, unlike DuckDB's matcher, which ignores `!`.
    Not(ExprId),
    And(ExprId),
    /// End of input, an override rule rather than grammar text.
    EndOfInput,
}

include!(concat!(env!("OUT_DIR"), "/grammar_tables.rs"));
