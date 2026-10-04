//! JSON-RPC dispatch, document store, and diagnostics: the loop `serve()`
//! runs over real stdio. The loop is generic over `impl BufRead` +
//! `impl Write`, so tests drive it from memory.
//!
//! Analysis is `grebe_rules::analysis::analyze`, the same function
//! `grebe check` calls: whole-file parse first, per-statement fallback with
//! span offsetting so one bad statement doesn't discard the rest of the
//! file's findings, and the same selection rule as `check --select`: with a
//! selection, exactly the listed codes; without one, every code whose
//! effective severity isn't `Off`. Each document's `grebe.toml` is
//! discovered from its own path, as the CLI discovers it from the first PATH.
//!
//! The selection comes from the editor as `grebe.select` -- read once from
//! `initialize`'s `initializationOptions.select`, and again on every
//! `workspace/didChangeConfiguration` (`settings.grebe.select`), so toggling
//! the setting re-lints open documents without an editor restart. A code
//! that doesn't exist in the registry is dropped from the selection rather
//! than failing the request -- unlike the CLI, which exits 2 on an unknown
//! `--select` code, this server has no request to fail and must keep
//! running -- and reported back with `window/showMessage` so a typo is
//! visible instead of silently inert.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use grebe_rules::Severity;
use grebe_rules::analysis::Selection;
use grebe_rules::config::Config;
#[cfg(test)]
use grebe_rules::detect::Finding;
use grebe_syntax::Span;

use crate::json::{self, Json};
use crate::position::{self, Encoding};
use crate::transport;

/// Run the server loop to completion. Returns the process exit code: `0`
/// only if the client completed the clean `shutdown` request then `exit`
/// notification sequence; `1` for any other way the connection ended
/// (premature EOF, `exit` without a prior `shutdown`).
pub fn run<R: BufRead, W: Write>(mut reader: R, mut writer: W) -> i32 {
    let mut state = State::default();
    loop {
        let body = match transport::read_message(&mut reader) {
            Ok(Some(b)) => b,
            Ok(None) => return exit_code(&state), // clean EOF
            Err(_) => return exit_code(&state),   // framing broke; nothing left to recover
        };
        let Ok(text) = std::str::from_utf8(&body) else {
            continue; // not UTF-8 -- malformed, drop it, keep serving
        };
        let Ok(msg) = json::parse(text) else {
            continue; // malformed JSON -- drop it, keep serving
        };
        if msg.as_object().is_none() {
            continue; // not a JSON-RPC object -- drop it
        }
        let method = msg.get("method").and_then(Json::as_str);
        let has_id = msg.get("id").is_some();
        let id = msg.get("id").cloned().unwrap_or(Json::Null);
        let params = msg.get("params").cloned().unwrap_or(Json::Null);

        match method {
            Some("initialize") => {
                let result = handle_initialize(&params, &mut state, &mut writer);
                respond(&mut writer, id, result);
            }
            Some("initialized") => {} // notification, nothing to do
            Some("shutdown") => {
                state.shutdown = true;
                respond(&mut writer, id, Json::Null);
            }
            Some("exit") => return exit_code(&state),
            Some("textDocument/didOpen") => handle_did_open(&params, &mut state, &mut writer),
            Some("textDocument/didChange") => {
                handle_did_change(&params, &mut state, &mut writer);
            }
            Some("textDocument/didSave") => handle_did_save(&params, &mut state, &mut writer),
            Some("textDocument/didClose") => {
                handle_did_close(&params, &mut state, &mut writer);
            }
            Some("textDocument/semanticTokens/full") => {
                let result = handle_semantic_tokens(&params, &state);
                respond(&mut writer, id, result);
            }
            Some("textDocument/codeAction") => {
                let result = handle_code_action(&params, &mut state, &mut writer);
                respond(&mut writer, id, result);
            }
            Some("textDocument/formatting") => {
                let result = handle_formatting(&params, &mut state, &mut writer);
                respond(&mut writer, id, result);
            }
            Some("grebe/statements") => {
                let result = handle_statements(&params, &state);
                respond(&mut writer, id, result);
            }
            Some("workspace/didChangeConfiguration") => {
                handle_did_change_configuration(&params, &mut state, &mut writer);
            }
            Some(_) => {
                if has_id {
                    respond_error(&mut writer, id, -32601, "method not found");
                }
                // else: unknown notification, ignore silently.
            }
            None => {
                if has_id {
                    respond_error(&mut writer, id, -32600, "invalid request");
                }
            }
        }
    }
}

