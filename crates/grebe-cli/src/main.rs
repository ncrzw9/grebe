//! `grebe` — the one binary.
//!
//! Maximum functionality, minimal interface: one flag per behaviour, no
//! aliases, no jargon in help text.
//!
//! One static binary with no runtime dependencies: it links no database
//! engine, opens no database, reads no catalog and makes no network call.
//! Its only inputs are SQL text and a config file, which keeps it fast,
//! hermetic in CI, and free of coupling to any installed engine version.
//! Name resolution against a schema is out of scope by design.
//!
//! ```text
//! grebe format [PATHS]     canonical layout; --check to only report
//! grebe check  [PATHS]     lint; --select CODE,... picks the MOD rules to run,
//!                          --fix rewrites in place (--unsafe widens it)
//! grebe rules              the registry, as a table
//! grebe lsp                the language server, on stdio
//! ```
//!
//! Config: `grebe.toml`, or `[tool.grebe]` in `pyproject.toml`, discovered by
//! `grebe_rules::config::discover` walking upward from the first PATH
//! argument (the current directory, for stdin). `--config PATH` reads
//! exactly that file instead; `--no-config` reads none. `[format]` knobs
//! apply to `format`; `select` (only when `--select` is absent -- the flag
//! wins), `[severity]` overrides and `foreign-heads` apply to `check`;
//! `include`/`exclude` filter a directory walk on either verb. Shared
//! resolution logic lives in `src/setup.rs` so the two verbs cannot disagree
//! about what a config file means.
//!
//! # Verbs
//!
//! `lsp` runs the language server (`grebe_lsp`) on stdio. `format` rewrites
//! files with `grebe_format::format`, or formats stdin to stdout through `-`;
//! `--check` reports without writing. `check --fix` (and `--unsafe`) applies
//! fixes through `grebe_rules::fix`, which sorts the edits, rejects overlaps
//! and splices them onto the original buffer, repeating until the file stops
//! changing. `check --json` prints one JSON document (`grebe_lsp::json`) and
//! nothing else on stdout.
//!
//! `parse`, `tree`, `batch` and `split` are undocumented on purpose:
//! internal verbs that drive the matcher directly when testing it against a
//! corpus, not part of the user-facing surface.
//!
//! Formatting is never a diagnostic: `check` reports no layout findings, and
//! `format --check` is the only layout gate.
//!
//! Exit codes: 0 clean (or findings that are all below error), 1 findings at
//! error severity (or, for `format --check`, a file that would change), 2
//! usage or config error, or a file that could not be read or written.

mod setup;

/// `print!` / `println!` for stdout, ending the run when the reader has
/// gone. Rust ignores SIGPIPE, so a plain `println!` panics when output is
/// piped into something that stops reading (`grebe rules | head`). This
/// exits instead, with 141, the status a Unix tool killed by SIGPIPE reports,
/// so `set -o pipefail` scripts see the usual signal rather than a false 0.
macro_rules! out {
    ($($arg:tt)*) => { emit(format_args!($($arg)*), false) };
}
macro_rules! outln {
    () => { emit(format_args!(""), true) };
    ($($arg:tt)*) => { emit(format_args!($($arg)*), true) };
}

fn emit(args: std::fmt::Arguments<'_>, newline: bool) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let written = stdout.write_fmt(args).and_then(|()| {
        if newline {
            stdout.write_all(b"\n")
        } else {
            Ok(())
        }
    });
    if let Err(e) = written {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(141);
        }
        eprintln!("grebe: cannot write output: {e}");
        std::process::exit(2);
    }
}

/// The parser is recursive, and a Rust stack overflow ABORTS -- it is not a
/// catchable error. `MAX_DEPTH` bounds recursion, but the frames themselves
/// are far larger in a debug build than in release, so a depth limit alone
/// cannot promise "never crashes" across build profiles. Running the work on
/// a thread with an explicit, generous stack makes the promise
/// profile-independent. One thread per process, so batch mode pays it once.
const STACK: usize = 256 * 1024 * 1024;

