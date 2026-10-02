//! `grebe.toml` / `[tool.grebe]` configuration: parsing, upward discovery,
//! and the typed [`Config`] the rest of the tool reads.
//!
//! This module only *produces* a [`Config`]; wiring it into `grebe check` /
//! `grebe format` / the LSP (reading `--config`/`--no-config`, applying
//! `include`/`exclude`, merging `[severity]` over the registry defaults, and
//! so on) belongs to the front ends; see the config doc comment in
//! `crates/grebe-cli/src/main.rs`.
//!
//! # Format
//!
//! `grebe.toml` is TOML, but only a hand-written **subset** is understood —
//! no external crate, per the workspace rule. Supported: `#` comments, blank
//! lines, table headers (`[severity]`, `[format]`, and — in `pyproject.toml`
//! — `[tool.grebe]`, `[tool.grebe.severity]`, `[tool.grebe.format]`), bare
//! and quoted keys, basic strings (`"..."`, with `\"` `\\` `\n` `\t`
//! escapes), literal strings (`'...'`, no escapes), integers, booleans, and
//! arrays of strings that may span multiple lines with a trailing comma.
//! Anything outside that subset — floats, dates, inline tables, dotted keys,
//! multi-line basic strings — is not understood; in a native `grebe.toml`
//! that is a parse error, in a `pyproject.toml` it is simply outside
//! `[tool.grebe]` and gets skipped, exactly like every other tool's table.
//!
//! Diagnostics from this module are plain [`ConfigError`] values (a path, a
//! line, a message). Nothing downstream matches on the message, the same
//! discipline as lint diagnostics, even though config errors are not lint
//! findings.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::Severity;

/// Casing the formatter should render keywords in. The only one of the three
/// format knobs exposed as a choice rather than a number.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum KeywordCase {
    #[default]
    Upper,
    Lower,
}

/// `[format]` (or `[tool.grebe.format]`) overrides. `None` means "use the
/// formatter's built-in default": `indent_size` 4, `inline_threshold` 100,
/// `keyword_case` UPPER.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormatConfig {
    pub indent_size: Option<usize>,
    pub inline_threshold: Option<usize>,
    pub keyword_case: Option<KeywordCase>,
}

/// One discovered/parsed configuration file, resolved to typed fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Directory containing the config file. Globs in `include`/`exclude` are
    /// relative to it.
    pub root: PathBuf,
    /// The file the config came from (for messages).
    pub path: PathBuf,
    /// Allowlist-first: if non-empty, only paths matching one of these globs
    /// are checked/formatted. Path globs are the primary way to keep known
    /// non-DuckDB directories out; foreign-dialect detection is the fallback.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Same meaning as `--select`: `Some(codes)` runs only these codes and
    /// enables `Off` ones. Codes uppercased.
    pub select: Option<Vec<String>>,
    /// Extra foreign-dialect statement heads for SRC003 (see
    /// `crate::source` for what a head is: a short uppercase keyword sequence
    /// like `"BEGIN TRAN"`). Stored uppercased and whitespace-normalized.
    /// Additive only: the built-in heads always apply.
    pub foreign_heads: Vec<String>,
    /// `[severity]` overrides, code -> severity, codes uppercased, in file
    /// order.
    pub severity: Vec<(String, Severity)>,
    pub format: FormatConfig,
}

/// A problem found while parsing a config file: where, and in plain English.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub path: PathBuf,
    /// 1-based; `0` when no single line applies (e.g. the file could not be
    /// read at all).
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.path.display(), self.line, self.message)
    }
}

impl std::error::Error for ConfigError {}

fn err(path: &Path, line: usize, message: impl Into<String>) -> ConfigError {
    ConfigError {
        path: path.to_path_buf(),
        line,
        message: message.into(),
    }
}

// ---------------------------------------------------------------------
// Tokenizing into logical lines
// ---------------------------------------------------------------------

/// A comment-stripped, bracket-balanced chunk of the file: either a table
/// header or a `key = value` assignment, always exactly one of the two,
/// tagged with the line it started on.
struct LogicalLine {
    text: String,
    line: usize,
}

