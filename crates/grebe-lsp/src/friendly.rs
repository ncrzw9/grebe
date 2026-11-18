//! Rewrite as friendly SQL: what `grebe check --fix --select FRIENDLY`
//! does to a file, done to a document in the editor, then formatted.
//!
//! The rules are [`grebe_rules::FRIENDLY`] and nothing else: the editor's
//! `grebe.select` and a `grebe.toml`'s `select` say which findings to show,
//! not what this rewrite means. Suppression comments still hold, since a
//! suppressed finding is never fixed.
//!
//! Two reaches:
//!
//! - the whole document: every fix, then the document is formatted;
//! - some statements (a selection): each is fixed and, if that changed it,
//!   formatted on its own. A statement the fixes leave alone is not touched,
//!   and neither is anything outside the statements asked for.
//!
//! Either way nothing is returned when no fix applied: asking for friendly
//! SQL that is already friendly does not reformat it.
//!
//! Fixes are applied the way `--fix` applies them ([`grebe_rules::fix`]):
//! safe ones only, overlaps dropped and picked up by the next pass, each
//! edit checked to keep comments and keep the statement parsing, repeated
//! until a pass changes nothing.

use grebe_rules::analysis::{Selection, analyze};
use grebe_rules::detect::Finding;
use grebe_rules::fix::{MAX_PASSES, Safety, apply};
use grebe_syntax::Span;

use crate::json::Json;

/// The code action kind of the rewrite. A `source` action: it is asked for
/// by kind (a "source action" menu, or on save), never shown under a
/// lightbulb. Deliberately not under `source.fixAll`: a client set to fix
/// everything on save asks for that kind and would get a change of style
/// nobody chose.
pub const ACTION_KIND: &str = "source.grebe.friendly";