fn main() {
    let code = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(run)
        .expect("spawn parser thread")
        .join()
        .unwrap_or(101);
    std::process::exit(code);
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    // The internal verbs (`parse`, `tree`, `batch`, `split`) are left out of the help
    // on purpose; see the module doc comment.
    if args.len() >= 2 && args[1] == "--help" {
        print_top_help();
        return 0;
    }
    // A distributed binary has to be able to say which one it is. Without
    // this, someone installing a build on a second machine has no way to tell
    // whether the copy that is running is the copy they just installed.
    if args.len() >= 2 && args[1] == "--version" {
        outln!("grebe {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    if args.len() >= 3 && args[1] == "parse" {
        let sql = &args[2];
        let (ok, far) = grebe_syntax::matcher::parse_check(sql);
        if ok {
            outln!("OK");
            return 0;
        } else {
            outln!("FAIL {}", far);
            return 1;
        }
    }

    // Split mode for corpus testing: SQL on stdin, each statement written
    // out NUL-terminated, trimmed, empty ones dropped. Harnesses use it so
    // they cut a corpus into statements exactly as grebe does.
    if args.len() >= 2 && args[1] == "split" {
        use std::io::Read;
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            eprintln!("grebe: stdin is not valid UTF-8");
            return 2;
        }
        for sp in grebe_syntax::token::split_statements(&buf) {
            let stmt = buf[sp.start as usize..sp.end as usize].trim();
            if !stmt.is_empty() {
                out!("{stmt}\0");
            }
        }
        return 0;
    }

    // Batch mode for corpus testing: NUL-separated statements on stdin, one
    // verdict per line (`OK`, `FAIL <offset>`, `CRASH`), so a harness can
    // compare the matcher's accept/reject against a reference parser.
    // One process for the whole corpus: spawning a process per statement
    // costs more than parsing it, so per-statement runs would measure process
    // startup rather than the matcher.
    if args.len() >= 2 && args[1] == "batch" {
        use std::io::{Read, Write};
        let mut buf = String::new();
        if std::io::stdin().read_to_string(&mut buf).is_err() {
            eprintln!("grebe: stdin is not valid UTF-8");
            return 2;
        }
        let out = std::io::stdout();
        let mut out = std::io::BufWriter::new(out.lock());
        for stmt in buf.split('\0') {
            if stmt.is_empty() {
                continue;
            }
            // A panic here would otherwise abort the whole run; catching it
            // keeps CRASH distinguishable from a clean reject.
            let verdict = std::panic::catch_unwind(|| grebe_syntax::matcher::parse_check(stmt));
            match verdict {
                Ok((true, _)) => writeln!(out, "OK").ok(),
                Ok((false, far)) => writeln!(out, "FAIL {far}").ok(),
                Err(_) => writeln!(out, "CRASH").ok(),
            };
        }
        out.flush().ok();
        return 0;
    }

    if args.len() >= 3 && args[1] == "tree" {
        let sql = &args[2];
        match grebe_syntax::matcher::parse(sql) {
            Some(t) => {
                let mut stack = vec![(t.root(), 0usize)];
                while let Some((id, d)) = stack.pop() {
                    let txt = t.text(id, sql);
                    let short: String = txt.chars().take(48).collect();
                    outln!(
                        "{:indent$}{} [{}..{}] {:?}",
                        "",
                        t.rule_name(id),
                        t.node(id).span.start,
                        t.node(id).span.end,
                        short,
                        indent = d * 2
                    );
                    for &c in t.children(id).iter().rev() {
                        stack.push((c, d + 1));
                    }
                }
                return 0;
            }
            None => {
                eprintln!("parse failed");
                return 1;
            }
        }
    }

    // `lsp` speaks LSP over stdio and owns the process's stdin/stdout from
    // here on -- nothing else may print to stdout once it starts, or the
    // client sees a protocol violation rather than a diagnostic.
    if args.len() >= 2 && args[1] == "lsp" {
        if args.len() >= 3 && args[2] == "--help" {
            print_lsp_help();
            return 0;
        }
        return grebe_lsp::serve();
    }

    if args.len() >= 2 && args[1] == "format" {
        if args.len() >= 3 && args[2] == "--help" {
            print_format_help();
            return 0;
        }
        let mut want_check = false;
        let mut config_flag: Option<&String> = None;
        let mut no_config = false;
        let mut paths: Vec<&String> = Vec::new();
        let mut it = args[2..].iter();
        while let Some(a) = it.next() {
            if a == "--check" {
                want_check = true;
            } else if a == "--config" {
                match it.next() {
                    Some(v) => config_flag = Some(v),
                    None => {
                        eprintln!("grebe format: --config needs a path");
                        return 2;
                    }
                }
            } else if a == "--no-config" {
                no_config = true;
            } else if a == "-" {
                // A lone `-` is stdin, not a flag: it must not fall into the
                // "unrecognized flag" branch below just because it starts
                // with `-`.
                paths.push(a);
            } else if a.starts_with('-') {
                eprintln!("grebe format: unrecognized flag {a}");
                eprintln!("Run 'grebe format --help' for usage.");
                return 2;
            } else {
                paths.push(a);
            }
        }
        if config_flag.is_some() && no_config {
            eprintln!("grebe format: --config and --no-config are mutually exclusive");
            return 2;
        }
        // Stdin means exactly one PATH and it is `-`; mixing `-` with real
        // paths has no sensible meaning (which file does the stdout go
        // with?) so it is refused rather than guessed at.
        let stdin_mode = paths.len() == 1 && paths[0] == "-";
        if !stdin_mode && paths.iter().any(|p| p.as_str() == "-") {
            eprintln!("grebe format: '-' must be the only path");
            return 2;
        }

        let discover_from = match paths.first() {
            Some(p) if !stdin_mode => std::path::PathBuf::from((*p).as_str()),
            _ => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        };
        let choice = if let Some(p) = config_flag {
            setup::ConfigChoice::Explicit(std::path::PathBuf::from(p.as_str()))
        } else if no_config {
            setup::ConfigChoice::None
        } else {
            setup::ConfigChoice::Discover
        };
        let config = match setup::resolve_config("format", choice, &discover_from) {
            Ok(c) => c,
            Err(code) => return code,
        };
        let opts = setup::format_options(config.as_ref());

        if stdin_mode {
            use std::io::Read;
            let mut buf = String::new();
            if std::io::stdin().read_to_string(&mut buf).is_err() {
                eprintln!("grebe format: stdin is not valid UTF-8");
                return 2;
            }
            let out = grebe_format::format(&buf, &opts);
            if want_check {
                return i32::from(out != buf);
            }
            out!("{out}");
            return 0;
        }

        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for p in &paths {
            setup::collect(
                std::path::Path::new(p.as_str()),
                config.as_ref(),
                &mut files,
            );
        }
        if files.is_empty() {
            eprintln!("grebe format: no .sql files found");
            return 2;
        }
        files.sort();

        let mut changed = 0usize;
        let mut unchanged = 0usize;
        // Formatting is per file and pure, so it runs on every core; writing
        // and reporting stay sequential, in path order.
        let formatted = par_map(&files, |f| {
            let src = std::fs::read_to_string(f).map_err(|e| e.to_string())?;
            let out = grebe_format::format(&src, &opts);
            Ok::<_, String>((src, out))
        });
        let mut unreadable = 0usize;
        for (f, result) in files.iter().zip(formatted) {
            let (src, out) = match result {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("grebe format: could not read {}: {e}", f.display());
                    unreadable += 1;
                    continue;
                }
            };
            if out == src {
                unchanged += 1;
                continue;
            }
            if want_check {
                outln!("would reformat {}", f.display());
            } else {
                if let Err(e) = std::fs::write(f, &out) {
                    eprintln!("grebe format: could not write {}: {e}", f.display());
                    return 2;
                }
                outln!("reformatted {}", f.display());
            }
            changed += 1;
        }
        if want_check {
            eprintln!("\n{changed} file(s) would be reformatted, {unchanged} unchanged");
        } else {
            eprintln!("\n{changed} file(s) reformatted, {unchanged} unchanged");
        }
        if unreadable > 0 {
            return 2;
        }
        return i32::from(want_check && changed > 0);
    }

    if args.len() >= 2 && args[1] == "check" {
        if args.len() >= 3 && args[2] == "--help" {
            print_check_help();
            return 0;
        }
        // `--select CODE[,CODE]` turns on MOD rules that ship Off, and
        // restricts the MOD rules run to exactly those codes. Without it, a rule whose registry
        // default_severity is Off stays silent -- those rules fire often on
        // idiomatic DuckDB SQL, so they are opt-in rather than noise by
        // default.
        let mut select: Option<Vec<String>> = None;
        // `--fix` applies safe fixes; `--fix --unsafe` includes the unsafe
        // band. `--unsafe` alone is refused rather than silently implying
        // `--fix`: the flag widens what a rewrite may do, it does not ask for
        // one, and guessing wrong here edits a user's file.
        let mut want_fix = false;
        let mut want_unsafe = false;
        // `--json`: one JSON document on stdout with each finding as a
        // structured value (code, effective severity, byte span), so CI and
        // scripts read findings without parsing human-oriented lines.
        let mut want_json = false;
        let mut config_flag: Option<&String> = None;
        let mut no_config = false;
        // `--catalog FILE`: a catalog snapshot (see `grebe catalog`). With
        // one, the CAT rules check table and column names against it;
        // without one they do not run, and nothing else changes.
        let mut catalog_flag: Option<&str> = None;
        let mut paths: Vec<&String> = Vec::new();
        let mut it = args[2..].iter();
        while let Some(a) = it.next() {
            if a == "--catalog" {
                match it.next() {
                    Some(v) => catalog_flag = Some(v.as_str()),
                    None => {
                        eprintln!("grebe check: --catalog needs a path");
                        return 2;
                    }
                }
            } else if let Some(v) = a.strip_prefix("--catalog=") {
                catalog_flag = Some(v);
            } else if a == "--select" {
                match it.next() {
                    Some(v) => {
                        select = Some(grebe_rules::expand_select(
                            v.split(',')
                                .map(|c| c.trim().to_ascii_uppercase())
                                .collect(),
                        ));
                    }
                    None => {
                        eprintln!("grebe check: --select needs a rule code");
                        return 2;
                    }
                }
            } else if let Some(v) = a.strip_prefix("--select=") {
                select = Some(grebe_rules::expand_select(
                    v.split(',')
                        .map(|c| c.trim().to_ascii_uppercase())
                        .collect(),
                ));
            } else if a == "--fix" {
                want_fix = true;
            } else if a == "--unsafe" {
                want_unsafe = true;
            } else if a == "--json" {
                want_json = true;
            } else if a == "--config" {
                match it.next() {
                    Some(v) => config_flag = Some(v),
                    None => {
                        eprintln!("grebe check: --config needs a path");
                        return 2;
                    }
                }
            } else if a == "--no-config" {
                no_config = true;
            } else if a.starts_with('-') {
                // An unrecognised `--flag` is a usage error, never a path:
                // treating it as one would silently find nothing there and
                // carry on, hiding typos and unimplemented flags.
                eprintln!("grebe check: unrecognized flag {a}");
                eprintln!("Run 'grebe check --help' for usage.");
                return 2;
            } else {
                paths.push(a);
            }
        }
        if want_unsafe && !want_fix {
            eprintln!("grebe check: --unsafe only means something with --fix");
            eprintln!("Run 'grebe check --help' for usage.");
            return 2;
        }
        if config_flag.is_some() && no_config {
            eprintln!("grebe check: --config and --no-config are mutually exclusive");
            return 2;
        }
        let fix = want_fix.then_some(if want_unsafe {
            grebe_rules::fix::Safety::IncludeUnsafe
        } else {
            grebe_rules::fix::Safety::SafeOnly
        });
        if let Some(sel) = &select {
            for c in sel {
                if grebe_rules::lookup(c).is_none() {
                    eprintln!("grebe check: unknown rule code {c}");
                    return 2;
                }
            }
        }

        let discover_from = paths.first().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            |p| std::path::PathBuf::from(p.as_str()),
        );
        let choice = if let Some(p) = config_flag {
            setup::ConfigChoice::Explicit(std::path::PathBuf::from(p.as_str()))
        } else if no_config {
            setup::ConfigChoice::None
        } else {
            setup::ConfigChoice::Discover
        };
        let config = match setup::resolve_config("check", choice, &discover_from) {
            Ok(c) => c,
            Err(code) => return code,
        };
        let catalog = match setup::load_catalog(catalog_flag, config.as_ref()) {
            Ok(c) => c,
            Err(code) => return code,
        };
        let selection = setup::selection(config.as_ref(), select.as_deref(), catalog.as_ref());

        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for a in &paths {
            setup::collect(
                std::path::Path::new(a.as_str()),
                config.as_ref(),
                &mut files,
            );
        }
        if files.is_empty() {
            eprintln!("grebe check: no .sql files found");
            return 2;
        }
        files.sort();
        let mut total = 0usize;
        let mut errors = 0usize;
        let mut unparsed = 0usize;
        let mut fixed_files = 0usize;
        let mut fixed_edits = 0usize;
        let mut oscillated = 0usize;
        let mut json_findings: Vec<grebe_lsp::json::Json> = Vec::new();
        // Analysis and fixing are per file and pure, so they run on every
        // core. Writing, counting and printing stay sequential, in path
        // order, so output and failure behaviour do not depend on timing.
        let analysed = par_map(&files, |f| {
            let src = std::fs::read_to_string(f).map_err(|e| e.to_string())?;
            // One shared pipeline (`grebe_rules::analysis`) -- whole-file
            // parse, per-statement fallback, `--select`/config selection,
            // and suppression comments. The LSP calls the same function, so
            // the two front ends cannot drift apart about what a file holds.
            let findings = grebe_rules::analysis::analyze(&src, &selection);
            // `--fix` converges rather than applying one pass: overlapping
            // edits are rejected, so two findings wanting adjacent bytes
            // leave work behind, and a second `--fix` run must be a no-op.
            let fixed = fix.map(|safety| {
                let done = grebe_rules::fix::apply_until_stable_with(&src, &selection, safety);
                let after = done
                    .output
                    .as_ref()
                    .map(|out| grebe_rules::analysis::analyze(out, &selection));
                (done, after)
            });
            Ok::<_, String>((src, findings, fixed))
        });
        // A file that cannot be read (missing, or not UTF-8) is not a clean
        // file: it fails the run, as an I/O error, instead of passing CI.
        let mut unreadable = 0usize;
        for (f, result) in files.iter().zip(analysed) {
            let (src, findings, fixed) = match result {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("grebe check: could not read {}: {e}", f.display());
                    unreadable += 1;
                    continue;
                }
            };
            let mut src = src;
            let mut findings = findings;

            // `--fix` rewrites the file, so what gets reported is what is
            // left afterwards, not what was there on entry.
            if let Some((done, after)) = fixed {
                if done.hit_pass_limit {
                    eprintln!(
                        "grebe check: {} still changing after {} passes -- \
                         stopping. This is a rule-set oscillation; please report it.",
                        f.display(),
                        grebe_rules::fix::MAX_PASSES
                    );
                    oscillated += 1;
                }
                if let (Some(out), Some(after)) = (done.output, after) {
                    if let Err(e) = std::fs::write(f, &out) {
                        eprintln!("grebe check: could not write {}: {e}", f.display());
                        return 2;
                    }
                    fixed_files += 1;
                    fixed_edits += done.applied;
                    src = out;
                    findings = after;
                }
            }

            unparsed += grebe_rules::analysis::unparsed_count(&src);

            let lines = line_starts(&src);
            for fnd in findings {
                let (line, col) = line_col(&src, &lines, fnd.span.start as usize);
                let rule = grebe_rules::lookup(fnd.code);
                let name = rule.map_or("", |r| r.name);
                // The exit code and the reported severity key on the
                // *effective* severity -- a `[severity]` override, else the
                // registry default -- not the registry default alone, so a
                // config that promotes MOD001 to `error` can fail CI on it.
                let sev = selection.severity_of(fnd.code);
                // A CAT finding is about one name, and may know a near one:
                // its replacement is a suggestion, never applied by --fix.
                let suggestion = fnd
                    .fix
                    .as_ref()
                    .filter(|_| fnd.code.starts_with("CAT"))
                    .map(|fix| fix.replacement.clone());
                if want_json {
                    let mut pairs = vec![
                        (
                            "path".to_string(),
                            grebe_lsp::json::Json::str(f.display().to_string()),
                        ),
                        ("line".to_string(), grebe_lsp::json::Json::num(line as f64)),
                        ("column".to_string(), grebe_lsp::json::Json::num(col as f64)),
                        ("code".to_string(), grebe_lsp::json::Json::str(fnd.code)),
                        ("name".to_string(), grebe_lsp::json::Json::str(name)),
                        (
                            "severity".to_string(),
                            grebe_lsp::json::Json::str(severity_str(sev)),
                        ),
                        (
                            "start".to_string(),
                            grebe_lsp::json::Json::num(fnd.span.start as f64),
                        ),
                        (
                            "end".to_string(),
                            grebe_lsp::json::Json::num(fnd.span.end as f64),
                        ),
                    ];
                    if let Some(s) = suggestion {
                        pairs.push(("suggestion".to_string(), grebe_lsp::json::Json::str(s)));
                    }
                    // What the catalog says that makes a CAT003-010 finding.
                    if let Some(d) = &fnd.detail {
                        pairs.push(("detail".to_string(), grebe_lsp::json::Json::str(d.clone())));
                    }
                    json_findings.push(grebe_lsp::json::Json::object(pairs));
                } else if let Some(s) = suggestion {
                    outln!(
                        "{}:{}:{}: {} {} (did you mean {s}?)",
                        f.display(),
                        line,
                        col,
                        fnd.code,
                        name
                    );
                } else if let Some(d) = &fnd.detail {
                    outln!(
                        "{}:{}:{}: {} {} ({d})",
                        f.display(),
                        line,
                        col,
                        fnd.code,
                        name
                    );
                } else {
                    outln!("{}:{}:{}: {} {}", f.display(), line, col, fnd.code, name);
                }
                total += 1;
                if sev == grebe_rules::Severity::Error {
                    errors += 1;
                }
            }
        }
        if want_json {
            let doc = grebe_lsp::json::Json::object(vec![
                (
                    "version".to_string(),
                    grebe_lsp::json::Json::str(env!("CARGO_PKG_VERSION")),
                ),
                (
                    "files".to_string(),
                    grebe_lsp::json::Json::num(files.len() as f64),
                ),
                (
                    "findings".to_string(),
                    grebe_lsp::json::Json::Array(json_findings),
                ),
                (
                    "unparsed".to_string(),
                    grebe_lsp::json::Json::num(unparsed as f64),
                ),
                (
                    "fixed".to_string(),
                    grebe_lsp::json::Json::num(fixed_edits as f64),
                ),
            ]);
            outln!("{}", grebe_lsp::json::to_string(&doc));
        }
        if fix.is_some() {
            eprintln!(
                "\nfixed {fixed_edits} finding(s) in {fixed_files} file(s){}",
                if oscillated > 0 {
                    format!("; {oscillated} file(s) stopped at the pass limit")
                } else {
                    String::new()
                }
            );
        }
        eprintln!(
            "\n{} finding(s) in {} file(s){}{}",
            total,
            files.len(),
            if errors > 0 {
                format!("; {errors} at error severity")
            } else {
                String::new()
            },
            if unparsed > 0 {
                format!("; {unparsed} statement(s) did not parse")
            } else {
                String::new()
            }
        );
        // The exit code keys on severity, not on whether any finding fired
        // at all (see the module doc comment): MOD findings are the tool's
        // opinions on otherwise-valid SQL, expected in normal use, and
        // treating every finding as failure would make `check`
        // useless as a CI gate. A clean parse with only MOD findings must
        // still exit 0 unless a config promoted one to `error`.
        if unreadable > 0 {
            return 2;
        }
        return i32::from(errors > 0);
    }

    if args.len() >= 2 && args[1] == "catalog" {
        return catalog_command(&args[2..]);
    }

    if args.len() >= 2 && args[1] == "rules" {
        if args.len() >= 3 && args[2] == "--help" {
            print_rules_help();
            return 0;
        }
        print_rules_table();
        return 0;
    }

    eprintln!("grebe: missing or unknown command; run 'grebe --help' for usage");
    2
}