fn exit_code(state: &State) -> i32 {
    i32::from(!state.shutdown)
}

/// Server-side session state: open documents and what's been negotiated.
#[derive(Default)]
struct State {
    documents: HashMap<String, String>,
    encoding: Encoding,
    shutdown: bool,
    /// `grebe.select`, normalised (trimmed, uppercased, unknown codes
    /// dropped). `None` means no selection is active -- the registry's
    /// default severities decide what fires, same as bare `grebe check`.
    /// `Some(_)` -- including `Some(vec![])` -- means only the listed codes
    /// fire, on or off by default alike, exactly `--select`'s semantics.
    select: Option<Vec<String>>,
    /// Distinct `grebe.toml` error messages already reported via
    /// `window/showMessage`, so a config error that keeps re-triggering (a
    /// document re-analysed on every keystroke) surfaces once, not on every
    /// request.
    reported_config_errors: HashSet<String>,
}

fn handle_initialize<W: Write>(params: &Json, state: &mut State, writer: &mut W) -> Json {
    let offered: Vec<&str> = params
        .get("capabilities")
        .and_then(|c| c.get("general"))
        .and_then(|g| g.get("positionEncodings"))
        .and_then(Json::as_array)
        .map(|arr| arr.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    state.encoding = position::negotiate(&offered);

    let raw_select = parse_select(
        params
            .get("initializationOptions")
            .and_then(|o| o.get("select")),
    );
    apply_select(raw_select, state, writer);

    Json::object(vec![(
        "capabilities".into(),
        Json::object(vec![
            // Full document sync (LSP `TextDocumentSyncKind.Full` = 1):
            // the client resends the whole text on every change, which costs
            // a larger payload and removes any need to apply range edits.
            ("textDocumentSync".into(), Json::num(1)),
            (
                "positionEncoding".into(),
                Json::str(state.encoding.as_str()),
            ),
            // Full-document, pull-based, no range, no deltas, no
            // modifiers. Encoding a whole document is cheap, and a `range`
            // provider would be a second code path that can disagree with
            // the first.
            (
                "semanticTokensProvider".into(),
                Json::object(vec![
                    (
                        "legend".into(),
                        Json::object(vec![
                            (
                                "tokenTypes".into(),
                                Json::Array(
                                    crate::semantic::LEGEND
                                        .iter()
                                        .map(|t| Json::str(*t))
                                        .collect(),
                                ),
                            ),
                            ("tokenModifiers".into(), Json::Array(vec![])),
                        ]),
                    ),
                    ("full".into(), Json::Bool(true)),
                ]),
            ),
            // Quick fixes. Declared as a plain `true` rather than with a
            // `codeActionKinds` list: we only ever produce `quickfix`, and
            // advertising the kinds buys nothing a client acts on.
            ("codeActionProvider".into(), Json::Bool(true)),
            // Whole-document formatting via `grebe_format::format`. There is
            // no range or on-type variant: the formatter only ever knows how
            // to lay out a complete document.
            ("documentFormattingProvider".into(), Json::Bool(true)),
        ]),
    )])
}

/// Read a `select` value (an array of rule-code strings) out of an
/// arbitrary JSON location. `None` when the key is absent or isn't an
/// array -- distinct from `Some(vec![])`, an array that was present but
/// empty. Codes are trimmed and uppercased here, same as the CLI's
/// `--select` parsing; whether each one actually names a rule is checked
/// later, in [`apply_select`].
fn parse_select(v: Option<&Json>) -> Option<Vec<String>> {
    v.and_then(Json::as_array).map(|arr| {
        arr.iter()
            .filter_map(Json::as_str)
            .map(|s| s.trim().to_ascii_uppercase())
            .collect()
    })
}

/// Install a freshly-parsed selection into `state`, dropping any code the
/// registry doesn't recognise and warning about it rather than failing --
/// an LSP server has no request to reject the way `grebe check --select`
/// can exit 2, and must keep serving.
fn apply_select<W: Write>(raw: Option<Vec<String>>, state: &mut State, writer: &mut W) {
    let Some(codes) = raw else {
        state.select = None;
        return;
    };
    let mut valid = Vec::new();
    let mut unknown = Vec::new();
    for code in grebe_rules::expand_select(codes) {
        if grebe_rules::lookup(&code).is_some() {
            valid.push(code);
        } else {
            unknown.push(code);
        }
    }
    if !unknown.is_empty() {
        notify_unknown_select(writer, &unknown);
    }
    state.select = Some(valid);
}

/// `window/showMessage`, type 2 (Warning): names the `grebe.select` codes
/// that don't match any rule in the registry, so a typo shows up in the
/// editor instead of just silently never matching a finding.
fn notify_unknown_select<W: Write>(writer: &mut W, unknown: &[String]) {
    let message = format!(
        "grebe.select: unknown rule code(s), ignored: {}",
        unknown.join(", ")
    );
    let params = Json::object(vec![
        ("type".into(), Json::num(2)),
        ("message".into(), Json::str(message)),
    ]);
    notify(writer, "window/showMessage", params);
}

/// Decode a `file://` URI to a filesystem path.
///
/// Handles the macOS/Linux `file:///abs/path` form (authority-less, so the
/// path starts right after the double slash of the scheme) and percent
/// escapes therein. A non-`file` URI, or a URI whose percent-escapes don't
/// decode to valid UTF-8, falls back to the current directory: `discover`
/// still runs, just rooted wherever the server process happens to be, which
/// is a saner failure than refusing to serve the request at all.
fn uri_to_path(uri: &str) -> PathBuf {
    let Some(rest) = uri.strip_prefix("file://") else {
        return std::env::current_dir().unwrap_or_default();
    };
    percent_decode(rest).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    )
}