/// Did the client ask for [`ACTION_KIND`]? `only` is the request's
/// `context.only`: a list of kinds, each of which also asks for the kinds
/// under it (`source` asks for `source.grebe.friendly`). No list asks for
/// nothing here.
#[must_use]
pub fn asked_for(only: Option<&Json>) -> bool {
    only.and_then(Json::as_array).is_some_and(|kinds| {
        kinds.iter().filter_map(Json::as_str).any(|kind| {
            ACTION_KIND
                .strip_prefix(kind)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
    })
}

/// The rewrite as a code action over `uri`, from its text `edits`. `None`
/// when there are none: an action that does nothing is not offered.
#[must_use]
pub fn action(uri: &str, edits: Vec<Json>) -> Option<Json> {
    if edits.is_empty() {
        return None;
    }
    Some(Json::object(vec![
        ("title".into(), Json::str("Rewrite as friendly SQL")),
        ("kind".into(), Json::str(ACTION_KIND)),
        (
            "edit".into(),
            Json::object(vec![(
                "changes".into(),
                Json::object(vec![(uri.to_string(), Json::Array(edits))]),
            )]),
        ),
    ]))
}

/// One replacement on the original document.
#[derive(Debug, PartialEq, Eq)]
pub struct Rewrite {
    pub span: Span,
    pub text: String,
}

/// Rewrite `src`. `statements` is `None` for the whole document, or the
/// statements to rewrite (spans of `src`, in source order, not overlapping).
/// `foreign_heads` is the config's, so what counts as another dialect's
/// statement is the same here as in the diagnostics.
#[must_use]
pub fn rewrite(
    src: &str,
    statements: Option<&[Span]>,
    foreign_heads: &[String],
    opts: &grebe_format::Options,
) -> Vec<Rewrite> {
    let select: Vec<String> = grebe_rules::FRIENDLY
        .iter()
        .map(|c| (*c).to_string())
        .collect();
    let selection = Selection {
        select: Some(&select),
        foreign_heads,
        ..Selection::default()
    };
    let len = u32::try_from(src.len()).unwrap_or(u32::MAX);
    let Some(statements) = statements else {
        let whole = Span::new(0, len);
        let Some((fixed, _)) = fix_within(src, whole, &selection) else {
            return Vec::new();
        };
        return vec![Rewrite {
            span: whole,
            text: grebe_format::format(&fixed, opts),
        }];
    };
    let mut out = Vec::new();
    for &st in statements {
        // Each statement starts from the original text: its fixes stay
        // inside it, so no other statement's span moves.
        let Some((fixed, end)) = fix_within(src, st, &selection) else {
            continue;
        };
        let text = &fixed[st.start as usize..end as usize];
        out.push(Rewrite {
            span: st,
            text: grebe_format::format(text, opts).trim_end().to_string(),
        });
    }
    out
}

/// Apply the selected fixes that lie inside `within`, until none is left.
/// The rewritten source and where `within` now ends, or `None` when nothing
/// applied.
///
/// The whole source is analyzed each pass, not the slice: a suppression
/// comment sits above its statement, outside the statement's own span.
fn fix_within(src: &str, within: Span, selection: &Selection<'_>) -> Option<(String, u32)> {
    let mut current: Option<String> = None;
    let mut end = within.end;
    for _ in 0..MAX_PASSES {
        let text = current.as_deref().unwrap_or(src);
        let findings: Vec<Finding> = analyze(text, selection)
            .into_iter()
            .filter(|f| {
                f.fix
                    .as_ref()
                    .is_some_and(|fix| fix.span.start >= within.start && fix.span.end <= end)
            })
            .collect();
        let Some(out) = apply(text, &findings, Safety::SafeOnly).output else {
            break;
        };
        // Every edit was inside the span, so the span's end moved by
        // exactly as much as the text grew or shrank.
        let after = i64::from(end) + out.len() as i64 - text.len() as i64;
        end = u32::try_from(after).unwrap_or(end);
        current = Some(out);
    }
    current.map(|text| (text, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(src: &str) -> Option<String> {
        let mut edits = rewrite(src, None, &[], &grebe_format::Options::default());
        assert!(edits.len() <= 1);
        edits.pop().map(|e| e.text)
    }

    /// The words of `sql`, so a test does not depend on line breaking.
    fn words(sql: &str) -> Vec<&str> {
        sql.split_whitespace().collect()
    }

    #[test]
    fn the_whole_document_is_fixed_then_formatted() {
        let src = "select region, count(*) from orders group by region;\nselect * from t;\n";
        let got = whole(src).expect("there is something to rewrite");
        assert_eq!(
            words(&got),
            words("FROM orders SELECT region, count() GROUP BY ALL; FROM t;")
        );
        // Formatted: what the formatter makes of it is what it already is.
        assert_eq!(
            grebe_format::format(&got, &grebe_format::Options::default()),
            got
        );
        // And nothing is left to rewrite.
        assert_eq!(whole(&got), None);
    }

    #[test]
    fn fixes_that_overlap_are_all_applied_in_the_end() {
        // The count(*) sits inside the text MOD041 moves, and the inner
        // query inside the outer one's FROM clause: one pass cannot take
        // them all.
        let src = "SELECT count(*) FROM (SELECT a FROM (SELECT b AS a FROM t));\n";
        let got = whole(src).unwrap();
        assert_eq!(
            words(&got),
            words("FROM (FROM (FROM t SELECT a: b) SELECT a) SELECT count();")
        );
    }

    #[test]
    fn friendly_sql_is_left_alone_even_when_unformatted() {
        assert_eq!(whole("from   t select a;\n"), None);
        // No friendly rule has anything to say about these.
        assert_eq!(whole("select 1;\n"), None);
    }

    #[test]
    fn only_friendly_rules_apply() {
        // MOD006 (ifnull) has a safe fix and MOD002 (= NULL) an unsafe one;
        // neither is friendly SQL, so both stay as written.
        let got = whole("select ifnull(a, 0) from t where a = null;\n").unwrap();
        assert_eq!(
            words(&got),
            words("FROM t SELECT ifnull(a, 0) WHERE a = NULL;")
        );
    }

    #[test]
    fn a_suppressed_finding_is_not_rewritten() {
        let src = "-- grebe: ignore[MOD041]\nSELECT a FROM t;\n";
        assert_eq!(whole(src), None);
    }

    #[test]
    fn a_comment_in_the_way_keeps_the_statement_as_written() {
        let src = "SELECT a, -- the key\n    b\nFROM t;\n";
        assert_eq!(whole(src), None);
    }

    #[test]
    fn a_statement_that_does_not_parse_does_not_stop_the_rest() {
        let src = "SELECT FROM WHERE;\nSELECT a FROM t;\n";
        let got = whole(src).unwrap();
        assert_eq!(words(&got), words("SELECT FROM WHERE; FROM t SELECT a;"));
    }

    #[test]
    fn chosen_statements_are_rewritten_and_the_rest_is_not_touched() {
        let src = "select   a from t;\nselect b from u;\n-- grebe: ignore\nselect c from v;\n";
        let spans = crate::statements::all(src);
        assert_eq!(spans.len(), 3);
        // The second and third: the third is suppressed.
        let edits = rewrite(
            src,
            Some(&spans[1..]),
            &[],
            &grebe_format::Options::default(),
        );
        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].span, spans[1]);
        assert_eq!(words(&edits[0].text), words("FROM u SELECT b;"));
        assert_eq!(edits[0].text, edits[0].text.trim(), "no stray whitespace");
    }

    #[test]
    fn a_chosen_statement_with_nothing_to_fix_is_not_reformatted() {
        let src = "select   1;\nfrom   t select   a;\n";
        let spans = crate::statements::all(src);
        let edits = rewrite(src, Some(&spans), &[], &grebe_format::Options::default());
        assert_eq!(edits, vec![]);
    }
}