/// `grebe catalog [--duckdb PATH] [DATABASE]`: print a catalog snapshot of
/// DATABASE on stdout, by running the user's own `duckdb` CLI.
///
/// grebe embeds no engine, and this does not change that: the snapshot is
/// whatever `duckdb DATABASE -json -c <query>` prints, passed through after
/// checking it reads as a snapshot. The database is opened read-only and
/// `~/.duckdbrc` is not read, so the snapshot is of the file and nothing
/// else. DuckDB 2.0's CLI renders for an AI agent when it sees one in the
/// environment; an explicit output mode and `DUCKDB_AGENT_MODE=0` keep it
/// printing JSON.
fn catalog_command(args: &[String]) -> i32 {
    let mut duckdb = "duckdb".to_string();
    let mut database: Option<&str> = None;
    let mut with_metadata = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--help" {
            print_catalog_help();
            return 0;
        } else if a == "--with-metadata" {
            with_metadata = true;
        } else if a == "--duckdb" {
            match it.next() {
                Some(v) => duckdb = v.clone(),
                None => {
                    eprintln!("grebe catalog: --duckdb needs a path");
                    return 2;
                }
            }
        } else if let Some(v) = a.strip_prefix("--duckdb=") {
            duckdb = v.to_string();
        } else if a.starts_with('-') {
            eprintln!("grebe catalog: unrecognized flag {a}");
            eprintln!("Run 'grebe catalog --help' for usage.");
            return 2;
        } else if database.is_some() {
            eprintln!("grebe catalog: one database at a time");
            return 2;
        } else {
            database = Some(a.as_str());
        }
    }
    if let Some(db) = database {
        if !std::path::Path::new(db).is_file() {
            eprintln!("grebe catalog: no database file at {db}");
            return 2;
        }
    }
    // What duckdb prints for `sql`, and what it said on stderr.
    let query = |sql: &str| -> Result<(String, String), i32> {
        let mut cmd = std::process::Command::new(&duckdb);
        cmd.env("DUCKDB_AGENT_MODE", "0").arg("-no-init");
        match database {
            Some(db) => cmd.arg("-readonly").arg(db),
            None => cmd.arg(":memory:"),
        };
        cmd.arg("-json").arg("-c").arg(sql);
        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("grebe catalog: could not run {duckdb}: {e}");
                eprintln!("Install the DuckDB CLI, or point at it with --duckdb PATH.");
                return Err(2);
            }
        };
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            eprintln!("grebe catalog: {duckdb} failed: {}", stderr.trim());
            return Err(2);
        }
        Ok((String::from_utf8_lossy(&output.stdout).into_owned(), stderr))
    };
    let (stdout, stderr) = if with_metadata {
        // Two queries, put together as the object form of a snapshot.
        let columns = match query(grebe_rules::catalog::SNAPSHOT_NULLABLE_SQL) {
            Ok(r) => r,
            Err(code) => return code,
        };
        let tables = match query(grebe_rules::catalog::TABLES_SQL) {
            Ok(r) => r,
            Err(code) => return code,
        };
        (
            format!(
                "{{\"columns\": {},\n\"tables\": {}}}\n",
                columns.0.trim(),
                tables.0.trim()
            ),
            columns.1 + &tables.1,
        )
    } else {
        match query(grebe_rules::catalog::SNAPSHOT_SQL) {
            Ok(r) => r,
            Err(code) => return code,
        }
    };
    match grebe_rules::catalog::Catalog::from_json_text(&stdout) {
        Ok(cat) => {
            out!("{stdout}");
            let columns: usize = cat.tables().map(|t| t.columns.len()).sum();
            eprintln!(
                "{} table(s) and view(s), {columns} column(s)",
                cat.table_count()
            );
            0
        }
        Err(e) => {
            eprintln!("grebe catalog: {duckdb} did not print a snapshot: {e}");
            if !stderr.trim().is_empty() {
                eprintln!("{}", stderr.trim());
            }
            2
        }
    }
}