/// Minimal `%XX` percent-decoding. Clients escape at least spaces (`%20`)
/// in `file://` URIs; any other escape in a path (non-ASCII bytes, `%2F`,
/// ...) decodes the same way, since this is a byte-level unescape, not a
/// component-aware one. A truncated or non-hex escape yields `None`.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `window/showMessage`, type 1 (Error), for a `grebe.toml` parse failure --
/// deduplicated on the message text so a config error that keeps re-firing
/// (the document is re-analysed on every keystroke) surfaces once rather
/// than spamming the client.
fn report_config_error<W: Write>(message: &str, state: &mut State, writer: &mut W) {
    if state.reported_config_errors.insert(message.to_string()) {
        let params = Json::object(vec![
            ("type".into(), Json::num(1)),
            ("message".into(), Json::str(message)),
        ]);
        notify(writer, "window/showMessage", params);
    }
}

/// Resolve `uri`'s config, per request: no cache, no filesystem watcher.
/// `discover` is a handful of `stat` calls, and re-running it on every
/// request is what makes a saved `grebe.toml` take effect immediately --
/// correctness (never serving a stale config) over the cleverness of
/// invalidating a cache correctly. A discovery error is reported once (see
/// [`report_config_error`]) and the document is then treated as configless,
/// same as if no `grebe.toml` existed at all -- the server keeps serving.
fn config_for_uri<W: Write>(uri: &str, state: &mut State, writer: &mut W) -> Option<Config> {
    let path = uri_to_path(uri);
    match grebe_rules::config::discover(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            report_config_error(&e.to_string(), state, writer);
            None
        }
    }
}

/// The `select` / `[severity]` / `foreign-heads` a request should analyse
/// with, merged from the editor-level `grebe.select` and the discovered
/// config. Owns its vectors so a borrowed [`Selection`] can be built from
/// it without fighting the borrow checker across a `config_for_uri` call.
struct Resolved {
    select: Option<Vec<String>>,
    severity: Vec<(String, Severity)>,
    foreign_heads: Vec<String>,
}

