//! One analysis pipeline, shared by every front end.
//!
//! Whole-file parse as the fast path, per-statement fallback so one bad
//! statement does not discard the rest of the file's findings, span offsetting
//! on the fallback path, `--select`/severity filtering, and suppression. A
//! linter whose editor and CLI disagree about a file is worse than one that is
//! merely incomplete, so this is one function that `grebe check` and the LSP
//! both call, rather than two copies kept in step by hand.
//!
//! MOD rules only ever see statements that parse. A statement the matcher
//! rejects gets exactly one `PRS`/`SRC` finding saying why it was not linted,
//! and no MOD findings on top of it.
//!
//! Everything it returns is in **file coordinates**, fix spans included, so a
//! caller never has to know whether the fast path or the fallback produced a
//! given finding.

use crate::Severity;
use crate::detect::Finding;
use crate::suppress::Suppressions;
use grebe_syntax::Span;

/// Which rules run, at what severity, and which extra statement heads count
/// as foreign — the three things a `grebe.toml` or a `--select` can change.
///
/// `select` mirrors `--select`: `Some(codes)` runs only those `MOD` rules,
/// on or off by default alike; `None` runs every `MOD` rule whose
/// *effective* severity (a `[severity]` override, else the registry default)
/// is not `Off`. `select` wins over `severity` for the question "does it
/// run": naming a code is the most explicit thing a user can do, and it
/// still fires at its overridden severity.
///
/// `PRS` and `SRC` codes are outside the selection. They report that a
/// statement could not be analysed at all, so choosing which opinions to
/// run must never hide them; they follow their severity, and a `[severity]`
/// entry of `off` is the way to silence one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Selection<'a> {
    pub select: Option<&'a [String]>,
    /// `(code, severity)` overrides, codes uppercase.
    pub severity: &'a [(String, Severity)],
    /// Extra foreign-dialect heads for `SRC003`, uppercase, space-separated.
    pub foreign_heads: &'a [String],
}

impl Selection<'_> {
    /// The severity `code` reports at: an override if there is one, else the
    /// registry default. Unknown codes report `Off`.
    #[must_use]
    pub fn severity_of(&self, code: &str) -> Severity {
        if let Some((_, s)) = self
            .severity
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(code))
        {
            return *s;
        }
        crate::lookup(code).map_or(Severity::Off, |r| r.default_severity)
    }

    /// Does `code` run at all?
    #[must_use]
    pub fn enabled(&self, code: &str) -> bool {
        match self.select {
            Some(sel) if code.starts_with("MOD") => {
                sel.iter().any(|c| c.eq_ignore_ascii_case(code))
            }
            _ => self.severity_of(code) != Severity::Off,
        }
    }
}

/// [`analyze`] with only a `--select`; every other setting is the default.
pub fn analyze_source(src: &str, select: Option<&[String]>) -> Vec<Finding> {
    analyze(
        src,
        &Selection {
            select,
            ..Selection::default()
        },
    )
}

/// Analyze one source buffer into findings, in file coordinates.
///
/// Suppression comments are applied here, so no caller can forget them.
pub fn analyze(src: &str, selection: &Selection<'_>) -> Vec<Finding> {
    let sup = Suppressions::for_source(src);
    let keep = |code: &str, at: u32| selection.enabled(code) && !sup.is_suppressed(code, at);

    let mut findings: Vec<Finding> = Vec::new();

    // Whole-file parse is the fast path.
    if let Some(tree) = grebe_syntax::matcher::parse(src) {
        for f in crate::detect::analyze(&tree, src) {
            if keep(f.code, f.span.start) {
                findings.push(f);
            }
        }
    } else {
        // One statement we cannot parse must not cost us the rest of the file.
        for sp in grebe_syntax::token::split_statements(src) {
            let stmt = &src[sp.start as usize..sp.end as usize];
            match grebe_syntax::matcher::parse(stmt) {
                Some(tree) => {
                    for f in crate::detect::analyze(&tree, stmt) {
                        let at = sp.start + f.span.start;
                        if !keep(f.code, at) {
                            continue;
                        }
                        findings.push(Finding {
                            code: f.code,
                            span: Span::new(at, sp.start + f.span.end),
                            // The fix span needs the same offset as the finding
                            // span; offsetting one and not the other corrupts
                            // the file when `--fix` runs on a buffer whose
                            // whole-file parse failed.
                            fix: f.fix.map(|fix| crate::detect::Fix {
                                span: Span::new(sp.start + fix.span.start, sp.start + fix.span.end),
                                replacement: fix.replacement,
                            }),
                        });
                    }
                }
                // A rejected statement is classified, not dropped: PRS001 for a
                // syntax error, or an SRC code when we can say why we declined
                // to lint it. These spans are already file-absolute.
                None => {
                    for f in crate::source::classify_unparsed_with(
                        stmt,
                        sp.start,
                        selection.foreign_heads,
                    ) {
                        if keep(f.code, f.span.start) {
                            findings.push(f);
                        }
                    }
                }
            }
        }
    }

    findings.sort_by_key(|f| f.span.start);
    findings
}

