//! Integration tests: run the built `grebe` binary and inspect its exit
//! code, stdout/stderr and any files it wrote. Each test gets its own
//! throwaway directory under `std::env::temp_dir()`, removed on drop so a
//! failed run doesn't leave litter behind for the next one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A uniquely-named scratch directory, removed when it goes out of scope.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = format!(
            "grebe-cli-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.0.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&p, contents).expect("write fixture file");
        p
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join(name)).expect("read fixture file")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn grebe(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grebe"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run grebe")
}

fn grebe_stdin(dir: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_grebe"))
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grebe");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for grebe")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A statement the formatter is guaranteed to rewrite: lower-cased keywords
/// and a select list long enough that it must break one column per line
/// under the default `inline_threshold` of 100.
const MESSY_SQL: &str = "select alpha_column_name_long, beta_column_name_long, gamma_column_name_long, delta_column_name_long, epsilon_column_name_long from some_table;\n";

#[test]
fn format_rewrites_a_file_and_exits_zero() {
    let dir = TempDir::new("format-rewrite");
    dir.write("a.sql", MESSY_SQL);
    let expected = grebe_format::format(MESSY_SQL, &grebe_format::Options::default());
    assert_ne!(
        expected, MESSY_SQL,
        "fixture must actually need reformatting"
    );

    let out = grebe(dir.path(), &["format", "a.sql"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("reformatted a.sql"),
        "stdout: {}",
        stdout(&out)
    );
    assert_eq!(dir.read("a.sql"), expected);
}

#[test]
fn format_check_exits_one_and_leaves_the_file_alone() {
    let dir = TempDir::new("format-check");
    dir.write("a.sql", MESSY_SQL);

    let out = grebe(dir.path(), &["format", "--check", "a.sql"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("would reformat a.sql"),
        "stdout: {}",
        stdout(&out)
    );
    assert_eq!(dir.read("a.sql"), MESSY_SQL, "--check must not write");
}

#[test]
fn stdin_dash_round_trips() {
    let dir = TempDir::new("format-stdin");
    let expected = grebe_format::format(MESSY_SQL, &grebe_format::Options::default());

    let first = grebe_stdin(dir.path(), &["format", "-"], MESSY_SQL);
    assert!(first.status.success(), "stderr: {}", stderr(&first));
    assert_eq!(stdout(&first), expected);

    // Formatting is idempotent: feeding the output back in produces the
    // same output again, byte for byte.
    let second = grebe_stdin(dir.path(), &["format", "-"], &stdout(&first));
    assert!(second.status.success(), "stderr: {}", stderr(&second));
    assert_eq!(stdout(&second), stdout(&first));

    // --check on already-formatted stdin prints nothing and exits 0.
    let checked = grebe_stdin(dir.path(), &["format", "--check", "-"], &stdout(&first));
    assert!(checked.status.success());
    assert!(stdout(&checked).is_empty());
    assert!(stderr(&checked).is_empty());
}

#[test]
fn a_second_format_run_is_a_no_op() {
    let dir = TempDir::new("format-idempotent");
    dir.write("a.sql", MESSY_SQL);

    let first = grebe(dir.path(), &["format", "a.sql"]);
    assert!(first.status.success());

    let second = grebe(dir.path(), &["format", "a.sql"]);
    assert!(second.status.success(), "stderr: {}", stderr(&second));
    assert!(
        stdout(&second).is_empty(),
        "an unchanged file prints nothing: {}",
        stdout(&second)
    );
    assert!(
        stderr(&second).contains("0 file(s) reformatted"),
        "stderr: {}",
        stderr(&second)
    );
}

#[test]
fn check_json_output_parses_and_has_the_documented_keys() {
    let dir = TempDir::new("check-json");
    dir.write("a.sql", "SELECT count(*) FROM t;\n");

    let out = grebe(dir.path(), &["check", "--json", "a.sql"]);
    // MOD001 defaults to Info, not Error, so a clean parse with only this
    // finding must still exit 0.
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    let text = stdout(&out);
    let doc = grebe_lsp::json::parse(&text).unwrap_or_else(|e| panic!("{e}: {text:?}"));

    assert_eq!(
        doc.get("version").and_then(grebe_lsp::json::Json::as_str),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        doc.get("files").and_then(grebe_lsp::json::Json::as_f64),
        Some(1.0)
    );
    assert_eq!(
        doc.get("unparsed").and_then(grebe_lsp::json::Json::as_f64),
        Some(0.0)
    );
    assert_eq!(
        doc.get("fixed").and_then(grebe_lsp::json::Json::as_f64),
        Some(0.0)
    );

    let findings = doc
        .get("findings")
        .and_then(grebe_lsp::json::Json::as_array)
        .expect("findings array");
    let mod001 = findings
        .iter()
        .find(|f| f.get("code").and_then(grebe_lsp::json::Json::as_str) == Some("MOD001"))
        .expect("MOD001 finding");
    assert_eq!(
        mod001.get("path").and_then(grebe_lsp::json::Json::as_str),
        Some("a.sql")
    );
    assert_eq!(
        mod001.get("name").and_then(grebe_lsp::json::Json::as_str),
        Some("count-star")
    );
    assert_eq!(
        mod001
            .get("severity")
            .and_then(grebe_lsp::json::Json::as_str),
        Some("info")
    );
    assert!(mod001.get("line").is_some());
    assert!(mod001.get("column").is_some());
    assert!(mod001.get("start").is_some());
    assert!(mod001.get("end").is_some());
}

#[test]
fn grebe_toml_format_indent_size_changes_indentation_and_no_config_ignores_it() {
    let dir = TempDir::new("format-config");
    dir.write("grebe.toml", "[format]\nindent_size = 2\n");
    dir.write("a.sql", MESSY_SQL);

    let expected_indent_2 = grebe_format::format(
        MESSY_SQL,
        &grebe_format::Options {
            indent_size: 2,
            ..grebe_format::Options::default()
        },
    );
    let expected_default = grebe_format::format(MESSY_SQL, &grebe_format::Options::default());
    assert_ne!(
        expected_indent_2, expected_default,
        "fixture must show a visible indent difference"
    );

    let out = grebe(dir.path(), &["format", "a.sql"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(dir.read("a.sql"), expected_indent_2);

    // --no-config: the discovered grebe.toml must not apply.
    dir.write("a.sql", MESSY_SQL);
    let out = grebe(dir.path(), &["format", "--no-config", "a.sql"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(dir.read("a.sql"), expected_default);
}

#[test]
fn severity_override_promotes_a_finding_to_error() {
    let dir = TempDir::new("check-severity");
    dir.write("grebe.toml", "[severity]\nMOD001 = \"error\"\n");
    dir.write("a.sql", "SELECT count(*) FROM t;\n");

    // Without the config, MOD001 is Info: exit 0.
    let clean = TempDir::new("check-severity-baseline");
    clean.write("a.sql", "SELECT count(*) FROM t;\n");
    let baseline = grebe(clean.path(), &["check", "--no-config", "a.sql"]);
    assert!(baseline.status.success(), "stderr: {}", stderr(&baseline));

    // With it, MOD001 is promoted to error: exit 1.
    let out = grebe(dir.path(), &["check", "a.sql"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("MOD001"), "stdout: {}", stdout(&out));
}

#[test]
fn include_and_exclude_filter_a_directory_walk() {
    let dir = TempDir::new("check-include-exclude");
    dir.write("grebe.toml", "exclude = [\"skip.sql\"]\n");
    dir.write("keep.sql", "SELECT count(*) FROM t;\n");
    dir.write("skip.sql", "SELECT count(*) FROM t;\n");

    let out = grebe(dir.path(), &["check", "."]);
    let text = stdout(&out);
    assert!(text.contains("keep.sql"), "stdout: {text}");
    assert!(!text.contains("skip.sql"), "stdout: {text}");

    // A file named explicitly on the command line is always processed,
    // even if a config would otherwise exclude it.
    let out = grebe(dir.path(), &["check", "skip.sql"]);
    assert!(
        stdout(&out).contains("skip.sql"),
        "stdout: {}",
        stdout(&out)
    );
}

#[test]
fn a_bad_config_exits_two_with_path_and_line_in_the_message() {
    let dir = TempDir::new("check-bad-config");
    let config = dir.write("bad.toml", "[format]\nindent_size = 17\n");
    dir.write("a.sql", "SELECT 1;\n");

    let out = grebe(dir.path(), &["check", "--config", "bad.toml", "a.sql"]);
    assert_eq!(out.status.code(), Some(2), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains(config.to_string_lossy().as_ref()) || err.contains("bad.toml"),
        "stderr: {err}"
    );
    assert!(err.contains(":2:"), "stderr: {err}");
}

#[test]
fn many_files_report_in_path_order_and_deep_nesting_still_parses() {
    let dir = TempDir::new("parallel");
    // More files than cores, so several workers run; one is nested 200
    // levels deep, which must parse on a worker's stack as on the main one.
    let deep = format!("SELECT {}1{};\n", "(".repeat(200), ")".repeat(200));
    dir.write("a000.sql", &deep);
    for i in 1..40 {
        dir.write(&format!("a{i:03}.sql"), "SELECT count(*) FROM t;\n");
    }
    let out = grebe(dir.path(), &["check", "."]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("did not parse"), "{stderr}");
    let files: Vec<&str> = stdout.lines().filter_map(|l| l.split(':').next()).collect();
    let mut sorted = files.clone();
    sorted.sort_unstable();
    assert_eq!(files, sorted, "findings must come out in path order");
    assert_eq!(files.len(), 39, "{stdout}");

    let out = grebe(dir.path(), &["format", "--check", "."]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted, "format output must come out in path order");
}

#[test]
fn a_closed_stdout_ends_the_run_quietly() {
    let dir = TempDir::new("broken-pipe");
    // Far more output than a pipe buffer holds, so writes hit the closed end.
    dir.write("many.sql", &"SELECT count(*) FROM t;\n".repeat(20_000));
    let mut child = Command::new(env!("CARGO_BIN_EXE_grebe"))
        .args(["check", "."])
        .current_dir(dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run grebe");
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("wait for grebe");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.code(), Some(141), "{stderr}");
}

#[test]
fn select_all_runs_every_opt_in_rule() {
    let dir = TempDir::new("select-all");
    // A comma join: MOD010 (implicit-cross-join), Off by default.
    dir.write("a.sql", "SELECT * FROM a, b;\n");

    let out = grebe(dir.path(), &["check", "."]);
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("MOD010"),
        "MOD010 should stay quiet without --select"
    );

    let out = grebe(dir.path(), &["check", "--select", "ALL", "."]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MOD010"), "{stdout}");
    assert_eq!(out.status.code(), Some(0), "MOD010 is Info severity");
}

#[test]
fn select_all_combined_with_another_code_is_unknown() {
    let dir = TempDir::new("select-all-mixed");
    dir.write("a.sql", "SELECT 1;\n");
    let out = grebe(dir.path(), &["check", "--select", "ALL,MOD001", "."]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ALL"), "{stderr}");
}

#[test]
fn fix_with_select_friendly_rewrites_conventional_sql() {
    let dir = TempDir::new("select-friendly");
    let conventional = "\
WITH recent AS (SELECT id, region FROM orders WHERE placed > DATE '2024-01-01')
SELECT region, count(*) AS n FROM recent GROUP BY region ORDER BY region, n;
SELECT * FROM orders WHERE id = 1;
SELECT ifnull(a, 0) FROM t WHERE a = NULL;
SELECT a, b FROM t ORDER BY a, b;
";
    dir.write("a.sql", conventional);

    // Without --fix it only reports, and only the friendly rules.
    let out = grebe(
        dir.path(),
        &["check", "--no-config", "--select", "friendly", "a.sql"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for code in ["MOD001", "MOD003", "MOD013", "MOD022", "MOD023", "MOD041"] {
        assert!(stdout.contains(code), "{code} missing from:\n{stdout}");
    }
    for code in ["MOD002", "MOD006"] {
        assert!(!stdout.contains(code), "{code} is not friendly:\n{stdout}");
    }
    assert_eq!(dir.read("a.sql"), conventional);

    let out = grebe(
        dir.path(),
        &[
            "check",
            "--no-config",
            "--fix",
            "--select",
            "FRIENDLY",
            "a.sql",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        dir.read("a.sql"),
        "\
WITH recent AS (FROM orders SELECT id, region WHERE placed > DATE '2024-01-01')
FROM recent SELECT region, n: count() GROUP BY ALL ORDER BY region, n;
 FROM orders WHERE id = 1;
FROM t SELECT ifnull(a, 0) WHERE a = NULL;
FROM t SELECT a, b ORDER BY ALL;
"
    );

    // Nothing is left for a second run.
    let out = grebe(
        dir.path(),
        &["check", "--no-config", "--select", "FRIENDLY", "a.sql"],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "");

    // FRIENDLY can stand among other codes; ALL still cannot.
    let out = grebe(
        dir.path(),
        &[
            "check",
            "--no-config",
            "--select",
            "FRIENDLY,MOD006",
            "a.sql",
        ],
    );
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MOD006"), "{stdout}");

    // In a config file too.
    dir.write("grebe.toml", "select = [\"FRIENDLY\"]\n");
    dir.write("b.sql", "SELECT a FROM t;\n");
    let out = grebe(dir.path(), &["check", "--fix", "b.sql"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(dir.read("b.sql"), "FROM t SELECT a;\n");
}

#[test]
fn a_file_that_is_not_utf8_fails_the_run() {
    let dir = TempDir::new("not-utf8");
    std::fs::write(dir.path().join("bad.sql"), b"select 1;\n\xff\xfe;\n").expect("write");
    dir.write("good.sql", "select 1;\n");
    for args in [&["check", "."][..], &["format", "--check", "."][..]] {
        let out = grebe(dir.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains("bad.sql"), "{}", stderr(&out));
    }
}

// ------------------------------------------------------------- catalog --

const SHOP: &str = r#"[
{"database":"shop","schema":"main","table":"orders","column":"id","type":"BIGINT"},
{"database":"shop","schema":"main","table":"orders","column":"total","type":"DECIMAL(10,2)"}
]"#;

#[test]
fn check_without_a_catalog_reports_no_cat_findings() {
    let dir = TempDir::new("no-catalog");
    dir.write("q.sql", "SELECT o.totl FROM ordrs o;\n");
    let out = grebe(dir.path(), &["check", "q.sql"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}

#[test]
fn check_catalog_flag_reports_unknown_names_with_suggestions() {
    let dir = TempDir::new("catalog-flag");
    dir.write("catalog.json", SHOP);
    dir.write(
        "q.sql",
        "SELECT o.totl FROM orders o;\nSELECT * FROM ordrs;\nSELECT * FROM nothing_like_it;\n",
    );
    let out = grebe(dir.path(), &["check", "--catalog", "catalog.json", "q.sql"]);
    // Warnings: reported, and not a failing exit code.
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "q.sql:1:10: CAT002 unknown-column (did you mean total?)\n\
         q.sql:2:15: CAT001 unknown-table (did you mean orders?)\n\
         q.sql:3:15: CAT001 unknown-table\n"
    );

    // --fix never applies a suggestion, with or without --unsafe.
    let out = grebe(
        dir.path(),
        &[
            "check",
            "--catalog=catalog.json",
            "--fix",
            "--unsafe",
            "q.sql",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(dir.read("q.sql").contains("ordrs"));

    // --json carries the suggestion as a field of its own.
    let out = grebe(
        dir.path(),
        &["check", "--catalog", "catalog.json", "--json", "q.sql"],
    );
    let doc = grebe_lsp::json::parse(&stdout(&out)).expect("valid JSON");
    let findings = doc.get("findings").and_then(|f| f.as_array()).unwrap();
    assert_eq!(findings.len(), 3);
    assert_eq!(
        findings[0].get("suggestion").and_then(|s| s.as_str()),
        Some("total")
    );
    assert!(findings[2].get("suggestion").is_none());
}

#[test]
fn check_reads_the_catalog_a_config_names_and_the_flag_wins() {
    let dir = TempDir::new("catalog-config");
    dir.write("grebe.toml", "catalog = \"snap/catalog.json\"\n");
    std::fs::create_dir(dir.path().join("snap")).unwrap();
    dir.write("snap/catalog.json", SHOP);
    dir.write("other.json", "[]");
    dir.write("q.sql", "SELECT * FROM orders;\nSELECT * FROM ordrs;\n");
    let out = grebe(dir.path(), &["check", "q.sql"]);
    assert_eq!(
        stdout(&out),
        "q.sql:2:15: CAT001 unknown-table (did you mean orders?)\n"
    );
    // An empty catalog has no `orders` either.
    let out = grebe(dir.path(), &["check", "--catalog", "other.json", "q.sql"]);
    assert_eq!(stdout(&out).lines().count(), 2, "{}", stdout(&out));
    // --no-config: no catalog, no CAT.
    let out = grebe(dir.path(), &["check", "--no-config", "q.sql"]);
    assert_eq!(stdout(&out), "");
    // Severity is configurable like any rule's.
    dir.write(
        "grebe.toml",
        "catalog = \"snap/catalog.json\"\n[severity]\nCAT001 = \"error\"\n",
    );
    let out = grebe(dir.path(), &["check", "q.sql"]);
    assert_eq!(out.status.code(), Some(1));
    dir.write(
        "grebe.toml",
        "catalog = \"snap/catalog.json\"\n[severity]\nCAT001 = \"off\"\n",
    );
    let out = grebe(dir.path(), &["check", "q.sql"]);
    assert_eq!((out.status.code(), stdout(&out).as_str()), (Some(0), ""));
}

#[test]
fn a_catalog_that_cannot_be_read_is_a_usage_error() {
    let dir = TempDir::new("catalog-bad");
    dir.write("q.sql", "SELECT 1;\n");
    let out = grebe(dir.path(), &["check", "--catalog", "missing.json", "q.sql"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("could not read catalog missing.json"),
        "{}",
        stderr(&out)
    );
    dir.write("bad.json", "{\"tables\": []}");
    let out = grebe(dir.path(), &["check", "--catalog", "bad.json", "q.sql"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("is not a catalog snapshot"),
        "{}",
        stderr(&out)
    );
    let out = grebe(dir.path(), &["check", "--catalog"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn rules_lists_the_cat_rules() {
    let dir = TempDir::new("rules-cat");
    let out = grebe(dir.path(), &["rules"]);
    let text = stdout(&out);
    assert!(
        text.contains("CAT001") && text.contains("unknown-table"),
        "{text}"
    );
    assert!(
        text.contains("CAT002") && text.contains("unknown-column"),
        "{text}"
    );
}

/// `grebe catalog` runs the duckdb it is pointed at, read-only, without the
/// user's rc file, asking for JSON with agent mode off, and passes the
/// snapshot through. A stand-in script plays duckdb so the test needs no
/// engine; with `DUCKDB_CLI` set, a second test below runs the real one.
#[cfg(unix)]
#[test]
fn catalog_runs_the_given_duckdb_and_prints_its_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("catalog-cmd");
    dir.write("shop.duckdb", "");
    let fake = dir.write(
        "duckdb",
        "#!/bin/sh\n\
         echo \"$DUCKDB_AGENT_MODE|$1|$2|$3|$4|$5\" > args.txt\n\
         printf '%s' \"$6\" > query.txt\n\
         cat snapshot.json\n",
    );
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir.write("snapshot.json", SHOP);
    let out = grebe(
        dir.path(),
        &["catalog", "--duckdb", "./duckdb", "shop.duckdb"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), SHOP);
    assert_eq!(stderr(&out).trim(), "1 table(s) and view(s), 2 column(s)");
    assert_eq!(
        dir.read("args.txt").trim(),
        "0|-no-init|-readonly|shop.duckdb|-json|-c"
    );
    assert!(
        dir.read("query.txt")
            .contains("FROM duckdb_columns() WHERE NOT internal")
    );

    // What it prints is not a snapshot: say so, exit 2, print nothing.
    dir.write("snapshot.json", "duckdb agent mode on\n| a |\n");
    let out = grebe(
        dir.path(),
        &["catalog", "--duckdb", "./duckdb", "shop.duckdb"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stdout(&out), "");
    assert!(
        stderr(&out).contains("did not print a snapshot"),
        "{}",
        stderr(&out)
    );

    // No such database, no such duckdb, bad flags.
    let out = grebe(
        dir.path(),
        &["catalog", "--duckdb", "./duckdb", "nope.duckdb"],
    );
    assert_eq!(out.status.code(), Some(2));
    let out = grebe(
        dir.path(),
        &["catalog", "--duckdb", "./no-such-duckdb", "shop.duckdb"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("could not run"), "{}", stderr(&out));
    let out = grebe(dir.path(), &["catalog", "--what"]);
    assert_eq!(out.status.code(), Some(2));
    let out = grebe(dir.path(), &["catalog", "--help"]);
    assert_eq!(out.status.code(), Some(0));
}

/// The same against a real DuckDB CLI, when one is given (`DUCKDB_CLI`, as
/// the VS Code extension's tests take it): the snapshot of a real database
/// is one `grebe check --catalog` reads.
#[test]
fn catalog_snapshot_of_a_real_database_round_trips() {
    let Some(cli) = std::env::var_os("DUCKDB_CLI") else {
        return;
    };
    let cli = cli.to_string_lossy().to_string();
    let dir = TempDir::new("catalog-real");
    let made = Command::new(&cli)
        .current_dir(dir.path())
        .env("DUCKDB_AGENT_MODE", "0")
        .args([
            "-no-init",
            "shop.duckdb",
            "-c",
            "CREATE TABLE orders (id BIGINT, \"Order Date\" DATE); CREATE VIEW recent AS SELECT id FROM orders;",
        ])
        .output()
        .expect("run duckdb");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let out = grebe(dir.path(), &["catalog", "--duckdb", &cli, "shop.duckdb"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim(), "2 table(s) and view(s), 3 column(s)");
    dir.write("catalog.json", &stdout(&out));
    dir.write(
        "q.sql",
        "SELECT o.\"Order Date\", o.id, r.id FROM orders o, recent r;\nSELECT o.\"Order Dat\" FROM orders o;\n",
    );
    let out = grebe(dir.path(), &["check", "--catalog", "catalog.json", "q.sql"]);
    assert_eq!(
        stdout(&out),
        "q.sql:2:10: CAT002 unknown-column (did you mean \"Order Date\"?)\n"
    );

    // --with-metadata: the object form, which reads the same and says more.
    let out = grebe(
        dir.path(),
        &[
            "catalog",
            "--with-metadata",
            "--duckdb",
            &cli,
            "shop.duckdb",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim(), "2 table(s) and view(s), 3 column(s)");
    let text = stdout(&out);
    assert!(text.starts_with("{\"columns\": ["), "{text}");
    assert!(text.contains("\"nullable\":true"), "{text}");
    assert!(
        text.contains("\"table\":\"recent\",\"kind\":\"view\""),
        "{text}"
    );
    dir.write("catalog.json", &text);
    let out = grebe(dir.path(), &["check", "--catalog", "catalog.json", "q.sql"]);
    assert_eq!(
        stdout(&out),
        "q.sql:2:10: CAT002 unknown-column (did you mean \"Order Date\"?)\n"
    );
}

/// `--with-metadata` asks duckdb twice and puts the answers together.
#[cfg(unix)]
#[test]
fn catalog_with_metadata_prints_columns_and_tables() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("catalog-meta");
    dir.write("shop.duckdb", "");
    let fake = dir.write(
        "duckdb",
        "#!/bin/sh\n\
         case \"$6\" in\n\
         *duckdb_tables*) cat tables.json ;;\n\
         *is_nullable*) cat snapshot.json ;;\n\
         *) echo unexpected ;;\n\
         esac\n",
    );
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir.write("snapshot.json", SHOP);
    dir.write(
        "tables.json",
        "[{\"database\":\"shop\",\"schema\":\"main\",\"table\":\"orders\",\"kind\":\"table\",\
         \"rows\":12,\"primary_key\":[\"id\"],\"unique\":[],\"indexes\":null}]\n",
    );
    let out = grebe(
        dir.path(),
        &[
            "catalog",
            "--duckdb",
            "./duckdb",
            "--with-metadata",
            "shop.duckdb",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out).trim(), "1 table(s) and view(s), 2 column(s)");
    let text = stdout(&out);
    assert!(text.starts_with("{\"columns\": ["), "{text}");
    assert!(
        text.contains("\"tables\": [{\"database\":\"shop\""),
        "{text}"
    );
    assert!(text.trim_end().ends_with("}]}"), "{text}");
    // And `check` reads what it wrote.
    dir.write("catalog.json", &text);
    dir.write("q.sql", "SELECT * FROM ordrs;\n");
    let out = grebe(dir.path(), &["check", "--catalog", "catalog.json", "q.sql"]);
    assert_eq!(
        stdout(&out),
        "q.sql:1:15: CAT001 unknown-table (did you mean orders?)\n"
    );
}