impl Resolved {
    fn selection(&self) -> Selection<'_> {
        Selection {
            select: self.select.as_deref(),
            severity: &self.severity,
            foreign_heads: &self.foreign_heads,
        }
    }
}

/// Merge `state`'s editor-level selection with `config`'s. The editor's
/// `grebe.select`, when the editor has set one at all (including an explicit
/// empty list), wins over the file's `select`, the same way `--select` wins
/// on the command line: it is the more specific, more recent choice.
/// `[severity]` and `foreign-heads` have no editor-level equivalent, so they
/// always come from the config alone.
fn resolve_selection(state: &State, config: Option<&Config>) -> Resolved {
    let select = state
        .select
        .clone()
        .or_else(|| config.and_then(|c| c.select.clone()));
    let severity = config.map(|c| c.severity.clone()).unwrap_or_default();
    let foreign_heads = config.map(|c| c.foreign_heads.clone()).unwrap_or_default();
    Resolved {
        select,
        severity,
        foreign_heads,
    }
}

/// The formatter `Options` for `config`'s `[format]` table, falling back
/// field-by-field to `grebe_format::Options::default()` -- a `grebe.toml`
/// that sets only `indent_size` still gets the built-in `inline_threshold`
/// and `keyword_case`.
fn format_options(config: Option<&Config>) -> grebe_format::Options {
    let default = grebe_format::Options::default();
    let Some(config) = config else {
        return default;
    };
    grebe_format::Options {
        indent_size: config.format.indent_size.unwrap_or(default.indent_size),
        inline_threshold: config
            .format
            .inline_threshold
            .unwrap_or(default.inline_threshold),
        keyword_case: match config.format.keyword_case {
            Some(grebe_rules::config::KeywordCase::Upper) => grebe_format::KeywordCase::Upper,
            Some(grebe_rules::config::KeywordCase::Lower) => grebe_format::KeywordCase::Lower,
            None => default.keyword_case,
        },
    }
}

/// `workspace/didChangeConfiguration`: re-read `grebe.select` and re-lint
/// every open document against the new selection, so flipping the setting
/// takes effect without an editor restart.
fn handle_did_change_configuration<W: Write>(params: &Json, state: &mut State, writer: &mut W) {
    let raw_select = parse_select(
        params
            .get("settings")
            .and_then(|s| s.get("grebe"))
            .and_then(|p| p.get("select")),
    );
    apply_select(raw_select, state, writer);

    let mut docs: Vec<(String, String)> = state
        .documents
        .iter()
        .map(|(uri, text)| (uri.clone(), text.clone()))
        .collect();
    docs.sort();
    for (uri, text) in &docs {
        publish(writer, uri, text, state);
    }
}

fn handle_did_open<W: Write>(params: &Json, state: &mut State, writer: &mut W) {
    let Some(doc) = params.get("textDocument") else {
        return;
    };
    let (Some(uri), Some(text)) = (
        doc.get("uri").and_then(Json::as_str),
        doc.get("text").and_then(Json::as_str),
    ) else {
        return;
    };
    state.documents.insert(uri.to_string(), text.to_string());
    publish(writer, uri, text, state);
}

fn handle_did_change<W: Write>(params: &Json, state: &mut State, writer: &mut W) {
    let Some(uri) = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
    else {
        return;
    };
    // Full sync: the client sends the whole new text as the single element
    // of `contentChanges` (no `range` field to apply incrementally).
    let Some(text) = params
        .get("contentChanges")
        .and_then(Json::as_array)
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("text"))
        .and_then(Json::as_str)
    else {
        return;
    };
    state.documents.insert(uri.to_string(), text.to_string());
    publish(writer, uri, text, state);
}