/// How many statements in `src` the matcher rejects.
///
/// Reported separately by `grebe check` because "did not parse" is a fact about
/// the file, not a finding, and stays true even when `PRS001` is suppressed.
pub fn unparsed_count(src: &str) -> usize {
    if grebe_syntax::matcher::parse(src).is_some() {
        return 0;
    }
    grebe_syntax::token::split_statements(src)
        .into_iter()
        .filter(|sp| {
            grebe_syntax::matcher::parse(&src[sp.start as usize..sp.end as usize]).is_none()
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_rule_on_the_fast_path() {
        let f = analyze_source("SELECT count(*) FROM t;", None);
        assert!(f.iter().any(|f| f.code == "MOD001"));
    }

    #[test]
    fn a_bad_statement_does_not_swallow_its_siblings() {
        let src = "THIS IS NOT SQL AT ALL;\nSELECT count(*) FROM t;";
        let f = analyze_source(src, None);
        assert!(f.iter().any(|f| f.code == "MOD001"));
        assert!(f.iter().any(|f| f.code == "PRS001"));
    }

    #[test]
    fn fallback_spans_are_file_absolute() {
        let bad = "THIS IS NOT SQL AT ALL";
        let src = format!("{bad};\nSELECT count(*) FROM t;");
        let f = analyze_source(&src, None);
        let m = f.iter().find(|f| f.code == "MOD001").expect("MOD001");
        assert_eq!(&src[m.span.start as usize..m.span.end as usize], "*");
        assert!(m.span.start as usize > bad.len());
    }

    #[test]
    fn fallback_fix_spans_are_file_absolute_too() {
        // MOD001 carries a safe fix. On the fallback path its fix span must be
        // offset with the finding span, or an applied fix lands at the wrong
        // byte range entirely.
        let bad = "THIS IS NOT SQL AT ALL";
        let src = format!("{bad};\nSELECT count(*) FROM t;");
        let f = analyze_source(&src, None);
        let m = f.iter().find(|f| f.code == "MOD001").expect("MOD001");
        let fix = m.fix.as_ref().expect("MOD001 has a fix");
        assert_eq!(&src[fix.span.start as usize..fix.span.end as usize], "*");
    }

    #[test]
    fn selection_restricts_and_enables() {
        let src = "SELECT * FROM t;";
        assert!(analyze_source(src, None).is_empty());
        let sel = vec!["MOD025".to_string()];
        assert!(
            analyze_source(src, Some(&sel))
                .iter()
                .any(|f| f.code == "MOD025")
        );
    }

    #[test]
    fn suppression_is_applied_here_so_no_caller_can_forget_it() {
        let src = "-- grebe: ignore[MOD001]\nSELECT count(*) FROM t;";
        assert!(analyze_source(src, None).is_empty());
    }

    #[test]
    fn a_severity_override_turns_a_rule_on_or_off() {
        let src = "SELECT * FROM t;";
        let on = vec![("MOD025".to_string(), Severity::Warning)];
        let sel = Selection {
            severity: &on,
            ..Selection::default()
        };
        assert!(analyze(src, &sel).iter().any(|f| f.code == "MOD025"));
        assert_eq!(sel.severity_of("MOD025"), Severity::Warning);

        let off = vec![("MOD001".to_string(), Severity::Off)];
        let sel = Selection {
            severity: &off,
            ..Selection::default()
        };
        assert!(analyze("SELECT count(*) FROM t;", &sel).is_empty());
    }

    #[test]
    fn select_wins_over_an_off_override() {
        let off = vec![("MOD001".to_string(), Severity::Off)];
        let codes = vec!["MOD001".to_string()];
        let sel = Selection {
            select: Some(&codes),
            severity: &off,
            foreign_heads: &[],
        };
        assert!(
            analyze("SELECT count(*) FROM t;", &sel)
                .iter()
                .any(|f| f.code == "MOD001")
        );
    }

    #[test]
    fn a_selection_never_hides_parse_or_dialect_findings() {
        let codes = vec!["MOD001".to_string()];
        let f = analyze_source("SELEC 1;\nSELECT count(*) FROM t;", Some(&codes));
        assert!(f.iter().any(|f| f.code == "PRS001"), "{f:?}");
        assert!(f.iter().any(|f| f.code == "MOD001"), "{f:?}");
        let f = analyze_source("{{ config(x) }} SELECT 1;", Some(&codes));
        assert!(f.iter().any(|f| f.code == "SRC002"), "{f:?}");
        // ...but a selection still restricts the MOD rules.
        let f = analyze_source("SELECT count(*) FROM t;", Some(&["MOD002".to_string()]));
        assert!(!f.iter().any(|f| f.code == "MOD001"), "{f:?}");
    }

    #[test]
    fn severity_off_silences_a_parse_finding() {
        let off = vec![("PRS001".to_string(), Severity::Off)];
        let sel = Selection {
            severity: &off,
            ..Selection::default()
        };
        assert!(analyze("SELEC 1;", &sel).iter().all(|f| f.code != "PRS001"));
    }

    #[test]
    fn extra_foreign_heads_reach_src003() {
        let heads = vec!["BEGIN TRAN".to_string()];
        let sel = Selection {
            foreign_heads: &heads,
            ..Selection::default()
        };
        let f = analyze("BEGIN TRAN;", &sel);
        assert!(f.iter().any(|f| f.code == "SRC003"), "{f:?}");
        assert!(
            analyze_source("BEGIN TRAN;", None)
                .iter()
                .any(|f| f.code == "PRS001")
        );
    }

    #[test]
    fn unparsed_count_counts_statements_not_files() {
        assert_eq!(unparsed_count("SELECT 1;"), 0);
        assert_eq!(unparsed_count("SELECT 1;\nNOT SQL AT ALL;"), 1);
    }
}