/// Splits `text` into [`LogicalLine`]s: strips `#` comments (outside quotes),
/// and joins a `key = [ ... ]` assignment that spans several physical lines
/// into one logical line, since our subset's only multi-line construct is an
/// array. Strings never span physical lines in this subset.
fn split_logical_lines(text: &str, path: &Path) -> Result<Vec<LogicalLine>, ConfigError> {
    let mut lines = Vec::new();
    let mut buf = String::new();
    let mut buf_line = 1usize;
    let mut cur_line = 1usize;
    let mut depth: i32 = 0;
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escape = false;

    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_basic {
            buf.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_basic = false;
            } else if c == '\n' {
                return Err(err(
                    path,
                    cur_line,
                    "string is not closed before end of line",
                ));
            }
            if c == '\n' {
                cur_line += 1;
            }
            continue;
        }
        if in_literal {
            buf.push(c);
            if c == '\'' {
                in_literal = false;
            } else if c == '\n' {
                return Err(err(
                    path,
                    cur_line,
                    "string is not closed before end of line",
                ));
            }
            if c == '\n' {
                cur_line += 1;
            }
            continue;
        }
        match c {
            '"' => {
                if buf.is_empty() {
                    buf_line = cur_line;
                }
                in_basic = true;
                buf.push(c);
            }
            '\'' => {
                if buf.is_empty() {
                    buf_line = cur_line;
                }
                in_literal = true;
                buf.push(c);
            }
            '#' => {
                while let Some(&next) = chars.peek() {
                    if next == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '[' => {
                if buf.is_empty() {
                    buf_line = cur_line;
                }
                depth += 1;
                buf.push(c);
            }
            ']' => {
                depth -= 1;
                if depth < 0 {
                    return Err(err(path, cur_line, "unexpected `]` with no matching `[`"));
                }
                buf.push(c);
            }
            '\n' => {
                cur_line += 1;
                if depth == 0 {
                    let trimmed = buf.trim();
                    if !trimmed.is_empty() {
                        lines.push(LogicalLine {
                            text: trimmed.to_string(),
                            line: buf_line,
                        });
                    }
                    buf.clear();
                } else {
                    buf.push(' ');
                }
            }
            ' ' | '\t' | '\r' => {
                if !buf.is_empty() {
                    buf.push(c);
                }
            }
            _ => {
                if buf.is_empty() {
                    buf_line = cur_line;
                }
                buf.push(c);
            }
        }
    }
    if in_basic || in_literal {
        return Err(err(
            path,
            cur_line,
            "string is not closed before end of file",
        ));
    }
    if depth != 0 {
        return Err(err(path, buf_line, "array is missing a closing `]`"));
    }
    let trimmed = buf.trim();
    if !trimmed.is_empty() {
        lines.push(LogicalLine {
            text: trimmed.to_string(),
            line: buf_line,
        });
    }
    Ok(lines)
}

/// Splits a logical line on its first `=` that is outside any quoted
/// string, returning `(key_text, value_text)` both trimmed. `None` if there
/// is no such `=`.
fn split_key_value(line: &str) -> Option<(String, String)> {
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escape = false;
    let chars: Vec<char> = line.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if in_basic {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_basic = false;
            }
            continue;
        }
        if in_literal {
            if c == '\'' {
                in_literal = false;
            }
            continue;
        }
        match c {
            '"' => in_basic = true,
            '\'' => in_literal = true,
            '=' => {
                let key: String = chars[..i].iter().collect();
                let value: String = chars[i + 1..].iter().collect();
                return Some((key.trim().to_string(), value.trim().to_string()));
            }
            _ => {}
        }
    }
    None
}

/// Splits the inside of an array (already stripped of its `[` `]`) on
/// top-level commas, quote-aware, dropping a trailing empty element left by
/// a trailing comma.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escape = false;
    for c in s.chars() {
        if in_basic {
            cur.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_basic = false;
            }
            continue;
        }
        if in_literal {
            cur.push(c);
            if c == '\'' {
                in_literal = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_basic = true;
                cur.push(c);
            }
            '\'' => {
                in_literal = true;
                cur.push(c);
            }
            ',' => {
                parts.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let last = cur.trim();
    if !last.is_empty() {
        parts.push(last.to_string());
    }
    parts
}

// ---------------------------------------------------------------------
// Key and value parsing
// ---------------------------------------------------------------------

/// Decodes a basic-string body (no surrounding quotes) applying `\"` `\\`
/// `\n` `\t` escapes.
fn decode_basic_string(inner: &str, line: usize, path: &Path) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => {
                return Err(err(
                    path,
                    line,
                    format!("`\\{other}` is not a supported escape"),
                ));
            }
            None => return Err(err(path, line, "string ends with a trailing backslash")),
        }
    }
    Ok(out)
}

/// Parses one fully-formed string token (`"..."` or `'...'`, quotes
/// included, nothing else in `s`) into its content.
fn parse_string_token(s: &str, line: usize, path: &Path) -> Result<String, ConfigError> {
    if s.len() < 2 {
        return Err(err(path, line, format!("expected a string, found `{s}`")));
    }
    let first = s.chars().next().unwrap();
    let last = s.chars().next_back().unwrap();
    match first {
        '"' if last == '"' => decode_basic_string(&s[1..s.len() - 1], line, path),
        '\'' if last == '\'' => Ok(s[1..s.len() - 1].to_string()),
        '"' | '\'' => Err(err(path, line, "string is not closed")),
        _ => Err(err(path, line, format!("expected a string, found `{s}`"))),
    }
}