fn handle_did_save<W: Write>(params: &Json, state: &mut State, writer: &mut W) {
    let Some(uri) = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
    else {
        return;
    };
    // `didSave` may carry the full text (`includeText`); if it does, treat
    // it as the current content. Either way, re-analyse and republish.
    if let Some(text) = params.get("text").and_then(Json::as_str) {
        state.documents.insert(uri.to_string(), text.to_string());
    }
    let Some(text) = state.documents.get(uri).cloned() else {
        return;
    };
    publish(writer, uri, &text, state);
}

fn handle_did_close<W: Write>(params: &Json, state: &mut State, writer: &mut W) {
    let Some(uri) = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
    else {
        return;
    };
    state.documents.remove(uri);
    // Publish an empty set so a closed document's squiggles don't linger.
    publish_diagnostics(writer, uri, &[]);
}

/// A `select`-only view of `grebe_rules::analysis::analyze_source`, for the
/// selection-semantics unit tests below (`grebe_rules`'s own test suite is
/// the source of truth for those semantics; what this file needs to prove is
/// only that the server delegates rather than reimplementing). Request
/// handlers also resolve `[severity]` and `foreign-heads` from config (see
/// [`resolve_selection`]) and call `grebe_rules::analysis::analyze`
/// directly, so this helper has no caller outside `#[cfg(test)]`.
#[cfg(test)]
fn analyze(src: &str, select: &Option<Vec<String>>) -> Vec<Finding> {
    grebe_rules::analysis::analyze_source(src, select.as_deref())
}

/// `textDocument/semanticTokens/full`.
///
/// A request for a document the server has never seen answers with an empty
/// token list rather than an error: the client is not wrong to ask, it just
/// raced `didOpen`, and an error would surface to the user as a broken editor
/// rather than as one frame of missing colour.
fn handle_semantic_tokens(params: &Json, state: &State) -> Json {
    let text = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
        .and_then(|uri| state.documents.get(uri));

    let data = text.map_or_else(Vec::new, |t| crate::semantic::encode(t, state.encoding));
    Json::object(vec![(
        "data".into(),
        Json::Array(data.into_iter().map(|n| Json::num(f64::from(n))).collect()),
    )])
}

/// `textDocument/codeAction`.
///
/// The findings are recomputed rather than cached from the last `publish`.
/// That costs a parse, and buys the guarantee that an action never edits a
/// span the document no longer has: the client can send this against a buffer
/// it has already changed, and a stale fix span would corrupt the file.
fn handle_code_action<W: Write>(params: &Json, state: &mut State, writer: &mut W) -> Json {
    let Some(uri) = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
    else {
        return Json::Array(vec![]);
    };
    let uri = uri.to_string();
    let Some(text) = state.documents.get(&uri).cloned() else {
        return Json::Array(vec![]);
    };
    let encoding = state.encoding;

    let num = |v: Option<&Json>| v.and_then(Json::as_f64).unwrap_or(0.0) as u32;
    let r = params.get("range");
    let start = r.and_then(|r| r.get("start"));
    let end = r.and_then(|r| r.get("end"));
    let to_byte = |p: Option<&Json>| {
        position::position_to_byte(
            &text,
            num(p.and_then(|p| p.get("line"))),
            num(p.and_then(|p| p.get("character"))),
            encoding,
        )
    };
    let range = Span::new(to_byte(start), to_byte(end));

    let config = config_for_uri(&uri, state, writer);
    let resolved = resolve_selection(state, config.as_ref());
    let findings = grebe_rules::analysis::analyze(&text, &resolved.selection());
    Json::Array(crate::code_action::actions(
        &uri, &text, &findings, range, encoding,
    ))
}

fn severity_to_lsp(sev: Severity) -> f64 {
    match sev {
        Severity::Error => 1.0,
        Severity::Warning => 2.0,
        Severity::Info => 3.0,
        // A finding at effective severity `Off` only reaches here when a
        // selection names its code (see `Selection::enabled`). Once opted
        // in, it's a real diagnostic the user asked to see, not a suppressed
        // one -- Warning, not Info, so it doesn't read as lower priority
        // than the rules that ship on by default.
        Severity::Off => 2.0,
    }
}

