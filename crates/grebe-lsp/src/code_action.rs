//! Quick fixes — offering in the editor what `grebe check --fix` applies.
//!
//! Every fixable finding carries a `Fix`, and `--fix` applies them. Without
//! code actions a user reading a squiggle would have to leave, run
//! `grebe check PATH --fix`, and come back. The actions here are built from
//! those same `Fix` values, so the editor and the CLI cannot disagree about
//! what a fix *is*.
//!
//! A fix stays a byte-range replacement on the original buffer until this
//! module turns it into a `TextEdit`, the same boundary conversion
//! diagnostics use. Each action carries exactly one finding's fix, so no
//! overlap resolution or repeat-until-stable loop is needed here; those
//! belong to the bulk `--fix` path.
//!
//! # Safe and unsafe are both offered, and labelled
//!
//! The CLI defaults to the safe band and requires `--unsafe` to widen, because
//! it rewrites files in bulk with nobody watching. A code action is the
//! opposite situation: one finding, chosen deliberately, with the diff visible
//! and undo one keystroke away. Hiding unsafe fixes there would not protect
//! anyone — it would just send them to the CLI flag that rewrites *everything*
//! unsafe at once, which is strictly worse. So both are offered and the unsafe
//! ones say so in their title.

use grebe_rules::detect::Finding;
use grebe_rules::registry::FixSafety;
use grebe_syntax::Span;

use crate::json::Json;
use crate::position::{self, Encoding};

/// Build the `textDocument/codeAction` result for `range` over `src`.
///
/// `range` is a byte span already converted from the client's line/character
/// positions. Findings are offered when they *overlap* it, not when they are
/// contained by it: an editor sends the cursor position or the selection, and
/// a zero-width cursor sitting inside a finding's span contains nothing at all.
pub fn actions(
    uri: &str,
    src: &str,
    findings: &[Finding],
    range: Span,
    enc: Encoding,
) -> Vec<Json> {
    findings
        .iter()
        .filter(|f| overlaps(f.span, range))
        .filter_map(|f| action_for(uri, src, f, enc))
        .collect()
}

/// Half-open overlap, with zero-width ranges (a bare cursor) treated as
/// touching the span they sit inside.
fn overlaps(a: Span, b: Span) -> bool {
    if b.start == b.end {
        return b.start >= a.start && b.start <= a.end;
    }
    a.start < b.end && b.start < a.end
}

fn action_for(uri: &str, src: &str, f: &Finding, enc: Encoding) -> Option<Json> {
    let fix = f.fix.as_ref()?;
    let rule = grebe_rules::lookup(f.code)?;
    let safety = rule.fix_safety;
    if safety == FixSafety::None {
        return None;
    }

    let title = match safety {
        // Naming the rule, not just the action, is what makes a list of three
        // quick fixes legible when three findings overlap one line.
        FixSafety::Safe => format!("{}: fix {}", f.code, rule.name),
        FixSafety::Unsafe => format!(
            "{}: fix {} (unsafe — may change results)",
            f.code, rule.name
        ),
        FixSafety::None => unreachable!("filtered above"),
    };

    let (sl, sc) = position::byte_to_position(src, fix.span.start, enc);
    let (el, ec) = position::byte_to_position(src, fix.span.end, enc);
    let edit = Json::object(vec![
        (
            "range".into(),
            Json::object(vec![
                ("start".into(), pos(sl, sc)),
                ("end".into(), pos(el, ec)),
            ]),
        ),
        ("newText".into(), Json::str(&fix.replacement)),
    ]);

    Some(Json::object(vec![
        ("title".into(), Json::str(&title)),
        // `quickfix` is what puts it under the lightbulb next to the squiggle.
        ("kind".into(), Json::str("quickfix")),
        (
            "edit".into(),
            Json::object(vec![(
                "changes".into(),
                Json::object(vec![(uri.to_string(), Json::Array(vec![edit]))]),
            )]),
        ),
    ]))
}