/// One parsed value, before it is assigned to a typed [`Config`] field.
enum Value {
    Str(String),
    Int(i64),
    // No config key is boolean-typed; the variant exists so the parser
    // accepts `true`/`false` syntactically (per the TOML subset) and reports
    // a proper type-mismatch error, rather than "could not understand the
    // value", when one is used where another type belongs.
    Bool,
    Array(Vec<String>),
}

impl Value {
    fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "a string",
            Value::Int(_) => "an integer",
            Value::Bool => "a boolean",
            Value::Array(_) => "an array",
        }
    }
}

fn parse_value(raw: &str, line: usize, path: &Path) -> Result<Value, ConfigError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(err(path, line, "missing a value after `=`"));
    }
    if let Some(stripped) = s.strip_prefix('[') {
        let Some(inner) = stripped.strip_suffix(']') else {
            return Err(err(path, line, "array is missing a closing `]`"));
        };
        let mut items = Vec::new();
        for item in split_top_level_commas(inner) {
            if !(item.starts_with('"') || item.starts_with('\'')) {
                return Err(err(
                    path,
                    line,
                    format!("arrays may only contain strings, found `{item}`"),
                ));
            }
            items.push(parse_string_token(&item, line, path)?);
        }
        return Ok(Value::Array(items));
    }
    if s.starts_with('"') || s.starts_with('\'') {
        return Ok(Value::Str(parse_string_token(s, line, path)?));
    }
    if s == "true" || s == "false" {
        return Ok(Value::Bool);
    }
    if s.starts_with('-') || s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return s
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| err(path, line, format!("expected an integer, found `{s}`")));
    }
    Err(err(
        path,
        line,
        format!("could not understand the value `{s}`"),
    ))
}

fn expect_array(
    value: Value,
    key: &str,
    line: usize,
    path: &Path,
) -> Result<Vec<String>, ConfigError> {
    match value {
        Value::Array(v) => Ok(v),
        other => Err(err(
            path,
            line,
            format!(
                "`{key}` must be an array of strings, found {}",
                other.type_name()
            ),
        )),
    }
}

fn parse_key_component(raw: &str, line: usize, path: &Path) -> Result<String, ConfigError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(err(path, line, "empty key"));
    }
    if s.starts_with('"') || s.starts_with('\'') {
        return parse_string_token(s, line, path);
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Ok(s.to_string())
    } else {
        Err(err(path, line, format!("`{s}` is not a valid key")))
    }
}

fn parse_header(text: &str, line: usize, path: &Path) -> Result<Vec<String>, ConfigError> {
    let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) else {
        return Err(err(path, line, "table header is missing a closing `]`"));
    };
    if inner.trim().is_empty() {
        return Err(err(path, line, "table header has no name"));
    }
    inner
        .split('.')
        .map(|part| parse_key_component(part, line, path))
        .collect()
}

/// Collapses runs of whitespace to a single space and uppercases — the shape
/// `crate::source`'s foreign-head list expects (e.g. `"begin  tran"` ->
/// `"BEGIN TRAN"`).
fn normalize_head(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

fn parse_severity_value(s: &str, line: usize, path: &Path) -> Result<Severity, ConfigError> {
    match s.to_ascii_lowercase().as_str() {
        "error" => Ok(Severity::Error),
        "warning" => Ok(Severity::Warning),
        "info" => Ok(Severity::Info),
        "off" => Ok(Severity::Off),
        _ => Err(err(
            path,
            line,
            format!("`{s}` is not a severity; use error, warning, info, or off"),
        )),
    }
}

// ---------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------

/// Which table a logical line's `key = value` belongs to, resolved from the
/// most recent table header (or the implicit top-level table at the start of
/// a native `grebe.toml`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    /// `include` / `exclude` / `select` / `foreign-heads`.
    Top,
    Severity,
    Format,
    /// A table this module does not read: content in `pyproject.toml` that
    /// belongs to another tool. (In native mode an unrecognized table is an
    /// error at the header itself and never gets this far.)
    Foreign,
}

fn classify_section(
    parts: &[String],
    pyproject: bool,
    line: usize,
    path: &Path,
) -> Result<Section, ConfigError> {
    let joined: Vec<&str> = parts.iter().map(String::as_str).collect();
    if !pyproject {
        return match joined.as_slice() {
            ["severity"] => Ok(Section::Severity),
            ["format"] => Ok(Section::Format),
            _ => Err(err(
                path,
                line,
                format!("unknown table `[{}]`", joined.join(".")),
            )),
        };
    }
    match joined.as_slice() {
        ["tool", "grebe"] => Ok(Section::Top),
        ["tool", "grebe", "severity"] => Ok(Section::Severity),
        ["tool", "grebe", "format"] => Ok(Section::Format),
        _ if joined.first() == Some(&"tool") && joined.get(1) == Some(&"grebe") => Err(err(
            path,
            line,
            format!("unknown table `[{}]`", joined.join(".")),
        )),
        _ => Ok(Section::Foreign),
    }
}