/// `grebe catalog --help`.
fn print_catalog_help() {
    outln!("grebe catalog — print a catalog snapshot of a DuckDB database");
    outln!();
    outln!("Usage: grebe catalog [--duckdb PATH] [--with-metadata] [DATABASE] > catalog.json");
    outln!();
    outln!("Arguments:");
    outln!("  DATABASE   a DuckDB database file, opened read-only. Without one,");
    outln!("             the snapshot is of an empty in-memory database.");
    outln!();
    outln!("Options:");
    outln!("  --duckdb PATH     the duckdb CLI to run (default: duckdb on PATH)");
    outln!("  --with-metadata   also say what DuckDB knows of each table: whether");
    outln!("                    each column may be NULL, the estimated row count,");
    outln!("                    the primary key, unique constraints and indexes");
    outln!();
    outln!("The snapshot is a JSON array with one object per column:");
    outln!("  {{\"database\", \"schema\", \"table\", \"column\", \"type\"}}");
    outln!("It is what DuckDB prints in -json mode for:");
    outln!("  {}", grebe_rules::catalog::SNAPSHOT_SQL);
    outln!("so any other way of running that query gives the same file.");
    outln!();
    outln!("With --with-metadata the snapshot is an object instead:");
    outln!("  {{\"columns\": [the same rows, each with \"nullable\"],");
    outln!("   \"tables\": [{{\"database\", \"schema\", \"table\", \"kind\", \"rows\",");
    outln!("               \"primary_key\", \"unique\", \"indexes\"}}]}}");
    outln!("Both forms are read wherever a snapshot is. The row count is DuckDB's");
    outln!("estimate (it does not go down when rows are deleted). CAT003 to");
    outln!("CAT010 read these facts and stay silent without them.");
    outln!();
    outln!("Give it to 'grebe check --catalog FILE', or name it in grebe.toml as");
    outln!("catalog = \"catalog.json\", and CAT001 (unknown table) and CAT002");
    outln!("(unknown column) check names against it, and CAT003 to CAT010 use");
    outln!("what it says of each table. grebe itself embeds no");
    outln!("engine: this command is the only one that runs duckdb, and only");
    outln!("when you ask for it.");
    outln!();
    outln!("Exit codes:");
    outln!("  0   a snapshot was printed");
    outln!("  2   usage error, or duckdb could not be run or failed");
}

