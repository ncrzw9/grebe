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