fn apply_top(
    config: &mut Config,
    seen: &mut HashSet<String>,
    key: &str,
    val_raw: &str,
    line: usize,
    path: &Path,
) -> Result<(), ConfigError> {
    let canonical = match key {
        "include" => "include",
        "exclude" => "exclude",
        "select" => "select",
        "foreign-heads" | "foreign_heads" => "foreign-heads",
        other => return Err(err(path, line, format!("unknown key `{other}`"))),
    };
    if !seen.insert(canonical.to_string()) {
        return Err(err(path, line, format!("duplicate key `{canonical}`")));
    }
    let value = parse_value(val_raw, line, path)?;
    match canonical {
        "include" => config.include = expect_array(value, "include", line, path)?,
        "exclude" => config.exclude = expect_array(value, "exclude", line, path)?,
        "select" => {
            let arr = expect_array(value, "select", line, path)?;
            let mut codes = Vec::with_capacity(arr.len());
            for code in arr {
                let upper = code.to_ascii_uppercase();
                if crate::lookup(&upper).is_none() {
                    return Err(err(path, line, format!("unknown rule code `{upper}`")));
                }
                codes.push(upper);
            }
            config.select = Some(codes);
        }
        "foreign-heads" => {
            let arr = expect_array(value, "foreign-heads", line, path)?;
            config.foreign_heads = arr.iter().map(|h| normalize_head(h)).collect();
        }
        _ => unreachable!("canonical key set is exhaustive above"),
    }
    Ok(())
}

fn apply_severity(
    config: &mut Config,
    seen: &mut HashSet<String>,
    key: &str,
    val_raw: &str,
    line: usize,
    path: &Path,
) -> Result<(), ConfigError> {
    let code = key.to_ascii_uppercase();
    if crate::lookup(&code).is_none() {
        return Err(err(path, line, format!("unknown rule code `{code}`")));
    }
    if !seen.insert(code.clone()) {
        return Err(err(path, line, format!("duplicate key `{code}`")));
    }
    let value = parse_value(val_raw, line, path)?;
    let text = match value {
        Value::Str(s) => s,
        other => {
            return Err(err(
                path,
                line,
                format!("`{code}` must be a string, found {}", other.type_name()),
            ));
        }
    };
    let severity = parse_severity_value(&text, line, path)?;
    config.severity.push((code, severity));
    Ok(())
}