fn publish<W: Write>(writer: &mut W, uri: &str, text: &str, state: &mut State) {
    let encoding = state.encoding;
    let config = config_for_uri(uri, state, writer);
    let resolved = resolve_selection(state, config.as_ref());
    let selection = resolved.selection();
    let findings = grebe_rules::analysis::analyze(text, &selection);
    let diagnostics: Vec<Json> = findings
        .iter()
        .filter_map(|f| {
            let rule = grebe_rules::lookup(f.code)?;
            let (sl, sc) = position::byte_to_position(text, f.span.start, encoding);
            let (el, ec) = position::byte_to_position(text, f.span.end, encoding);
            Some(Json::object(vec![
                (
                    "range".into(),
                    Json::object(vec![
                        ("start".into(), pos(sl, sc)),
                        ("end".into(), pos(el, ec)),
                    ]),
                ),
                (
                    "severity".into(),
                    Json::num(severity_to_lsp(selection.severity_of(f.code))),
                ),
                ("code".into(), Json::str(rule.code)),
                ("source".into(), Json::str("grebe")),
                ("message".into(), Json::str(rule.message)),
            ]))
        })
        .collect();
    publish_diagnostics(writer, uri, &diagnostics);
}

/// `textDocument/formatting`.
///
/// The request's own `options` (`tabSize`, `insertSpaces`, ...) are ignored
/// on purpose: grebe's formatting knobs are `grebe.toml`'s `[format]` table,
/// the same source `grebe format` reads, not whatever the editor's generic
/// per-language settings happen to say -- one style per project, not one
/// style per editor.
///
/// A document the server has never seen (raced `didOpen`, or closed since)
/// answers `[]`, same posture as `textDocument/codeAction` and
/// `semanticTokens/full`: not wrong to ask, nothing yet to answer with.
fn handle_formatting<W: Write>(params: &Json, state: &mut State, writer: &mut W) -> Json {
    let Some(uri) = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
    else {
        return Json::Array(vec![]);
    };
    let uri = uri.to_string();
    let Some(text) = state.documents.get(&uri).cloned() else {
        return Json::Array(vec![]);
    };
    let encoding = state.encoding;

    let config = config_for_uri(&uri, state, writer);
    let opts = format_options(config.as_ref());
    let formatted = grebe_format::format(&text, &opts);
    if formatted == text {
        return Json::Array(vec![]);
    }

    let (el, ec) = position::byte_to_position(&text, text.len() as u32, encoding);
    let edit = Json::object(vec![
        (
            "range".into(),
            Json::object(vec![
                ("start".into(), pos(0, 0)),
                ("end".into(), pos(el, ec)),
            ]),
        ),
        ("newText".into(), Json::str(formatted)),
    ]);
    Json::Array(vec![edit])
}

/// `grebe/statements`: the statements a run should execute, as
/// `[{ range, text, kind }]` in source order (`kind`: see
/// [`crate::statements::kind`]). `params.range` is optional; absent,
/// every statement in the document. The text is sent too, so the client runs
/// exactly the bytes the server split, not a re-slice of a buffer that may
/// have changed since.
fn handle_statements(params: &Json, state: &State) -> Json {
    let Some(text) = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
        .and_then(|uri| state.documents.get(uri))
    else {
        return Json::Array(vec![]);
    };
    let encoding = state.encoding;
    let num = |v: Option<&Json>| v.and_then(Json::as_f64).unwrap_or(0.0) as u32;
    let to_byte = |p: &Json| {
        position::position_to_byte(text, num(p.get("line")), num(p.get("character")), encoding)
    };
    let spans = match params
        .get("range")
        .and_then(|r| Some((r.get("start")?, r.get("end")?)))
    {
        Some((start, end)) => {
            crate::statements::select(text, Span::new(to_byte(start), to_byte(end)))
        }
        None => crate::statements::all(text),
    };
    Json::Array(
        spans
            .into_iter()
            .map(|s| {
                let (sl, sc) = position::byte_to_position(text, s.start, encoding);
                let (el, ec) = position::byte_to_position(text, s.end, encoding);
                Json::object(vec![
                    (
                        "range".into(),
                        Json::object(vec![
                            ("start".into(), pos(sl, sc)),
                            ("end".into(), pos(el, ec)),
                        ]),
                    ),
                    (
                        "text".into(),
                        Json::str(text[s.start as usize..s.end as usize].to_string()),
                    ),
                    (
                        "kind".into(),
                        Json::str(crate::statements::kind(
                            &text[s.start as usize..s.end as usize],
                        )),
                    ),
                ])
            })
            .collect(),
    )
}