/// Top-level `grebe --help`. Lists the user-facing verbs (`check`, `format`,
/// `rules`, `lsp`); `parse`, `tree`, `batch` and `split` are internal testing verbs
/// and are omitted on purpose (see the module doc comment's "Verbs" section).
fn print_top_help() {
    outln!("grebe — a DuckDB SQL formatter and linter");
    outln!();
    outln!("Usage: grebe <command> [ARGS]");
    outln!();
    outln!("Commands:");
    outln!("  format   rewrite SQL files to canonical layout");
    outln!("  check    lint SQL files or directories");
    outln!("  rules    print the rule registry as a table");
    outln!("  catalog  print a catalog snapshot of a DuckDB database, for");
    outln!("           check --catalog (runs your own duckdb CLI)");
    outln!("  lsp      run the language server on stdin/stdout");
    outln!();
    outln!("  --version   print the version and exit");
    outln!();
    outln!("Run 'grebe <command> --help' for that command's own flags.");
    outln!();
    outln!("Both format and check read grebe.toml (or [tool.grebe] in");
    outln!("pyproject.toml), discovered by walking up from the first path");
    outln!("given; --config PATH reads a specific file, --no-config reads none.");
    outln!();
    outln!("Exit codes:");
    outln!("  0   no findings, or every finding is below error severity");
    outln!("  1   at least one finding at error severity, or format --check");
    outln!("      found a file that would change");
    outln!("  2   usage or config error, or a file that could not be read");
    outln!("      (missing, or not UTF-8) or written");
}