fn apply_format(
    config: &mut Config,
    seen: &mut HashSet<String>,
    key: &str,
    val_raw: &str,
    line: usize,
    path: &Path,
) -> Result<(), ConfigError> {
    if !matches!(key, "indent_size" | "inline_threshold" | "keyword_case") {
        return Err(err(path, line, format!("unknown key `{key}`")));
    }
    if !seen.insert(key.to_string()) {
        return Err(err(path, line, format!("duplicate key `{key}`")));
    }
    let value = parse_value(val_raw, line, path)?;
    match key {
        "indent_size" => {
            let Value::Int(n) = value else {
                return Err(err(
                    path,
                    line,
                    format!(
                        "`indent_size` must be an integer, found {}",
                        value.type_name()
                    ),
                ));
            };
            if !(1..=16).contains(&n) {
                return Err(err(path, line, "`indent_size` must be between 1 and 16"));
            }
            config.format.indent_size = Some(n as usize);
        }
        "inline_threshold" => {
            let Value::Int(n) = value else {
                return Err(err(
                    path,
                    line,
                    format!(
                        "`inline_threshold` must be an integer, found {}",
                        value.type_name()
                    ),
                ));
            };
            if !(20..=1000).contains(&n) {
                return Err(err(
                    path,
                    line,
                    "`inline_threshold` must be between 20 and 1000",
                ));
            }
            config.format.inline_threshold = Some(n as usize);
        }
        "keyword_case" => {
            let Value::Str(s) = value else {
                return Err(err(
                    path,
                    line,
                    format!(
                        "`keyword_case` must be a string, found {}",
                        value.type_name()
                    ),
                ));
            };
            config.format.keyword_case = Some(match s.to_ascii_lowercase().as_str() {
                "upper" => KeywordCase::Upper,
                "lower" => KeywordCase::Lower,
                _ => {
                    return Err(err(
                        path,
                        line,
                        "`keyword_case` must be \"upper\" or \"lower\"",
                    ));
                }
            });
        }
        _ => unreachable!("key set checked above"),
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------

/// Parses `text`, plus whether a `[tool.grebe*]` table was actually seen
/// (only meaningful, and only checked, in `pyproject` mode — [`discover`]
/// uses it to decide whether a `pyproject.toml` counts as a hit).
fn parse_inner(text: &str, path: &Path, pyproject: bool) -> Result<(Config, bool), ConfigError> {
    let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut config = Config {
        root,
        path: path.to_path_buf(),
        ..Config::default()
    };
    let mut section = if pyproject {
        Section::Foreign
    } else {
        Section::Top
    };
    let mut saw_grebe_table = false;
    let mut top_seen = HashSet::new();
    let mut severity_seen = HashSet::new();
    let mut format_seen = HashSet::new();

    for ll in split_logical_lines(text, path)? {
        let t = ll.text.trim();
        if t.is_empty() {
            continue;
        }
        if t.starts_with('[') {
            let parts = parse_header(t, ll.line, path)?;
            section = classify_section(&parts, pyproject, ll.line, path)?;
            if pyproject && section != Section::Foreign {
                saw_grebe_table = true;
            }
            continue;
        }
        let (key_raw, val_raw) =
            split_key_value(t).ok_or_else(|| err(path, ll.line, "expected `key = value`"))?;
        let key = parse_key_component(&key_raw, ll.line, path)?;
        match section {
            Section::Foreign => {}
            Section::Top => apply_top(&mut config, &mut top_seen, &key, &val_raw, ll.line, path)?,
            Section::Severity => apply_severity(
                &mut config,
                &mut severity_seen,
                &key,
                &val_raw,
                ll.line,
                path,
            )?,
            Section::Format => {
                apply_format(&mut config, &mut format_seen, &key, &val_raw, ll.line, path)?
            }
        }
    }
    Ok((config, saw_grebe_table))
}

/// Parses config text. `pyproject = true` means the text is a
/// `pyproject.toml` and only the `[tool.grebe]` table (and its subtables
/// `[tool.grebe.severity]`, `[tool.grebe.format]`) are read; everything else
/// in the file is skipped without error. `path` is used for `root` (its
/// parent) and error messages.
pub fn parse(text: &str, path: &Path, pyproject: bool) -> Result<Config, ConfigError> {
    parse_inner(text, path, pyproject).map(|(config, _)| config)
}

/// Reads and parses one file. A file named `pyproject.toml` is parsed in
/// pyproject mode.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| err(path, 0, format!("could not read file: {e}")))?;
    let pyproject = path.file_name().is_some_and(|n| n == "pyproject.toml");
    parse(&text, path, pyproject)
}

/// Walks upward from `start` (a file or directory; if a file, starts at its
/// parent) to the filesystem root. In each directory looks for `grebe.toml`
/// first, then `pyproject.toml` (only counts if it actually contains a
/// `[tool.grebe]` table). First hit wins. `Ok(None)` if nothing found.
pub fn discover(start: &Path) -> Result<Option<Config>, ConfigError> {
    let start_is_dir = std::fs::metadata(start)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    let mut dir = if start_is_dir {
        start.to_path_buf()
    } else {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    loop {
        let grebe_path = dir.join("grebe.toml");
        if grebe_path.is_file() {
            return load(&grebe_path).map(Some);
        }
        let pyproject_path = dir.join("pyproject.toml");
        if pyproject_path.is_file() {
            let text = std::fs::read_to_string(&pyproject_path)
                .map_err(|e| err(&pyproject_path, 0, format!("could not read file: {e}")))?;
            let (config, saw_grebe_table) = parse_inner(&text, &pyproject_path, true)?;
            if saw_grebe_table {
                return Ok(Some(config));
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return Ok(None),
        }
    }
}

// ---------------------------------------------------------------------
// Glob matching
// ---------------------------------------------------------------------

/// Matches `*` / `?` / literal characters within one path segment (never
/// crossing a `/`).
fn segment_match(pattern: &[char], text: &[char]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            segment_match(&pattern[1..], text)
                || (!text.is_empty() && segment_match(pattern, &text[1..]))
        }
        (Some('?'), Some(_)) => segment_match(&pattern[1..], &text[1..]),
        (Some(p), Some(t)) if p == t => segment_match(&pattern[1..], &text[1..]),
        _ => false,
    }
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(&"**") => {
            segments_match(&pattern[1..], path)
                || (!path.is_empty() && segments_match(pattern, &path[1..]))
        }
        Some(p) => {
            if path.is_empty() {
                return false;
            }
            let pc: Vec<char> = p.chars().collect();
            let tc: Vec<char> = path[0].chars().collect();
            segment_match(&pc, &tc) && segments_match(&pattern[1..], &path[1..])
        }
    }
}

/// Glob match for `include`/`exclude`. `path` is relative to [`Config::root`],
/// forward slashes. Supports `*` (within one segment), `**` (zero or more
/// segments), `?`, and literal characters. A pattern with no `/` matches
/// against the file name only (like `.gitignore`). Case-sensitive.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let path_segments: Vec<&str> = path.split('/').collect();
    if !pattern.contains('/') {
        let base = path_segments.last().copied().unwrap_or("");
        let pc: Vec<char> = pattern.chars().collect();
        let tc: Vec<char> = base.chars().collect();
        return segment_match(&pc, &tc);
    }
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    segments_match(&pattern_segments, &path_segments)
}

