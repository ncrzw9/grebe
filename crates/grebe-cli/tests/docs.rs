//! Keeps the examples in `docs/` exact: each one is run against the built
//! `grebe` binary and its output compared with what the page shows.
//!
//! An example is an HTML comment holding the command line, then its code
//! blocks:
//!
//! ````markdown
//! <!-- example: grebe check --select MOD001 query.sql -->
//!
//! ```toml        (optional: written to grebe.toml)
//! ```sql         (the input: written to the .sql file the command names,
//!                 or sent to stdin when the command reads `-`; absent when
//!                 the command names neither)
//! ```text        (the output)
//! ````
//!
//! Commands joined with ` && ` run in turn, in one directory that also holds
//! every file under `docs/examples/`. An output block marked `sql` is the
//! file as the commands leave it, or stdout when the input came on stdin;
//! any other output block is what the commands print, stdout then stderr of
//! each. Output from DuckDB itself is marked `<!-- duckdb -->` and is not
//! run here.
//!
//! `GREBE_DOCS_BLESS=1 cargo test -p grebe --test docs` rewrites every
//! output block with what grebe prints now. `GREBE_DOCS_BIN=PATH` runs
//! another build, a release one say, instead of the test's own.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MARK: &str = "<!-- example: ";

/// One fenced code block: its language and its lines, with the line index
/// of its opening fence in the page.
struct Block {
    lang: String,
    body: String,
    open: usize,
    close: usize,
}

struct Example {
    page: String,
    line: usize,
    commands: Vec<Vec<String>>,
    config: Option<String>,
    input: Option<String>,
    output: Block,
}

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

/// The next fenced block at or after line `from`.
fn next_block(lines: &[&str], from: usize) -> Option<Block> {
    let open = (from..lines.len()).find(|&i| lines[i].starts_with("```"))?;
    let lang = lines[open].trim_start_matches('`').trim().to_string();
    let close = (open + 1..lines.len()).find(|&i| lines[i].starts_with("```"))?;
    let mut body = String::new();
    for l in &lines[open + 1..close] {
        body.push_str(l);
        body.push('\n');
    }
    Some(Block {
        lang,
        body,
        open,
        close,
    })
}

fn parse(page: &str, text: &str) -> Vec<Example> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(rest) = l.strip_prefix(MARK) else {
            continue;
        };
        let cmd = rest
            .strip_suffix(" -->")
            .unwrap_or_else(|| panic!("{page}:{}: unclosed example comment", i + 1));
        let commands: Vec<Vec<String>> = cmd
            .split(" && ")
            .map(|c| {
                let words: Vec<String> = c.split_whitespace().map(str::to_string).collect();
                assert_eq!(
                    words.first().map(String::as_str),
                    Some("grebe"),
                    "{page}:{}: a command must start with grebe",
                    i + 1
                );
                words[1..].to_vec()
            })
            .collect();
        let takes_input = commands
            .iter()
            .flatten()
            .any(|w| w == "-" || w.ends_with(".sql"));
        let mut at = i + 1;
        let mut take = || {
            let b = next_block(&lines, at)
                .unwrap_or_else(|| panic!("{page}:{}: example is missing a code block", i + 1));
            at = b.close + 1;
            b
        };
        let mut first = take();
        let config = if first.lang == "toml" {
            let c = first.body;
            first = take();
            Some(c)
        } else {
            None
        };
        let (input, output) = if takes_input {
            (Some(first.body), take())
        } else {
            (None, first)
        };
        out.push(Example {
            page: page.to_string(),
            line: i + 1,
            commands,
            config,
            input,
            output,
        });
    }
    out
}

/// Runs one example in its own directory and returns what it produced.
fn run(ex: &Example, dir: &Path) -> String {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("create example dir");
    for entry in std::fs::read_dir(docs_dir().join("examples")).expect("docs/examples") {
        let p = entry.expect("dir entry").path();
        std::fs::copy(&p, dir.join(p.file_name().expect("file name"))).expect("copy example file");
    }
    if let Some(c) = &ex.config {
        std::fs::write(dir.join("grebe.toml"), c).expect("write grebe.toml");
    }
    let stdin = ex.commands.iter().flatten().any(|w| w == "-");
    let file = ex
        .commands
        .iter()
        .flatten()
        .find(|w| w.ends_with(".sql"))
        .cloned();
    if let (Some(f), Some(input), false) = (&file, &ex.input, stdin) {
        std::fs::write(dir.join(f), input).expect("write input");
    }
    let mut printed = String::new();
    let mut stdout = String::new();
    for args in &ex.commands {
        let bin = std::env::var_os("GREBE_DOCS_BIN")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_grebe").into());
        let mut child = Command::new(bin)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn grebe");
        let mut pipe = child.stdin.take().expect("stdin");
        if stdin {
            pipe.write_all(ex.input.as_deref().unwrap_or("").as_bytes())
                .expect("write stdin");
        }
        drop(pipe);
        let o = child.wait_with_output().expect("wait for grebe");
        stdout = String::from_utf8_lossy(&o.stdout).into_owned();
        printed.push_str(&stdout);
        printed.push_str(&String::from_utf8_lossy(&o.stderr));
    }
    // The version grebe prints changes with every release; the docs show it
    // as 1.0.0, so a version bump alone does not fail the docs.
    let printed = printed.replace(
        &format!("\"version\":\"{}\"", env!("CARGO_PKG_VERSION")),
        "\"version\":\"1.0.0\"",
    );
    if ex.output.lang == "sql" {
        if stdin {
            return stdout;
        }
        let f = file.expect("an sql output needs a file");
        return std::fs::read_to_string(dir.join(f)).expect("read result");
    }
    printed
}

#[test]
fn docs_examples_are_exact() {
    let bless = std::env::var_os("GREBE_DOCS_BLESS").is_some();
    let mut pages: Vec<PathBuf> = std::fs::read_dir(docs_dir())
        .expect("docs/")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    pages.sort();
    let scratch = std::env::temp_dir().join(format!("grebe-docs-test-{}", std::process::id()));
    let failures: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = pages
            .iter()
            .map(|path| {
                let scratch = &scratch;
                s.spawn(move || {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    let text = std::fs::read_to_string(path).expect("read page");
                    let examples = parse(&name, &text);
                    let mut failures = Vec::new();
                    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
                    // Replace from the end so earlier line numbers hold.
                    let mut edits = Vec::new();
                    for (n, ex) in examples.iter().enumerate() {
                        let got = run(ex, &scratch.join(format!("{name}-{n}")));
                        if got != ex.output.body {
                            failures.push(format!(
                                "{}:{}: output differs\n--- documented\n{}--- grebe\n{}",
                                ex.page, ex.line, ex.output.body, got
                            ));
                            edits.push((ex.output.open, ex.output.close, got));
                        }
                    }
                    if bless && !edits.is_empty() {
                        for (open, close, got) in edits.into_iter().rev() {
                            lines.splice(open + 1..close, got.lines().map(str::to_string));
                        }
                        let mut new = lines.join("\n");
                        new.push('\n');
                        std::fs::write(path, new).expect("write page");
                    }
                    (examples.len(), failures)
                })
            })
            .collect();
        let mut total = 0;
        let mut all = Vec::new();
        for h in handles {
            let (n, f) = h.join().expect("page thread");
            total += n;
            all.extend(f);
        }
        assert!(total >= 100, "only {total} examples found in docs/");
        all
    });
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(
        failures.is_empty() || bless,
        "{} example(s) differ:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}
