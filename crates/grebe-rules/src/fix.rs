//! Applying fixes.
//!
//! Every `Finding` carries an `Option<Fix>`; this module applies them: sort the edits,
//! reject overlaps, splice them onto the original buffer in a single pass.
//!
//! Two properties matter more than speed here, because this is the one part of
//! grebe that *writes to a user's file*:
//!
//! - **One pass over the original buffer.** Edit spans are byte ranges into the
//!   source as the rules saw it. Applying edits one at a time would invalidate
//!   every later span the moment the first replacement changed the length.
//! - **Overlaps are rejected, not merged.** Two rules wanting the same bytes
//!   disagree about what those bytes should say, and there is no principled way
//!   to pick. The earlier edit wins, the later one is dropped and counted, and
//!   a second `--fix` run picks it up against the new text.

use crate::detect::{Finding, Fix};
use crate::registry::FixSafety;

/// The result of applying a set of fixes to one buffer.
pub struct Applied {
    /// The rewritten source. `None` when nothing applied.
    pub output: Option<String>,
    /// How many edits were spliced in.
    pub applied: usize,
    /// How many were dropped because they overlapped an earlier edit.
    pub skipped_overlapping: usize,
}

/// Which fixes a run is allowed to apply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Safety {
    /// `--fix`: only rules whose registry `fix_safety` is `Safe`.
    SafeOnly,
    /// `--fix --unsafe`: `Safe` and `Unsafe` alike.
    IncludeUnsafe,
}

/// Is this finding's fix eligible under `safety`?
///
/// The gate is the *registry's* `fix_safety` for the rule, not anything the
/// finding carries. A rule declares once how dangerous its rewrite is; a
/// finding cannot promote itself.
fn eligible(f: &Finding, safety: Safety) -> bool {
    if f.fix.is_none() {
        return false;
    }
    match crate::lookup(f.code).map(|r| r.fix_safety) {
        Some(FixSafety::Safe) => true,
        Some(FixSafety::Unsafe) => safety == Safety::IncludeUnsafe,
        // `None` fix safety, or a code with no registry row: not fixable.
        _ => false,
    }
}

/// Apply every eligible fix in `findings` to `src`.
///
/// `findings` must carry **file-absolute** spans — what
/// [`crate::analysis::analyze_source`] returns.
pub fn apply(src: &str, findings: &[Finding], safety: Safety) -> Applied {
    let mut edits: Vec<&Fix> = findings
        .iter()
        .filter(|f| eligible(f, safety))
        .filter_map(|f| f.fix.as_ref())
        .collect();

    // Sort by start, then by end: a deterministic order is what makes "the
    // earlier edit wins" a rule rather than a coin flip.
    edits.sort_by_key(|e| (e.span.start, e.span.end));

    let mut out = String::with_capacity(src.len());
    let mut cursor: u32 = 0;
    let mut applied = 0usize;
    let mut skipped_overlapping = 0usize;

    for e in edits {
        // A reversed or out-of-bounds span is a rule bug; refuse it (counted
        // with the overlaps) rather than panic on the slice. A zero-width
        // span is a valid insertion.
        if e.span.end < e.span.start || e.span.end as usize > src.len() {
            skipped_overlapping += 1;
            continue;
        }
        if e.span.start < cursor {
            skipped_overlapping += 1;
            continue;
        }
        out.push_str(&src[cursor as usize..e.span.start as usize]);
        out.push_str(&e.replacement);
        cursor = e.span.end;
        applied += 1;
    }

    if applied == 0 {
        return Applied {
            output: None,
            applied: 0,
            skipped_overlapping,
        };
    }
    out.push_str(&src[cursor as usize..]);
    Applied {
        output: Some(out),
        applied,
        skipped_overlapping,
    }
}

/// The most passes [`apply_until_stable`] will make before giving up.
///
/// Convergence is normally one or two passes. The bound exists because a
/// rule set can in principle oscillate (A rewrites to B, B rewrites to A), and
/// a linter that spins forever on a user's file is worse than one that stops
/// and says so.
pub const MAX_PASSES: usize = 10;

/// The result of [`apply_until_stable`].
pub struct Converged {
    /// The final text; `None` if nothing changed at all.
    pub output: Option<String>,
    /// Edits applied, summed across passes.
    pub applied: usize,
    /// Passes that changed the text.
    pub passes: usize,
    /// True when the pass bound was reached and edits were still pending --
    /// a rule-set oscillation, and a bug worth reporting rather than hiding.
    pub hit_pass_limit: bool,
}

/// Apply fixes repeatedly until the buffer stops changing.
///
/// A single pass cannot always finish the job. Overlapping edits are rejected
/// rather than merged, so when two findings want adjacent bytes -- two unused
/// CTEs in one `WITH`, whose deletions share the comma between them -- the
/// later edit is dropped and would still be pending afterwards. Returning
/// "3 skipped as overlapping, re-run to apply" puts the tool's own bookkeeping
/// on the user, and re-running `--fix` on fixed output must produce no
/// edits. Converging here satisfies both, within [`MAX_PASSES`].
pub fn apply_until_stable(src: &str, select: Option<&[String]>, safety: Safety) -> Converged {
    apply_until_stable_with(
        src,
        &crate::analysis::Selection {
            select,
            ..crate::analysis::Selection::default()
        },
        safety,
    )
}