fn to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

impl Config {
    /// Is this path (absolute or relative to cwd) selected by
    /// `include`/`exclude`? An absolute path is made relative to `root`
    /// first. A path outside `root` is matched against `exclude` as written
    /// and is selected only when `include` is empty. Empty `include` means
    /// everything is included.
    pub fn selects(&self, path: &Path) -> bool {
        let relative = if path.is_absolute() {
            path.strip_prefix(&self.root).ok().map(Path::to_path_buf)
        } else {
            Some(path.to_path_buf())
        };

        let check = relative.as_deref().map_or_else(|| to_slash(path), to_slash);
        if self
            .exclude
            .iter()
            .any(|pattern| glob_match(pattern, &check))
        {
            return false;
        }
        match relative {
            None => self.include.is_empty(),
            Some(_) => {
                self.include.is_empty()
                    || self
                        .include
                        .iter()
                        .any(|pattern| glob_match(pattern, &check))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // -- parse: a full example, every top-level key exercised -----------

    #[test]
    fn parses_a_full_example() {
        let text = r#"
            # a comment
            include = ["*.sql", "sql/**/*.sql"]
            exclude = [
                "generated/*.sql",
                "vendor/**",
            ]
            select = ["mod001", "MOD002"]
            foreign-heads = ["begin   tran", "EXEC"]

            [severity]
            MOD001 = "off"
            "MOD002" = "Error"

            [format]
            indent_size = 2
            inline_threshold = 80
            keyword_case = "lower"
        "#;
        let config = parse(text, &p("/repo/grebe.toml"), false).expect("should parse");
        assert_eq!(config.root, p("/repo"));
        assert_eq!(config.path, p("/repo/grebe.toml"));
        assert_eq!(config.include, vec!["*.sql", "sql/**/*.sql"]);
        assert_eq!(config.exclude, vec!["generated/*.sql", "vendor/**"]);
        assert_eq!(
            config.select,
            Some(vec!["MOD001".to_string(), "MOD002".to_string()])
        );
        assert_eq!(config.foreign_heads, vec!["BEGIN TRAN", "EXEC"]);
        assert_eq!(
            config.severity,
            vec![
                ("MOD001".to_string(), Severity::Off),
                ("MOD002".to_string(), Severity::Error)
            ]
        );
        assert_eq!(config.format.indent_size, Some(2));
        assert_eq!(config.format.inline_threshold, Some(80));
        assert_eq!(config.format.keyword_case, Some(KeywordCase::Lower));
    }

    #[test]
    fn empty_file_parses_to_defaults() {
        let config = parse("", &p("/repo/grebe.toml"), false).expect("should parse");
        assert_eq!(
            config,
            Config {
                root: p("/repo"),
                path: p("/repo/grebe.toml"),
                ..Config::default()
            }
        );
    }

    // -- pyproject mode ---------------------------------------------------

    #[test]
    fn pyproject_mode_ignores_foreign_tables() {
        let text = r#"
            [build-system]
            requires = ["setuptools"]

            [project]
            name = "whatever"
            version = 1.0
            keywords = ["a", "[b]"]

            [tool.black]
            line-length = 88

            [tool.grebe]
            include = ["*.sql"]

            [tool.grebe.severity]
            MOD001 = "warning"
        "#;
        let config = parse(text, &p("/repo/pyproject.toml"), true).expect("should parse");
        assert_eq!(config.include, vec!["*.sql"]);
        assert_eq!(
            config.severity,
            vec![("MOD001".to_string(), Severity::Warning)]
        );
    }

    #[test]
    fn pyproject_mode_without_tool_grebe_parses_to_defaults() {
        let text = "[project]\nname = \"whatever\"\n";
        let config = parse(text, &p("/repo/pyproject.toml"), true).expect("should parse");
        assert_eq!(
            config,
            Config {
                root: p("/repo"),
                path: p("/repo/pyproject.toml"),
                ..Config::default()
            }
        );
    }

    #[test]
    fn native_mode_rejects_tool_grebe_header() {
        let text = "[tool.grebe]\ninclude = [\"*.sql\"]\n";
        let e = parse(text, &p("/repo/grebe.toml"), false).unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn pyproject_unknown_subtable_under_tool_grebe_errors() {
        let text = "[tool.grebe.nope]\nx = 1\n";
        let e = parse(text, &p("/repo/pyproject.toml"), true).unwrap_err();
        assert_eq!(e.line, 1);
    }

    // -- error classes, each with the right line number -------------------

    #[test]
    fn unknown_top_level_key_errors_with_line() {
        let text = "include = [\"*.sql\"]\nbogus = \"x\"\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("bogus"), "{}", e.message);
    }

    #[test]
    fn unknown_table_errors_with_line() {
        let text = "\n\n[nope]\nx = 1\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 3);
    }

    #[test]
    fn unknown_rule_code_in_select_errors() {
        let text = "select = [\"MOD999\"]\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 1);
        assert!(e.message.contains("MOD999"), "{}", e.message);
    }

    #[test]
    fn unknown_rule_code_in_severity_errors() {
        let text = "[severity]\nMOD999 = \"off\"\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("MOD999"), "{}", e.message);
    }

    #[test]
    fn bad_severity_value_errors() {
        let text = "[severity]\nMOD001 = \"maybe\"\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn wrong_type_for_include_errors() {
        let text = "include = \"*.sql\"\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn wrong_type_for_severity_value_errors() {
        let text = "[severity]\nMOD001 = 1\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn duplicate_top_level_key_errors() {
        let text = "include = [\"a\"]\ninclude = [\"b\"]\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn duplicate_key_via_alias_errors() {
        let text = "foreign-heads = [\"A\"]\nforeign_heads = [\"B\"]\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn duplicate_severity_key_errors() {
        let text = "[severity]\nMOD001 = \"off\"\n\"MOD001\" = \"warning\"\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 3);
    }

    #[test]
    fn duplicate_format_key_errors() {
        let text = "[format]\nindent_size = 2\nindent_size = 4\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 3);
    }

    #[test]
    fn indent_size_out_of_range_errors() {
        let text = "[format]\nindent_size = 17\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn indent_size_wrong_type_errors() {
        let text = "[format]\nindent_size = \"4\"\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn inline_threshold_out_of_range_errors() {
        let text = "[format]\ninline_threshold = 19\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn keyword_case_bad_value_errors() {
        let text = "[format]\nkeyword_case = \"sideways\"\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn unknown_format_key_errors() {
        let text = "[format]\nwidth = 4\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn unterminated_string_errors() {
        let text = "include = [\"*.sql\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn unterminated_array_errors() {
        let text = "include = [\n\"a.sql\"\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn bad_escape_errors() {
        let text = "include = [\"\\q\"]\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn key_value_line_with_no_equals_errors() {
        let text = "include\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn array_with_non_string_element_errors() {
        let text = "include = [1]\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        assert_eq!(e.line, 1);
    }

    #[test]
    fn multiline_array_reports_the_assignment_line() {
        let text = "include = [\n\"a.sql\",\nbogus\n]\n";
        let e = parse(text, &p("c.toml"), false).unwrap_err();
        // The whole multi-line array is one logical line, starting at line 1.
        assert_eq!(e.line, 1);
    }

    // -- Display -----------------------------------------------------------

    #[test]
    fn config_error_display_format() {
        let e = ConfigError {
            path: p("grebe.toml"),
            line: 5,
            message: "boom".to_string(),
        };
        assert_eq!(format!("{e}"), "grebe.toml:5: boom");
    }

    // -- glob_match ----------------------------------------------------

    #[test]
    fn glob_star_matches_within_a_segment() {
        assert!(glob_match("*.sql", "foo.sql"));
        assert!(glob_match("*.sql", "sql/foo.sql")); // bare pattern -> basename only
        assert!(!glob_match("*.sql", "foo.py"));
    }

    #[test]
    fn glob_star_does_not_cross_a_slash() {
        assert!(!glob_match("sql/*.sql", "sql/nested/foo.sql"));
        assert!(glob_match("sql/*.sql", "sql/foo.sql"));
    }

    #[test]
    fn glob_double_star_matches_zero_or_more_segments() {
        assert!(glob_match("sql/**/*.sql", "sql/foo.sql"));
        assert!(glob_match("sql/**/*.sql", "sql/a/b/foo.sql"));
        assert!(!glob_match("sql/**/*.sql", "other/foo.sql"));
        assert!(glob_match("**/generated/**", "a/b/generated/c/d.sql"));
        assert!(glob_match("**/generated/**", "generated/d.sql"));
        assert!(!glob_match("**/generated/**", "a/b/c.sql"));
    }

    #[test]
    fn glob_question_mark_matches_one_char() {
        assert!(glob_match("a?c.sql", "abc.sql"));
        assert!(!glob_match("a?c.sql", "ac.sql"));
        assert!(!glob_match("a?c.sql", "abbc.sql"));
    }

    #[test]
    fn glob_bare_pattern_matches_filename_only() {
        assert!(glob_match("foo.sql", "a/b/foo.sql"));
        assert!(!glob_match("foo.sql", "a/b/other.sql"));
    }

    #[test]
    fn glob_is_case_sensitive() {
        assert!(!glob_match("*.SQL", "foo.sql"));
    }

    // -- Config::selects -------------------------------------------------

    fn cfg(root: &str, include: &[&str], exclude: &[&str]) -> Config {
        Config {
            root: p(root),
            path: p(root).join("grebe.toml"),
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn selects_empty_include_means_everything() {
        let c = cfg("/repo", &[], &[]);
        assert!(c.selects(&p("/repo/a.sql")));
        assert!(c.selects(&p("/repo/sub/b.sql")));
    }

    #[test]
    fn selects_respects_include_allowlist() {
        let c = cfg("/repo", &["sql/**/*.sql"], &[]);
        assert!(c.selects(&p("/repo/sql/a.sql")));
        assert!(!c.selects(&p("/repo/other/a.sql")));
    }

    #[test]
    fn selects_exclude_wins_over_include() {
        let c = cfg("/repo", &["**/*.sql"], &["**/generated/**"]);
        assert!(c.selects(&p("/repo/sql/a.sql")));
        assert!(!c.selects(&p("/repo/sql/generated/a.sql")));
    }

    #[test]
    fn selects_path_outside_root_with_include_is_excluded() {
        let c = cfg("/repo", &["*.sql"], &[]);
        assert!(!c.selects(&p("/elsewhere/a.sql")));
    }

    #[test]
    fn selects_path_outside_root_without_include_is_included() {
        let c = cfg("/repo", &[], &[]);
        assert!(c.selects(&p("/elsewhere/a.sql")));
    }

    #[test]
    fn selects_relative_path_is_treated_as_relative_to_root() {
        let c = cfg("/repo", &["sql/*.sql"], &[]);
        assert!(c.selects(&p("sql/a.sql")));
        assert!(!c.selects(&p("other/a.sql")));
    }

    // -- discover ----------------------------------------------------------

    /// A unique scratch directory under the OS temp dir, removed on drop.
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "grebe-rules-config-test-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let root = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&root).expect("create temp tree");
            TempTree { root }
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let d = self.root.join(rel);
            std::fs::create_dir_all(&d).expect("create subdir");
            d
        }

        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let f = self.root.join(rel);
            std::fs::write(&f, contents).expect("write file");
            f
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn discover_finds_grebe_toml_in_start_directory() {
        let tree = TempTree::new("finds-here");
        tree.write("grebe.toml", "include = [\"*.sql\"]\n");
        let found = discover(&tree.root)
            .expect("discover should not error")
            .expect("should find one");
        assert_eq!(found.include, vec!["*.sql"]);
    }

    #[test]
    fn discover_walks_upward_to_find_grebe_toml() {
        let tree = TempTree::new("walks-up");
        tree.write("grebe.toml", "include = [\"*.sql\"]\n");
        let leaf = tree.dir("a/b/c");
        let found = discover(&leaf)
            .expect("discover should not error")
            .expect("should find one");
        assert_eq!(found.include, vec!["*.sql"]);
        assert_eq!(found.root, tree.root);
    }

    #[test]
    fn discover_starts_at_parent_when_given_a_file() {
        let tree = TempTree::new("file-start");
        tree.write("grebe.toml", "include = [\"*.sql\"]\n");
        let file = tree.root.join("some_query.sql"); // need not exist
        let found = discover(&file)
            .expect("discover should not error")
            .expect("should find one");
        assert_eq!(found.root, tree.root);
    }

    #[test]
    fn discover_prefers_grebe_toml_over_pyproject_toml() {
        let tree = TempTree::new("prefers-grebe");
        tree.write("grebe.toml", "include = [\"*.sql\"]\n");
        tree.write("pyproject.toml", "[tool.grebe]\ninclude = [\"*.py\"]\n");
        let found = discover(&tree.root)
            .expect("discover should not error")
            .expect("should find one");
        assert_eq!(found.include, vec!["*.sql"]);
    }

    #[test]
    fn discover_falls_back_to_pyproject_toml_with_tool_grebe() {
        let tree = TempTree::new("pyproject-hit");
        tree.write("pyproject.toml", "[tool.grebe]\ninclude = [\"*.py\"]\n");
        let found = discover(&tree.root)
            .expect("discover should not error")
            .expect("should find one");
        assert_eq!(found.include, vec!["*.py"]);
    }

    #[test]
    fn discover_skips_pyproject_toml_without_tool_grebe() {
        let tree = TempTree::new("pyproject-miss");
        tree.write("pyproject.toml", "[project]\nname = \"x\"\n");
        let leaf = tree.dir("a");
        let found = discover(&leaf).expect("discover should not error");
        assert!(found.is_none());
    }

    #[test]
    fn discover_returns_none_when_nothing_found() {
        let tree = TempTree::new("none-found");
        let leaf = tree.dir("a/b");
        let found = discover(&leaf).expect("discover should not error");
        assert!(found.is_none());
    }

    #[test]
    fn discover_propagates_parse_errors() {
        let tree = TempTree::new("propagates-error");
        tree.write("grebe.toml", "bogus-key = 1\n");
        let err = discover(&tree.root).expect_err("should error");
        assert_eq!(err.line, 1);
    }
}