fn pos(line: u32, character: u32) -> Json {
    Json::object(vec![
        ("line".into(), Json::num(line)),
        ("character".into(), Json::num(character)),
    ])
}

fn publish_diagnostics<W: Write>(writer: &mut W, uri: &str, diagnostics: &[Json]) {
    let params = Json::object(vec![
        ("uri".into(), Json::str(uri)),
        ("diagnostics".into(), Json::Array(diagnostics.to_vec())),
    ]);
    notify(writer, "textDocument/publishDiagnostics", params);
}

fn respond<W: Write>(writer: &mut W, id: Json, result: Json) {
    let msg = Json::object(vec![
        ("jsonrpc".into(), Json::str("2.0")),
        ("id".into(), id),
        ("result".into(), result),
    ]);
    let _ = transport::write_message(writer, &json::to_string(&msg));
}

fn respond_error<W: Write>(writer: &mut W, id: Json, code: i32, message: &str) {
    let msg = Json::object(vec![
        ("jsonrpc".into(), Json::str("2.0")),
        ("id".into(), id),
        (
            "error".into(),
            Json::object(vec![
                ("code".into(), Json::num(code)),
                ("message".into(), Json::str(message)),
            ]),
        ),
    ]);
    let _ = transport::write_message(writer, &json::to_string(&msg));
}

