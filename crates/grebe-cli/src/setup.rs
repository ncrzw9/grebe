//! Shared "resolve config, build a `Selection`, collect files" plumbing for
//! `grebe check` and `grebe format`.
//!
//! Both verbs read `grebe.toml` (or `[tool.grebe]` in `pyproject.toml`) the
//! same way -- `--config PATH` loads exactly that file, `--no-config` reads
//! none, otherwise `grebe_rules::config::discover` walks upward from the
//! first PATH argument (the current directory, for stdin). Putting that
//! resolution, the include/exclude-aware file walk, and the
//! config-plus-flags merge into `Selection`/`format::Options` here means
//! `check` and `format` cannot quietly diverge on what a config file means.
//! The LSP runs the same `discover` from each document's path, so the editor
//! and the CLI resolve the same config for the same file.

use std::path::{Path, PathBuf};

use grebe_rules::config::Config;

/// How a verb was told to find its config, from `--config PATH` /
/// `--no-config` / neither.
pub enum ConfigChoice {
    /// `--config PATH`: load exactly this file.
    Explicit(PathBuf),
    /// `--no-config`: read none.
    None,
    /// Neither flag: discover upward from `discover_from`.
    Discover,
}

/// Resolves `choice` to a `Config`, or prints `grebe <verb>: <error>` and
/// hands back the exit code (`2`) a config error is worth.
///
/// `discover_from` is only consulted for [`ConfigChoice::Discover`]; it is
/// the first PATH argument on the command line, or the current directory
/// when the run has no path argument to anchor on (stdin).
pub fn resolve_config(
    verb: &str,
    choice: ConfigChoice,
    discover_from: &Path,
) -> Result<Option<Config>, i32> {
    let found = match choice {
        ConfigChoice::Explicit(path) => grebe_rules::config::load(&path).map(Some),
        ConfigChoice::None => Ok(None),
        ConfigChoice::Discover => grebe_rules::config::discover(discover_from),
    };
    found.map_err(|e| {
        eprintln!("grebe {verb}: {e}");
        2
    })
}

/// Every `.sql` file at or under `root`.
///
/// `root` itself -- a file named explicitly on the command line -- is always
/// processed: `include`/`exclude` only filter files turned up while walking
/// a directory, the same way a config can't hide a path someone pointed at
/// directly.
pub fn collect(root: &Path, config: Option<&Config>, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        if root.extension().is_some_and(|e| e == "sql") {
            out.push(root.to_path_buf());
        }
        return;
    }
    walk(root, config, out);
}

fn walk(dir: &Path, config: Option<&Config>, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk(&path, config, out);
        } else if path.extension().is_some_and(|ex| ex == "sql")
            && config.is_none_or(|c| c.selects(&path))
        {
            out.push(path);
        }
    }
}

/// Builds the `Selection` `grebe check` (and its `--fix`) runs with: a
/// `--select` flag wins over a config `select`, `[severity]` overrides and
/// `foreign-heads` come from the config unconditionally. `catalog` is the
/// snapshot the CAT rules read, already loaded (see [`load_catalog`]); with
/// `None` they do not run.
#[must_use]
pub fn selection<'a>(
    config: Option<&'a Config>,
    select_flag: Option<&'a [String]>,
    catalog: Option<&'a grebe_rules::catalog::Catalog>,
) -> grebe_rules::analysis::Selection<'a> {
    let select = select_flag.or_else(|| config.and_then(|c| c.select.as_deref()));
    grebe_rules::analysis::Selection {
        select,
        severity: config.map_or(&[][..], |c| c.severity.as_slice()),
        foreign_heads: config.map_or(&[][..], |c| c.foreign_heads.as_slice()),
        catalog,
        catalog_options: config.map(Config::catalog_options).unwrap_or_default(),
    }
}

/// Builds the `format::Options` `grebe format` runs with: the formatter's
/// built-in defaults, with any `[format]` knob a config sets overlaid on
/// top.
///
/// The `[format]` knobs are the whole style surface, and they live in
/// the config file only -- no command-line flags -- so a project has one
/// style regardless of who or what invokes the formatter.
#[must_use]
pub fn format_options(config: Option<&Config>) -> grebe_format::Options {
    let mut opts = grebe_format::Options::default();
    let Some(c) = config else { return opts };
    if let Some(n) = c.format.indent_size {
        opts.indent_size = n;
    }
    if let Some(n) = c.format.inline_threshold {
        opts.inline_threshold = n;
    }
    if let Some(kc) = c.format.keyword_case {
        opts.keyword_case = match kc {
            grebe_rules::config::KeywordCase::Upper => grebe_format::KeywordCase::Upper,
            grebe_rules::config::KeywordCase::Lower => grebe_format::KeywordCase::Lower,
        };
    }
    if let Some(on) = c.format.function_paren_space {
        opts.function_paren_space = on;
    }
    if let Some(on) = c.format.keyword_paren_space {
        opts.keyword_paren_space = on;
    }
    if let Some(s) = c.format.semicolon {
        opts.semicolon = match s {
            grebe_rules::config::Semicolon::Attached => grebe_format::Semicolon::Attached,
            grebe_rules::config::Semicolon::Spaced => grebe_format::Semicolon::Spaced,
            grebe_rules::config::Semicolon::Newline => grebe_format::Semicolon::Newline,
        };
    }
    opts
}

/// Reads the catalog snapshot `grebe check` runs the CAT rules against:
/// `--catalog FILE` if given, else the config's `catalog` key, else none.
/// Prints `grebe check: <error>` and hands back exit code `2` for a file
/// that cannot be read or is not a snapshot.
pub fn load_catalog(
    flag: Option<&str>,
    config: Option<&Config>,
) -> Result<Option<grebe_rules::catalog::Catalog>, i32> {
    let path = match (flag, config.and_then(|c| c.catalog.as_ref())) {
        (Some(p), _) => PathBuf::from(p),
        (None, Some(p)) => p.clone(),
        (None, None) => return Ok(None),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| {
        eprintln!(
            "grebe check: could not read catalog {}: {e}",
            path.display()
        );
        2
    })?;
    grebe_rules::catalog::Catalog::from_json_text(&text)
        .map(Some)
        .map_err(|e| {
            eprintln!(
                "grebe check: {} is not a catalog snapshot: {e}",
                path.display()
            );
            2
        })
}