/// [`apply_until_stable`] over a full [`crate::analysis::Selection`].
pub fn apply_until_stable_with(
    src: &str,
    selection: &crate::analysis::Selection<'_>,
    safety: Safety,
) -> Converged {
    let mut current: Option<String> = None;
    let mut applied = 0usize;
    let mut passes = 0usize;

    for _ in 0..MAX_PASSES {
        let text: &str = current.as_deref().unwrap_or(src);
        let findings = crate::analysis::analyze(text, selection);
        let step = apply(text, &findings, safety);
        let Some(out) = step.output else {
            return Converged {
                output: current,
                applied,
                passes,
                hit_pass_limit: false,
            };
        };
        applied += step.applied;
        passes += 1;
        current = Some(out);
    }

    // Bound reached: is there still work pending?
    let text: &str = current.as_deref().unwrap_or(src);
    let findings = crate::analysis::analyze(text, selection);
    let pending = apply(text, &findings, safety).applied > 0;
    Converged {
        output: current,
        applied,
        passes,
        hit_pass_limit: pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze_source;

    fn fix_once(src: &str, safety: Safety) -> Applied {
        let findings = analyze_source(src, None);
        apply(src, &findings, safety)
    }

    #[test]
    fn rewrites_count_star() {
        let got = fix_once("SELECT count(*) FROM t;", Safety::SafeOnly);
        assert_eq!(got.applied, 1);
        assert_eq!(got.output.as_deref(), Some("SELECT count() FROM t;"));
    }

    #[test]
    fn nothing_to_do_returns_no_output() {
        let got = fix_once("SELECT a FROM t;", Safety::SafeOnly);
        assert!(got.output.is_none());
        assert_eq!(got.applied, 0);
    }

    #[test]
    fn several_edits_apply_in_one_pass() {
        // Two statements, each with its own safe fix. Applying the first must
        // not invalidate the second's byte range.
        let src = "SELECT count(*) FROM t;\nSELECT count(*) FROM u;";
        let got = fix_once(src, Safety::SafeOnly);
        assert_eq!(got.applied, 2);
        assert_eq!(
            got.output.as_deref(),
            Some("SELECT count() FROM t;\nSELECT count() FROM u;")
        );
    }

    #[test]
    fn a_suppressed_finding_is_never_fixed() {
        // Suppression has to gate the fix, not merely the report -- silently
        // rewriting a line the user asked us to leave alone is the worst
        // possible reading of "ignore".
        let src = "-- grebe: ignore[MOD001]\nSELECT count(*) FROM t;";
        let got = fix_once(src, Safety::SafeOnly);
        assert!(got.output.is_none(), "suppressed finding was rewritten");
    }

    #[test]
    fn unsafe_fixes_need_the_flag() {
        // MOD002 null-comparison is registry `Unsafe`.
        let src = "SELECT a FROM t WHERE b = NULL;";
        let safe = fix_once(src, Safety::SafeOnly);
        let unsafe_ = fix_once(src, Safety::IncludeUnsafe);
        assert!(safe.output.is_none(), "unsafe fix applied without the flag");
        assert!(unsafe_.output.is_some(), "unsafe fix did not apply with it");
    }

    #[test]
    fn overlapping_edits_drop_the_later_one() {
        use grebe_syntax::Span;
        let src = "abcdef";
        let mk = |s: u32, e: u32, r: &str| Finding {
            code: "MOD001", // registry Safe
            span: Span::new(s, e),
            fix: Some(Fix {
                span: Span::new(s, e),
                replacement: r.to_string(),
            }),
        };
        let got = apply(src, &[mk(0, 3, "X"), mk(2, 5, "Y")], Safety::SafeOnly);
        assert_eq!(got.applied, 1);
        assert_eq!(got.skipped_overlapping, 1);
        assert_eq!(got.output.as_deref(), Some("Xdef"));
    }

    #[test]
    fn converges_when_adjacent_edits_overlap() {
        // Two unused CTEs in one WITH. MOD016 deletes an entry plus one
        // adjacent comma, so the two deletions share the comma between them
        // and a single pass drops one of them. The TPC-H/TPC-DS and
        // documentation corpus contains no such statement, so only a test
        // like this one covers it.
        let src = "WITH a AS (SELECT 1), b AS (SELECT 2) SELECT 9;";
        let one = apply(src, &analyze_source(src, None), Safety::SafeOnly);
        assert_eq!(one.skipped_overlapping, 1, "expected an overlap to drop");

        let done = apply_until_stable(src, None, Safety::SafeOnly);
        let out = done.output.expect("converged output");
        assert!(!done.hit_pass_limit);
        assert!(done.passes > 1, "should have needed more than one pass");
        // Both CTEs are gone.
        assert!(!out.contains("WITH"), "left a WITH behind: {out}");
        // And the result is stable.
        let again = apply_until_stable(&out, None, Safety::SafeOnly);
        assert!(again.output.is_none(), "still changing: {:?}", again.output);
    }

    #[test]
    fn convergence_is_a_no_op_when_there_is_nothing_to_do() {
        let done = apply_until_stable("SELECT a FROM t;", None, Safety::SafeOnly);
        assert!(done.output.is_none());
        assert_eq!(done.applied, 0);
        assert_eq!(done.passes, 0);
    }

    #[test]
    fn reaches_a_fixed_point() {
        // Re-running `--fix` on fixed output produces no edits.
        let src = "SELECT count(*) FROM t;\nSELECT count(*) FROM u;";
        let first = fix_once(src, Safety::SafeOnly).output.expect("first pass");
        let second = fix_once(&first, Safety::SafeOnly);
        assert!(
            second.output.is_none(),
            "second pass still edited: {:?}",
            second.output
        );
    }

    #[test]
    fn output_is_still_valid_sql() {
        let src = "SELECT count(*) FROM t;";
        let out = fix_once(src, Safety::SafeOnly).output.unwrap();
        assert!(
            grebe_syntax::matcher::parse(&out).is_some(),
            "fix produced unparseable SQL: {out}"
        );
    }
}
