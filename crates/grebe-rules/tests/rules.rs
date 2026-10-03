//! Rule fixtures: what each rule promises, as SQL.
//!
//! `fixtures/rules/<CODE>.sql` holds statements the rule must fire on and
//! near-misses it must stay silent on. `<CODE>.expected` lists every finding
//! that rule produces on the file, one per line: `line:col CODE`, then
//! ` => replacement` when the rule offers a fix. A detector change that adds,
//! drops or rewords a finding fails here until the expected file is updated,
//! which makes every change to a rule's behavior a reviewed diff.
//!
//! After a deliberate change, regenerate with
//! `GREBE_BLESS=1 cargo test -p grebe-rules --test rules` and review the diff.
//!
//! `setup.sql` is the data the fixtures query. It is not a fixture; it lets a
//! rule's fix be executed against the original and the results compared.

use std::fs;
use std::path::{Path, PathBuf};

use grebe_rules::analysis::analyze_source;

fn is_rule_code(stem: &str) -> bool {
    stem.len() == 6
        && stem[..3].bytes().all(|b| b.is_ascii_uppercase())
        && stem[3..].bytes().all(|b| b.is_ascii_digit())
}

fn line_col(src: &str, byte: usize) -> (usize, usize) {
    let before = &src[..byte];
    let line = before.matches('\n').count() + 1;
    let col = byte - before.rfind('\n').map_or(0, |i| i + 1) + 1;
    (line, col)
}

/// Every finding for `code` in `src`, in the `.expected` format. A fixture
/// that fails to parse is a broken fixture, not a silent rule.
fn render(path: &Path, src: &str, code: &str) -> String {
    let findings = analyze_source(src, Some(&[code.to_string()]));
    if let Some(prs) = findings.iter().find(|f| f.code.starts_with("PRS")) {
        let (line, col) = line_col(src, prs.span.start as usize);
        panic!("{}:{line}:{col}: fixture does not parse", path.display());
    }
    let mut out = String::new();
    for f in findings.iter().filter(|f| f.code == code) {
        let (line, col) = line_col(src, f.span.start as usize);
        out.push_str(&format!("{line}:{col} {}", f.code));
        if let Some(fix) = &f.fix {
            out.push_str(&format!(" => {}", fix.replacement));
        }
        out.push('\n');
    }
    out
}

#[test]
fn every_rule_fixture_matches_its_expected_findings() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rules");
    let bless = std::env::var_os("GREBE_BLESS").is_some();
    let mut fixtures: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("fixtures/rules exists")
        .map(|e| e.expect("readable entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "sql"))
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(is_rule_code)
        })
        .collect();
    fixtures.sort();
    assert!(
        !fixtures.is_empty(),
        "no rule fixtures found in {}",
        dir.display()
    );

    let mut failures = Vec::new();
    for sql_path in &fixtures {
        let code = sql_path.file_stem().and_then(|s| s.to_str()).unwrap();
        assert!(
            grebe_rules::lookup(code).is_some(),
            "{} names no registered rule",
            sql_path.display()
        );
        let src = fs::read_to_string(sql_path).expect("readable fixture");
        let actual = render(sql_path, &src, code);
        let expected_path = sql_path.with_extension("expected");
        if bless {
            fs::write(&expected_path, &actual).expect("writable expected file");
            continue;
        }
        let expected = fs::read_to_string(&expected_path).unwrap_or_else(|_| {
            panic!(
                "{} is missing; run with GREBE_BLESS=1 to create it",
                expected_path.display()
            )
        });
        if actual != expected {
            failures.push(format!(
                "{code}:\n--- expected\n{expected}--- actual\n{actual}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
