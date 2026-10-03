//! The `MOD` rule registry and its CST detectors.
//!
//! Registry rows are data, not code paths: adding a rule is adding a row.
//!
//! # What ships in v1
//!
//! Both bands:
//!
//! - **default-on:** MOD001-009, MOD011-016, MOD021, MOD026-030, MOD032,
//!   MOD035-040
//! - **opt-in (`off` until `[severity]` enables them):** MOD010, MOD017-020,
//!   MOD022-023, MOD025, MOD031, MOD033-034
//!
//! A rule is opt-in when its premise is real but it is noisy on idiomatic
//! DuckDB code, its payoff is unproven, or it is house style by nature. A CST
//! does not by itself license an unproven detector; each candidate rule needs
//! its noise measured on a corpus of valid SQL first.
//!
//! # Scope
//!
//! - **DuckDB SQL only.** Statements in another dialect are classified and
//!   skipped, not linted (see [`source`]).
//! - **DDL is a checked surface.** Every statement that parses runs every
//!   detector; there is no statement-type gate, so `CREATE TABLE`/`CREATE
//!   VIEW` bodies are linted like queries.
//! - **No layout rules.** Whitespace, casing and line breaking belong to the
//!   formatter; formatting is never a diagnostic.
//!
//! # When MOD rules run
//!
//! Only on statements that parse. A rejected statement gets exactly one
//! `PRS`/`SRC` finding explaining why it was not linted and nothing else, so
//! opinions never pile onto a statement that is already broken or deliberately
//! skipped (see [`analysis`]). Every detector is a pure function of the source
//! bytes and its CST, so the same input always yields the same findings.
//!
//! # Fix safety
//!
//! `safe` / `unsafe` / `none`, per rule. `safe` means the rewrite is
//! equivalent by DuckDB's own definition; `unsafe` means it can change
//! results (even from wrong to right); `none` means the rewrite needs human
//! judgement or knowledge this tool does not have, such as schemas. `--fix`
//! applies `safe` only; `--fix --unsafe` includes the rest. On a corpus of
//! TPC-H/TPC-DS queries and examples from DuckDB's documentation, 203 of 220
//! findings come from rules with a safe fix.
//!
//! Edits are byte ranges on the original buffer: sorted, overlap-rejected,
//! applied in one pass. Reaching a fixed point is a test — `--fix` over fixed
//! output must produce no edits.
//!
//! # The prose-matching prohibition
//!
//! Diagnostics are structured values with a code
//! and a byte span. Nothing downstream ever matches on message text.

pub mod registry;

pub use registry::{Category, FixSafety, RULES, Rule, Severity, expand_select, lookup};
pub mod analysis;
pub mod config;
pub mod detect;
pub mod fix;
pub mod source;
pub mod suppress;
pub mod tier2a;
pub mod tier2b;
pub mod tier3;
pub mod tier4;
pub mod tier5;

/// The rule codes that have a detector.
///
/// The registry is the full catalogue of rules; this is the subset a run can
/// emit. They are not the same list, and treating them as one misleads anyone
/// reading `grebe rules` -- a registered rule with no detector is silent, not
/// clean. Add a code here in the same commit that adds its detector; the
/// test below fails if a code here has no registry row.
pub const IMPLEMENTED: &[&str] = &[
    "MOD001", "MOD002", "MOD003", "MOD004", "MOD005", "MOD006", "MOD007", "MOD008", "MOD009",
    "MOD010", "MOD011", "MOD012", "MOD013", "MOD014", "MOD015", "MOD016", "MOD017", "MOD018",
    "MOD019", "MOD020", "MOD021", "MOD022", "MOD023", "MOD025", "MOD026", "MOD027", "MOD028",
    "MOD029", "MOD030", "MOD031", "MOD032", "MOD033", "MOD034", "MOD035", "MOD036", "MOD037",
    "MOD038", "MOD039", "MOD040", "PRS001", "SRC002", "SRC003",
];

/// Does `code` have a detector?
pub fn is_implemented(code: &str) -> bool {
    IMPLEMENTED.contains(&code)
}

#[cfg(test)]
mod implemented_tests {
    #[test]
    fn every_implemented_code_has_a_registry_row() {
        for c in super::IMPLEMENTED {
            assert!(
                super::lookup(c).is_some(),
                "{c} has a detector but no registry row"
            );
        }
    }

    #[test]
    fn implemented_codes_are_sorted_and_unique() {
        let mut sorted = super::IMPLEMENTED.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.as_slice(), super::IMPLEMENTED);
    }
}
