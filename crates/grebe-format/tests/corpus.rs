//! The formatter's acceptance gate over the dogfood corpus: TPC-H/TPC-DS
//! queries and examples mined from DuckDB's documentation.
//!
//! For every corpus file:
//!
//! 1. **Idempotent:** `format(format(x)) == format(x)`.
//! 2. **Token-preserving:** the code tokens of the output equal the input's,
//!    up to the case of words, commas (a trailing comma may be dropped when a
//!    list collapses) and statement terminators (always added).
//!
//! And for every file that parses as a whole:
//!
//! 3. **Shape-preserving:** the output parses, to a tree with the same
//!    pre-order sequence of rule names.
//!
//! Files that do not parse as a whole take the per-statement path, which
//! has its own ways to slip (a comment on a piece boundary), so 1 and 2 are
//! checked there too.
//!
//! The corpus is not distributed; when `corpus/` is absent the test passes
//! vacuously and says so.
//!
//! It is `#[ignore]`d in the default run because three parses of 1,700 files
//! take ~90 s in a debug build and ~9 s in release. Run it in release:
//!
//!     cargo test --release -p grebe-format --test corpus -- --ignored

use grebe_format::{Options, format};
use grebe_syntax::token::{TokenKind, tokenize};

fn corpus_files() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus");
    let mut out = Vec::new();
    fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|x| x == "sql") {
                out.push(path);
            }
        }
    }
    walk(&root, &mut out);
    out.sort();
    out
}

/// Code tokens, normalised for comparison.
fn code_tokens(src: &str) -> Vec<String> {
    tokenize(src)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| {
            let s = &src[t.span.start as usize..t.span.end as usize];
            if t.kind == TokenKind::Word {
                s.to_ascii_lowercase()
            } else {
                s.to_string()
            }
        })
        .filter(|s| s != "," && s != ";")
        .collect()
}

fn shape(src: &str) -> Option<Vec<&'static str>> {
    let t = grebe_syntax::matcher::parse(src)?;
    // Zero-width nodes (an absent optional clause) carry no text and land at
    // an arena-dependent position in pre-order; they are not shape.
    Some(
        t.walk()
            .into_iter()
            .filter(|&n| !t.node(n).span.is_empty())
            .map(|n| t.rule_name(n))
            .collect(),
    )
}

#[test]
#[ignore = "corpus gate; needs corpus/, run in release"]
fn corpus_round_trip() {
    let files = corpus_files();
    if files.is_empty() {
        eprintln!("corpus/ absent; passing vacuously");
        return;
    }
    let opts = Options::default();
    let mut checked = 0usize;
    let mut whole = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for f in &files {
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        let before = shape(&src);
        checked += 1;
        let once = format(&src, &opts);
        let twice = format(&once, &opts);
        if once != twice {
            failures.push(format!("{}: not idempotent", f.display()));
            continue;
        }
        if code_tokens(&src) != code_tokens(&once) {
            failures.push(format!("{}: token stream changed", f.display()));
            continue;
        }
        let Some(before) = before else {
            continue;
        };
        whole += 1;
        match shape(&once) {
            None => failures.push(format!("{}: output does not parse", f.display())),
            Some(after) if after != before => {
                failures.push(format!("{}: tree shape changed", f.display()));
            }
            Some(_) => {}
        }
    }
    eprintln!(
        "checked {checked} corpus files ({whole} parse as a whole); {} failed",
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} of {checked} files failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// `GREBE_DEBUG_FILE=path cargo test -p grebe-format --test corpus debug_one -- --nocapture`
#[test]
fn debug_one() {
    let Ok(path) = std::env::var("GREBE_DEBUG_FILE") else {
        return;
    };
    let src = std::fs::read_to_string(&path).expect("read");
    let opts = Options::default();
    let once = format(&src, &opts);
    let twice = format(&once, &opts);
    println!("==== OUTPUT\n{once}");
    if once != twice {
        println!("==== NOT IDEMPOTENT; second pass:\n{twice}");
        for (i, (a, b)) in once.lines().zip(twice.lines()).enumerate() {
            if a != b {
                println!("first differing line {}:\n  1: {a}\n  2: {b}", i + 1);
                break;
            }
        }
    }
    let a = code_tokens(&src);
    let b = code_tokens(&once);
    if a != b {
        let k = a
            .iter()
            .zip(b.iter())
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len()));
        println!(
            "==== TOKENS DIFFER at {k}: {:?} vs {:?}",
            &a[k.saturating_sub(3)..(k + 3).min(a.len())],
            &b[k.saturating_sub(3)..(k + 3).min(b.len())]
        );
    }
    match (shape(&src), shape(&once)) {
        (Some(x), Some(y)) if x != y => {
            let k = x
                .iter()
                .zip(y.iter())
                .position(|(p, q)| p != q)
                .unwrap_or(x.len().min(y.len()));
            println!(
                "==== SHAPE DIFFERS at {k}: {:?} vs {:?}",
                &x[k.saturating_sub(5)..(k + 5).min(x.len())],
                &y[k.saturating_sub(5)..(k + 5).min(y.len())]
            );
        }
        (_, None) => println!("==== OUTPUT DOES NOT PARSE"),
        _ => {}
    }
}
