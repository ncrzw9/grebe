//! DuckDB SQL syntax: tokenizer, packrat matcher, lossless CST.
//!
//! Built from DuckDB's own PEG grammar, vendored verbatim under `vendor/grammar`
//! and compiled to matcher tables by `build.rs`. Nothing here links libduckdb,
//! opens a database, or parses grammar text at runtime.
//!
//! The grammar is DuckDB's rather than a third-party SQL parser's because an
//! approximation of DuckDB's dialect drifts from it silently; linking the
//! engine instead would put a database in the binary and still not expose its
//! tokenizer. The pinned grammar snapshot (`vendor/SNAPSHOT`) *is* the DuckDB
//! version this crate targets: there is no runtime version detection, and
//! refreshing the snapshot is a reviewed change, never a runtime fetch.
//!
//! The crate has no external dependencies and no `unsafe` (forbidden at the
//! workspace lint level); safe Rust is fast enough without it.
//!
//! ## Differential testing
//!
//! A vendored grammar is only trustworthy while it is checked against the
//! engine it came from. Accept/reject ([`matcher::parse_check`]) is compared
//! statement by statement with a real DuckDB build over a SQL corpus, and the
//! hard rule is **zero engine-only accepts**: nothing DuckDB parses may be
//! rejected here. DuckDB is a test-time oracle only, never a dependency.
//!
//! ## What the grammar text does NOT carry
//!
//! The `.gram` files are not self-contained.
//! A consumer must supply two things, both implemented in this crate:
//!
//! 1. [`token`] — a DuckDB-faithful tokenizer, ported from `base_tokenizer.cpp`.
//! 2. [`keyword`] — the identifier/keyword-category semantics that
//!    `matcher_factory.cpp` installs over ~25 lexical rules via `AddRuleOverride`.
//!
//! ## Known divergences (deliberate, each checked against a real DuckDB engine)
//!
//! A divergence is either corrected in the build-time override layer or pinned
//! as a regression fixture (`tests/fixtures/known-divergences.sql`) — never
//! patched into `vendor/`.
//!
//! - Negative lookahead `!` is given real PEG semantics here. DuckDB's matcher
//!   parses but ignores it; agreement with the engine holds regardless, so
//!   current uses (e.g. `PlainIdentifier <- !ReservedKeyword ...`) are benign.
//! - Bare `SELECT FROM x` (no target list): the grammar text accepts it, the
//!   shipping parser rejects it. This crate follows the engine, via a
//!   structural override in `build.rs`; the fixture pins the rejection.
//! - A `...` prose placeholder is rejected here although a DuckDB build may
//!   accept it; it is not SQL, and bending the tokenizer to admit it would make
//!   it wrong about real SQL.

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctest;

pub mod cst;
pub mod grammar;
pub mod keyword;
pub mod matcher;
pub mod token;

/// A half-open byte range into the source buffer.
///
/// Byte offsets into the UTF-8 source throughout, never char or UTF-16
/// offsets: token extents, node spans and diagnostics all share this one
/// coordinate system. Line/column (terminal) and UTF-16 code units (LSP) are
/// computed at the output boundary, and only there.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// The bytes this span covers, given the buffer it was produced from.
    #[must_use]
    pub fn slice(self, src: &[u8]) -> &[u8] {
        &src[self.start as usize..self.end as usize]
    }
}