fn notify<W: Write>(writer: &mut W, method: &str, params: Json) {
    let msg = Json::object(vec![
        ("jsonrpc".into(), Json::str("2.0")),
        ("method".into(), Json::str(method)),
        ("params".into(), params),
    ]);
    let _ = transport::write_message(writer, &json::to_string(&msg));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_mapping_matches_registry() {
        assert_eq!(severity_to_lsp(Severity::Error), 1.0);
        assert_eq!(severity_to_lsp(Severity::Warning), 2.0);
        assert_eq!(severity_to_lsp(Severity::Info), 3.0);
        // Off only ever reaches severity_to_lsp via an explicit selection
        // (`Selection::enabled` gates it), where it reads as a normal Warning.
        assert_eq!(severity_to_lsp(Severity::Off), 2.0);
    }

    // Selection semantics themselves are `grebe_rules::analysis`'s and are
    // tested there. What matters here is that the server delegates to it and
    // does not reintroduce a second, drifting copy.
    #[test]
    fn no_selection_leaves_off_by_default_rules_silent() {
        // MOD025 (select-star) ships Off; MOD001 (count-star) ships on.
        let none: Option<Vec<String>> = None;
        assert!(analyze("SELECT * FROM t", &none).is_empty());
        assert!(
            analyze("SELECT count(*) FROM t", &none)
                .iter()
                .any(|f| f.code == "MOD001")
        );
    }

    #[test]
    fn a_selection_turns_one_on_and_restricts_to_it() {
        let sel = Some(vec!["MOD025".to_string()]);
        assert!(
            analyze("SELECT * FROM t", &sel)
                .iter()
                .any(|f| f.code == "MOD025")
        );
        // `--select` restricts as well as enables: MOD001 is on by default but
        // not listed, so it must not fire.
        assert!(analyze("SELECT count(*) FROM t", &sel).is_empty());
    }

    #[test]
    fn an_empty_or_unknown_selection_enables_nothing_and_does_not_panic() {
        let empty = Some(Vec::new());
        assert!(analyze("SELECT count(*) FROM t", &empty).is_empty());
        let bogus = Some(vec!["NOPE999".to_string()]);
        assert!(analyze("SELECT * FROM t", &bogus).is_empty());
    }

    #[test]
    fn the_server_honours_suppression_comments() {
        let none: Option<Vec<String>> = None;
        let src = "-- grebe: ignore[MOD001]\nSELECT count(*) FROM t;";
        assert!(analyze(src, &none).is_empty());
    }

    #[test]
    fn analyze_finds_mod001_count_star() {
        let findings = analyze("SELECT count(*) FROM t", &None);
        assert!(findings.iter().any(|f| f.code == "MOD001"));
    }

    #[test]
    fn analyze_selection_surfaces_an_off_by_default_rule() {
        // MOD025 (select-star) is silent without a selection...
        let no_sel = analyze("SELECT * FROM t", &None);
        assert!(!no_sel.iter().any(|f| f.code == "MOD025"));
        // ...and fires once MOD025 is selected.
        let sel = Some(vec!["MOD025".to_string()]);
        let with_sel = analyze("SELECT * FROM t", &sel);
        assert!(with_sel.iter().any(|f| f.code == "MOD025"));
    }

    #[test]
    fn analyze_recovers_per_statement_on_a_bad_statement() {
        // One statement that won't parse must not swallow findings from a
        // sibling statement that does.
        let src = "SELECT count(*) FROM t; THIS IS NOT SQL AT ALL (((;";
        let findings = analyze(src, &None);
        assert!(findings.iter().any(|f| f.code == "MOD001"));
    }

    #[test]
    fn analyze_offsets_spans_in_the_fallback_path() {
        let bad_stmt = "THIS IS NOT SQL AT ALL";
        let src = format!("{bad_stmt};\nSELECT count(*) FROM t");
        let findings = analyze(&src, &None);
        let f = findings
            .iter()
            .find(|f| f.code == "MOD001")
            .expect("MOD001 finding");
        let start = f.span.start as usize;
        let end = f.span.end as usize;
        // MOD001's span is exactly the offending `*`; it must land inside
        // the second statement (well past the first statement's own
        // length), correctly offset by the fallback path.
        assert!(start > bad_stmt.len());
        assert_eq!(&src[start..end], "*");
    }

    #[test]
    fn parse_select_distinguishes_absent_from_empty() {
        assert_eq!(parse_select(None), None);
        let empty = Json::Array(Vec::new());
        assert_eq!(parse_select(Some(&empty)), Some(Vec::new()));
        let not_array = Json::str("MOD001");
        assert_eq!(parse_select(Some(&not_array)), None);
    }

    #[test]
    fn parse_select_trims_and_uppercases() {
        let raw = Json::Array(vec![Json::str(" mod025 "), Json::str("mod001")]);
        assert_eq!(
            parse_select(Some(&raw)),
            Some(vec!["MOD025".to_string(), "MOD001".to_string()])
        );
    }

    #[test]
    fn apply_select_drops_unknown_codes_and_warns() {
        let mut state = State::default();
        let mut out: Vec<u8> = Vec::new();
        apply_select(
            Some(vec!["MOD001".to_string(), "NOPE999".to_string()]),
            &mut state,
            &mut out,
        );
        assert_eq!(state.select, Some(vec!["MOD001".to_string()]));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("window/showMessage"));
        assert!(text.contains("NOPE999"));
    }

    #[test]
    fn apply_select_absent_selection_does_not_warn() {
        let mut state = State::default();
        let mut out: Vec<u8> = Vec::new();
        apply_select(None, &mut state, &mut out);
        assert_eq!(state.select, None);
        assert!(out.is_empty());
    }

    #[test]
    fn apply_select_expands_all_with_no_warning() {
        let mut state = State::default();
        let mut out: Vec<u8> = Vec::new();
        apply_select(Some(vec!["ALL".to_string()]), &mut state, &mut out);
        let select = state.select.expect("selection set");
        assert!(select.len() > 1, "{select:?}");
        assert!(select.contains(&"MOD010".to_string()), "{select:?}");
        assert!(
            out.is_empty(),
            "ALL is a real expansion, not an unknown code"
        );
    }
}