fn pos(line: u32, character: u32) -> Json {
    Json::object(vec![
        ("line".into(), Json::num(f64::from(line))),
        ("character".into(), Json::num(f64::from(character))),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use grebe_rules::analysis::analyze_source;

    fn titles(src: &str, range: Span) -> Vec<String> {
        let findings = analyze_source(src, None);
        actions("file:///t.sql", src, &findings, range, Encoding::Utf16)
            .iter()
            .filter_map(|a| a.get("title").and_then(Json::as_str).map(str::to_string))
            .collect()
    }

    #[test]
    fn offers_a_safe_fix_for_a_finding_under_the_cursor() {
        let src = "SELECT count(*) FROM t;";
        let at = src.find('*').unwrap() as u32;
        let got = titles(src, Span::new(at, at)); // zero-width: a bare cursor
        assert_eq!(got, vec!["MOD001: fix count-star"]);
    }

    #[test]
    fn offers_nothing_away_from_any_finding() {
        let src = "SELECT count(*) FROM t;";
        let end = src.len() as u32;
        assert!(titles(src, Span::new(end, end)).is_empty());
    }

    #[test]
    fn an_unsafe_fix_is_offered_and_says_so() {
        // MOD002 null-comparison is registry `Unsafe`. An editor shows the
        // diff before applying, so the useful thing is to offer it labelled --
        // not to hide it and push the user to `--fix --unsafe`, which rewrites
        // every unsafe finding in the tree at once.
        let src = "SELECT a FROM t WHERE b = NULL;";
        let at = src.find("= NULL").unwrap() as u32;
        let got = titles(src, Span::new(at, at));
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("unsafe"), "{got:?}");
    }

    #[test]
    fn a_rule_with_no_fix_offers_no_action() {
        // MOD007 natural-join has `fix_safety: None`.
        let src = "SELECT * FROM a NATURAL JOIN b;";
        let at = src.find("NATURAL").unwrap() as u32;
        assert!(titles(src, Span::new(at, at)).is_empty());
    }

    #[test]
    fn a_selection_covering_several_findings_offers_each() {
        let src = "SELECT count(*), ifnull(a, 0) FROM t;";
        let got = titles(src, Span::new(0, src.len() as u32));
        assert!(got.iter().any(|t| t.starts_with("MOD001")), "{got:?}");
        assert!(got.iter().any(|t| t.starts_with("MOD006")), "{got:?}");
    }

    #[test]
    fn a_suppressed_finding_offers_no_action() {
        // Suppression is applied by `analyze_source`, so the finding never
        // reaches here -- the editor and the CLI agree by construction.
        let src = "-- grebe: ignore[MOD001]\nSELECT count(*) FROM t;";
        let at = src.find('*').unwrap() as u32;
        assert!(titles(src, Span::new(at, at)).is_empty());
    }

    #[test]
    fn the_edit_replaces_exactly_the_fix_span() {
        let src = "SELECT count(*) FROM t;";
        let at = src.find('*').unwrap() as u32;
        let findings = analyze_source(src, None);
        let acts = actions(
            "file:///t.sql",
            src,
            &findings,
            Span::new(at, at),
            Encoding::Utf16,
        );
        let edits = acts[0]
            .get("edit")
            .and_then(|e| e.get("changes"))
            .and_then(|c| c.get("file:///t.sql"))
            .and_then(Json::as_array)
            .expect("changes for the uri");
        let r = edits[0].get("range").unwrap();
        let sc = r.get("start").unwrap().get("character").unwrap();
        let ec = r.get("end").unwrap().get("character").unwrap();
        // MOD001 deletes the star: a zero-length replacement over one column.
        assert_eq!(ec.as_f64().unwrap() - sc.as_f64().unwrap(), 1.0);
        assert_eq!(edits[0].get("newText").and_then(Json::as_str), Some(""));
    }
}
