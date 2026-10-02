//! `--fix` against input designed to break it.
//!
//! Running `--fix` over a corpus of TPC-H/TPC-DS queries and examples from
//! DuckDB's documentation, with DuckDB checking the output, answers "does
//! this hold on ordinary SQL". It cannot answer "does this hold on SQL that
//! is awkward to rewrite": the corpus contains what it contains. It passes
//! even when MOD016's fix does not converge, since no corpus file declares
//! two unused CTEs in one `WITH`.
//!
//! `fixtures/fix-adversarial.sql` collects the awkward shapes. Every statement
//! in it must converge, still parse afterwards, and be stable under a re-run.

use grebe_rules::fix::{Safety, apply_until_stable};

/// Split the fixture on statement boundaries, keeping each statement's leading
/// comments with it — a suppression comment is part of its statement's input.
fn cases(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in src.lines() {
        let t = line.trim();
        // A blank line between entries ends the current case, unless we are
        // mid-statement (a case can span lines).
        if t.is_empty() {
            if current.trim_end().ends_with(';') {
                out.push(std::mem::take(&mut current));
            } else if !current.trim().is_empty() {
                current.push('\n');
            }
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out.into_iter()
        .filter(|c| c.lines().any(|l| !l.trim_start().starts_with("--")))
        .collect()
}

#[test]
fn every_adversarial_case_converges_and_stays_valid() {
    let src = include_str!("fixtures/fix-adversarial.sql");
    let cases = cases(src);
    assert!(cases.len() >= 12, "fixture lost cases: {}", cases.len());

    for safety in [Safety::SafeOnly, Safety::IncludeUnsafe] {
        for case in &cases {
            let done = apply_until_stable(case, None, safety);
            assert!(
                !done.hit_pass_limit,
                "{safety:?}: hit the pass limit (oscillating) on:\n{case}"
            );
            let Some(out) = done.output else {
                continue; // nothing to fix here
            };

            // Whatever we produced must still be SQL. The fixture's one
            // deliberately unparseable statement is paired with a valid one,
            // and a fix is only ever emitted for a statement that parsed, so
            // the *fixed* text must parse wherever the input did.
            if grebe_syntax::matcher::parse(case).is_some() {
                assert!(
                    grebe_syntax::matcher::parse(&out).is_some(),
                    "{safety:?}: fix produced unparseable SQL\n  in:  {case}\n  out: {out}"
                );
            }

            // And it must be stable: re-running changes nothing.
            let again = apply_until_stable(&out, None, safety);
            assert!(
                again.output.is_none(),
                "{safety:?}: not a fixed point\n  in:   {case}\n  out:  {out}\n  again: {:?}",
                again.output
            );
        }
    }
}

#[test]
fn a_suppressed_statement_is_never_rewritten() {
    let src = include_str!("fixtures/fix-adversarial.sql");
    let case = cases(src)
        .into_iter()
        .find(|c| c.contains("suppressed_t"))
        .expect("the suppression case");
    let done = apply_until_stable(&case, None, Safety::IncludeUnsafe);
    assert!(
        done.output.is_none(),
        "rewrote a suppressed statement: {:?}",
        done.output
    );
}