/// `grebe format --help`.
fn print_format_help() {
    outln!("grebe format — rewrite SQL files to canonical layout");
    outln!();
    outln!("Usage: grebe format PATHS... [--check] [--config PATH | --no-config]");
    outln!();
    outln!("Arguments:");
    outln!("  PATHS   one or more files or directories to format; directories are");
    outln!("          searched recursively for *.sql files. A single '-' reads");
    outln!("          stdin and writes the formatted text to stdout, nothing else.");
    outln!();
    outln!("Options:");
    outln!("  --check          write nothing; print 'would reformat <path>' for");
    outln!("                   each file that would change, and exit 1 if any");
    outln!("                   would (0 if none would).");
    outln!("  --config PATH    read this config file instead of discovering one.");
    outln!("  --no-config      read no config file, even if grebe.toml exists.");
    outln!();
    outln!("Without --config or --no-config, grebe.toml (or [tool.grebe] in");
    outln!("pyproject.toml) is discovered by walking up from the first path given");
    outln!("(the current directory, for stdin). Its [format] table sets:");
    outln!();
    outln!("  indent_size           spaces per indent level, 1 to 16 (default 4)");
    outln!("  inline_threshold      a construct that fits in this many columns");
    outln!("                        stays on one line, 20 to 1000 (default 100)");
    outln!("  keyword_case          \"upper\" or \"lower\" (default \"upper\")");
    outln!("  function_paren_space  true puts a space between a call's name and");
    outln!("                        its parenthesis: count (x), coalesce (a, b);");
    outln!("                        type modifiers stay decimal(18, 2)");
    outln!("                        (default false)");
    outln!("  keyword_paren_space   false removes the space between a keyword");
    outln!("                        and a parenthesis after it: IN(1, 2), AS(...),");
    outln!("                        OVER(...) (default true)");
    outln!("  semicolon             \"attached\" ends a statement n; \"spaced\"");
    outln!("                        n ; and \"newline\" puts the semicolon on a");
    outln!("                        line of its own (default \"attached\")");
    outln!();
    outln!("Exit codes:");
    outln!("  0   nothing needed changing, or files were rewritten");
    outln!("  1   --check found a file that would change");
    outln!("  2   usage error (unrecognized flag, no .sql files found,");
    outln!("      unreadable/unwritable file, or a config error)");
}

