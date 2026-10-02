//! Recorded divergences from a real DuckDB engine.
//!
//! These are deliberate and ruled on, not bugs waiting to be fixed. The test
//! pins current behaviour so a future grammar refresh or tokenizer change that
//! silently alters one shows up as a failure rather than as a quiet drift in
//! the differential's disagreement count.

use grebe_syntax::matcher::parse_check;

#[test]
fn recorded_divergences_hold() {
    let src = include_str!("fixtures/known-divergences.sql");
    let mut expect: Option<bool> = None;
    let mut checked = 0;
    for line in src.lines() {
        let t = line.trim();
        match t {
            "-- expect: REJECT" => expect = Some(false),
            "-- expect: PARSE" => expect = Some(true),
            _ if t.starts_with("--") || t.is_empty() => {}
            _ => {
                if let Some(want) = expect.take() {
                    let (got, _) = parse_check(t);
                    assert_eq!(got, want, "divergence changed for: {t}");
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 2, "expected 2 recorded divergences, saw {checked}");
}