/// `grebe check --help`.
fn print_check_help() {
    outln!("grebe check — lint SQL files");
    outln!();
    outln!("Usage: grebe check PATHS... [--select CODE[,CODE...]] [--fix [--unsafe]] [--json]");
    outln!("                    [--catalog FILE] [--config PATH | --no-config]");
    outln!();
    outln!("Arguments:");
    outln!("  PATHS   one or more files or directories to lint; directories are");
    outln!("          searched recursively for *.sql files");
    outln!();
    outln!("Options:");
    outln!("  --select CODE[,CODE...]   run only these MOD rules, and enable any of");
    outln!("                            them that default to Off (see 'grebe rules').");
    outln!("                            Without --select, every MOD rule whose default");
    outln!("                            severity is not Off runs. PRS and SRC findings");
    outln!("                            are always reported unless set to off in");
    outln!("                            [severity]. Wins over a config file's select.");
    outln!("  --select ALL              run every MOD rule, default-on and opt-in");
    outln!("                            alike -- the strictest check available.");
    outln!("  --select FRIENDLY         run the rules that rewrite a query into");
    outln!("                            DuckDB's friendly SQL (FROM first, GROUP BY");
    outln!("                            ALL, count(), ...); 'grebe rules' lists them.");
    outln!("                            All have a safe fix, so with --fix this turns");
    outln!("                            conventional SQL into that style. Can stand");
    outln!("                            among other codes: FRIENDLY,MOD006.");
    outln!("  --fix                     rewrite the files in place, applying the");
    outln!("                            fixes marked safe in 'grebe rules'. What is");
    outln!("                            printed afterwards is what is left.");
    outln!("  --unsafe                  with --fix, also apply the fixes marked");
    outln!("                            unsafe. These can change what a query");
    outln!("                            returns; read the diff. Needs --fix.");
    outln!("  --json                    print one JSON document to stdout instead");
    outln!("                            of text lines; summaries still go to");
    outln!("                            stderr.");
    outln!("  --catalog FILE            also check table and column names against");
    outln!("                            this catalog snapshot (CAT001, CAT002), and");
    outln!("                            queries against what it says of each table");
    outln!("                            (CAT003-CAT010, with --with-metadata);");
    outln!("                            'grebe catalog DATABASE' writes one. Wins");
    outln!("                            over a config file's catalog. Without a");
    outln!("                            catalog those rules do not run.");
    outln!("  --config PATH             read this config file instead of");
    outln!("                            discovering one.");
    outln!("  --no-config               read no config file, even if grebe.toml");
    outln!("                            exists.");
    outln!();
    outln!("Put '-- grebe: ignore[CODE]' on a statement to silence it (a bare");
    outln!("'-- grebe: ignore' silences every code). A suppressed finding is");
    outln!("never fixed.");
    outln!();
    outln!("Without --config or --no-config, grebe.toml (or [tool.grebe] in");
    outln!("pyproject.toml) is discovered by walking up from the first path given.");
    outln!("Its select, [severity], foreign-heads, catalog and large-table-rows apply");
    outln!("here (large-table-rows: the row estimate at which CAT003 calls a table");
    outln!("large, 1000000 by default); a config's own");
    outln!("select only takes effect when --select is absent on the command line.");
    outln!();
    outln!("Exit codes:");
    outln!("  0   no findings, or every finding is below error severity");
    outln!("  1   at least one finding at error severity (an effective severity,");
    outln!("      after any [severity] override)");
    outln!("  2   usage error (unrecognized flag, unknown rule code, no .sql");
    outln!("      files found, or a config error)");
}

/// `grebe rules --help`.
fn print_rules_help() {
    outln!("grebe rules — print the rule registry");
    outln!();
    outln!("Usage: grebe rules");
    outln!();
    outln!("No flags. Prints one row per rule: code, name, category, default");
    outln!("severity, and fix safety. Rows marked \"off (opt-in)\" are disabled by");
    outln!("default; turn one on with 'grebe check --select CODE,...'.");
    outln!();
    outln!("Categories: PRS a statement does not parse; SRC a statement not");
    outln!("analysed (template markup, another dialect); MOD an opinion on valid");
    outln!("SQL; CAT what a catalog snapshot shows (run only with --catalog or");
    outln!("catalog = in grebe.toml: CAT001-002 unknown names, CAT003-010 queries");
    outln!("its table metadata shows to be wrong or costly).");
    outln!();
    outln!("Exit codes:");
    outln!("  0   always");
}

/// `grebe rules`: the registry (`grebe_rules::RULES`) as a plain-text table,
/// sorted by code. Columns are aligned with spaces, not tabs, and widths are
/// computed from the actual rows rather than hardcoded so the table can't
/// silently misalign if a row's code or name grows.
fn print_rules_table() {
    let code = |r: &grebe_rules::Rule| r.code;
    let name = |r: &grebe_rules::Rule| r.name;
    let category = |r: &grebe_rules::Rule| match r.category {
        grebe_rules::Category::Prs => "PRS",
        grebe_rules::Category::Src => "SRC",
        grebe_rules::Category::Mod => "MOD",
        grebe_rules::Category::Cat => "CAT",
    };
    // Off-severity rows are opt-in rules, enabled through config: spelling
    // it "off (opt-in)" rather than bare "off" marks them at a glance
    // without adding a separate column.
    let severity = |r: &grebe_rules::Rule| match r.default_severity {
        grebe_rules::Severity::Error => "error",
        grebe_rules::Severity::Warning => "warning",
        grebe_rules::Severity::Info => "info",
        grebe_rules::Severity::Off => "off (opt-in)",
    };
    let fix = |r: &grebe_rules::Rule| match r.fix_safety {
        grebe_rules::FixSafety::Safe => "fix (safe)",
        grebe_rules::FixSafety::Unsafe => "fix (unsafe)",
        grebe_rules::FixSafety::None => "no fix",
    };

    let mut rows: Vec<&grebe_rules::Rule> = grebe_rules::RULES.iter().collect();
    rows.sort_by_key(|r| r.code);

    // A registered rule with no detector is silent, not clean. Someone
    // reading this table is deciding what to try, so the distinction between
    // "this rule found nothing" and "this rule cannot fire yet" has to be on
    // the row itself, not in a footnote they may not reach.
    let status = |r: &grebe_rules::Rule| {
        if grebe_rules::is_implemented(r.code) {
            "yes"
        } else {
            "not yet"
        }
    };

    let headers = ("CODE", "NAME", "CATEGORY", "SEVERITY", "DETECTOR", "FIX");
    let w_code = rows
        .iter()
        .map(|r| code(r).len())
        .max()
        .unwrap_or(0)
        .max(headers.0.len());
    let w_name = rows
        .iter()
        .map(|r| name(r).len())
        .max()
        .unwrap_or(0)
        .max(headers.1.len());
    let w_cat = rows
        .iter()
        .map(|r| category(r).len())
        .max()
        .unwrap_or(0)
        .max(headers.2.len());
    let w_sev = rows
        .iter()
        .map(|r| severity(r).len())
        .max()
        .unwrap_or(0)
        .max(headers.3.len());

    let w_st = rows
        .iter()
        .map(|r| status(r).len())
        .max()
        .unwrap_or(0)
        .max(headers.4.len());

    outln!(
        "{:w_code$}  {:w_name$}  {:w_cat$}  {:w_sev$}  {:w_st$}  {}",
        headers.0,
        headers.1,
        headers.2,
        headers.3,
        headers.4,
        headers.5
    );
    for r in &rows {
        outln!(
            "{:w_code$}  {:w_name$}  {:w_cat$}  {:w_sev$}  {:w_st$}  {}",
            code(r),
            name(r),
            category(r),
            severity(r),
            status(r),
            fix(r)
        );
    }
    outln!();
    outln!(
        "Rows marked \"off (opt-in)\" default to Off; enable one with 'grebe check --select CODE,...'."
    );
    outln!(
        "'--select FRIENDLY' is {}: the safe rewrites into DuckDB's friendly SQL.",
        grebe_rules::FRIENDLY.join(", ")
    );

    outln!(
        "DETECTOR \"not yet\" means the rule is registered but has no detector, so it cannot fire ({} of {} can).",
        grebe_rules::IMPLEMENTED.len(),
        grebe_rules::RULES.len()
    );
}

/// Byte offset of the start of every line, for [`line_col`].
fn line_starts(src: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(src.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

/// Byte offset -> 1-based line and column, counting columns in characters so
/// the number matches what an editor shows. A search of `lines`
/// ([`line_starts`]) rather than a scan from the top of the file: one scan
/// per finding made a file with many findings quadratic.
fn line_col(src: &str, lines: &[usize], off: usize) -> (usize, usize) {
    let off = off.min(src.len());
    let line = lines.partition_point(|&start| start <= off);
    let col = src[lines[line - 1]..off].chars().count() + 1;
    (line, col)
}

/// The effective severity of a `check` finding, spelled the way `[severity]`
/// and `--json` both use: lowercase, with `Off` as plain `off` rather than
/// the rules table's `off (opt-in)`.
fn severity_str(s: grebe_rules::Severity) -> &'static str {
    match s {
        grebe_rules::Severity::Error => "error",
        grebe_rules::Severity::Warning => "warning",
        grebe_rules::Severity::Info => "info",
        grebe_rules::Severity::Off => "off",
    }
}

/// `grebe lsp --help`. Deliberately terse: nobody types this verb by hand,
/// an editor spawns it. The one thing a human needs from it is confirmation
/// that the binary they configured is the one being launched.
fn print_lsp_help() {
    outln!("Usage: grebe lsp");
    outln!();
    outln!("Runs the language server over stdin/stdout, speaking LSP with");
    outln!("Content-Length framing. Editors spawn this; it is not useful");
    outln!("to run by hand.");
    outln!();
    outln!("Publishes diagnostics on open, change and save, and offers quick");
    outln!("fixes and semantic highlighting. Formatting (textDocument/formatting)");
    outln!("reads grebe.toml's [format] table the same way the CLI does;");
    outln!("completion and hover are not implemented.");
    outln!();
    outln!("The VS Code client lives in editors/vscode.");
}

/// Maps `work` over `items` on every available core and returns the results
/// in input order. Files are independent, so `check` and `format` scale with
/// cores; callers keep anything order-sensitive (writes, output) sequential.
/// Workers get the same [`STACK`] as the main thread, so parallel runs keep
/// the same no-crash guarantee on deeply nested SQL.
fn par_map<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let chunk = items.len().div_ceil(threads).max(1);
    let work = &work;
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|batch| {
                std::thread::Builder::new()
                    .stack_size(STACK)
                    .spawn_scoped(scope, move || batch.iter().map(work).collect::<Vec<_>>())
                    .expect("spawn worker thread")
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("worker thread panicked"))
            .collect()
    })
}
