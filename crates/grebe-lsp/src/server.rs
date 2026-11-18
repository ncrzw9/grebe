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
use std::rc::Rc;
use std::time::SystemTime;

use grebe_rules::Severity;
use grebe_rules::analysis::Selection;
use grebe_rules::catalog::{Catalog, Part};
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
///
/// Messages are read on their own thread and handled in batches: whatever
/// has arrived by the time the previous batch is done. A document's
/// diagnostics are published once per batch, after its last change in it,
/// so typing faster than the server can lint never queues a lint per
/// keystroke -- each pass lints only the newest text. Requests are answered
/// as they come, before that publishing, and a request whose
/// `$/cancelRequest` is in the same batch is answered as cancelled without
/// being run.
pub fn run<R: BufRead + Send + 'static, W: Write>(mut reader: R, mut writer: W) -> i32 {
    let (tx, rx) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
    std::thread::spawn(move || {
        loop {
            match transport::read_message(&mut reader) {
                Ok(Some(body)) => {
                    if tx.send(Some(body)).is_err() {
                        return;
                    }
                }
                // Clean EOF, or framing that broke: nothing left to read.
                Ok(None) | Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
            }
        }
    });

    let mut state = State::default();
    loop {
        let Ok(Some(first)) = rx.recv() else {
            return exit_code(&state);
        };
        let mut batch = vec![first];
        let mut ended = false;
        while let Ok(next) = rx.try_recv() {
            match next {
                Some(body) => batch.push(body),
                None => {
                    ended = true;
                    break;
                }
            }
        }
        if let Some(code) = process_batch(&batch, &mut state, &mut writer) {
            return code;
        }
        if ended {
            return exit_code(&state);
        }
    }
}

/// Handle one batch of message bodies, then publish what it changed.
/// `Some(code)` when it ends the session (`exit`).
fn process_batch<W: Write>(batch: &[Vec<u8>], state: &mut State, writer: &mut W) -> Option<i32> {
    let messages: Vec<Option<Json>> = batch
        .iter()
        .map(|body| {
            std::str::from_utf8(body)
                .ok()
                .and_then(|t| json::parse(t).ok())
        })
        .collect();
    let cancelled: HashSet<String> = messages
        .iter()
        .flatten()
        .filter(|m| m.get("method").and_then(Json::as_str) == Some("$/cancelRequest"))
        .filter_map(|m| m.get("params")?.get("id").map(json::to_string))
        .collect();
    for msg in messages {
        if let Some(code) = handle_message(msg, &cancelled, state, writer) {
            publish_dirty(state, writer);
            return Some(code);
        }
    }
    publish_dirty(state, writer);
    None
}

/// Handle one message. `Some(code)` when it ends the session (`exit`).
fn handle_message<W: Write>(
    msg: Option<Json>,
    cancelled: &HashSet<String>,
    state: &mut State,
    writer: &mut W,
) -> Option<i32> {
    let Some(mut msg) = msg else {
        // Not UTF-8 or not JSON: JSON-RPC's parse error, with a null id
        // since the request's own could not be read.
        respond_error(writer, Json::Null, -32700, "parse error");
        return None;
    };
    // Taken out of the message, not copied: a catalog's params are most of
    // a hundred megabytes.
    let params = match &mut msg {
        Json::Object(pairs) => pairs
            .iter_mut()
            .find(|(k, _)| k == "params")
            .map_or(Json::Null, |(_, v)| std::mem::replace(v, Json::Null)),
        _ => Json::Null,
    };
    if msg.as_object().is_none() {
        respond_error(writer, Json::Null, -32600, "invalid request");
        return None;
    }
    let method = msg.get("method").and_then(Json::as_str);
    let has_id = msg.get("id").is_some();
    let id = msg.get("id").cloned().unwrap_or(Json::Null);

    if method == Some("exit") {
        return Some(exit_code(state));
    }
    // The lifecycle: nothing but `initialize` before it, once; nothing
    // but `exit` after `shutdown`.
    match (method, state.initialized, state.shutdown) {
        (Some("initialize"), true, _) => {
            respond_error(writer, id, -32600, "already initialized");
            return None;
        }
        (Some("initialize"), false, _) => {}
        (_, false, _) => {
            if has_id {
                respond_error(writer, id, -32002, "server not initialized");
            }
            return None;
        }
        (_, _, true) => {
            if has_id {
                respond_error(writer, id, -32600, "server is shutting down");
            }
            return None;
        }
        _ => {}
    }
    if has_id && cancelled.contains(&json::to_string(&id)) {
        respond_error(writer, id, -32800, "request cancelled");
        return None;
    }

    match method {
        Some("initialize") => {
            let result = handle_initialize(&params, state, writer);
            state.initialized = true;
            respond(writer, id, result);
        }
        Some("initialized" | "$/cancelRequest") => {}
        Some("shutdown") => {
            state.shutdown = true;
            respond(writer, id, Json::Null);
        }
        Some("textDocument/didOpen") => handle_did_open(&params, state),
        Some("textDocument/didChange") => handle_did_change(&params, state),
        Some("textDocument/didSave") => handle_did_save(&params, state),
        Some("textDocument/didClose") => handle_did_close(&params, state, writer),
        Some("textDocument/semanticTokens/full") => {
            let result = handle_semantic_tokens(&params, state);
            respond(writer, id, result);
        }
        Some("textDocument/codeAction") => {
            let result = handle_code_action(&params, state, writer);
            respond(writer, id, result);
        }
        Some("textDocument/formatting") => {
            let result = handle_formatting(&params, state, writer);
            respond(writer, id, result);
        }
        Some("grebe/statements") => {
            let result = handle_statements(&params, state);
            respond(writer, id, result);
        }
        Some("grebe/friendly") => {
            let result = Json::Array(friendly_edits(&params, state, writer));
            respond(writer, id, result);
        }
        Some("textDocument/completion") => {
            let result = handle_completion(&params, state, writer);
            respond(writer, id, result);
        }
        Some("textDocument/hover") => {
            let result = handle_hover(&params, state, writer);
            respond(writer, id, result);
        }
        Some("grebe/catalog") => handle_catalog(&params, state, writer),
        Some("workspace/didChangeConfiguration") => {
            handle_did_change_configuration(&params, state, writer);
        }
        Some(_) => {
            if has_id {
                respond_error(writer, id, -32601, "method not found");
            }
            // else: unknown notification, ignore silently.
        }
        None => {
            if has_id {
                respond_error(writer, id, -32600, "invalid request");
            }
        }
    }
    None
}

/// Lint and publish every document changed since the last publish, once
/// each, in the order they were first changed.
fn publish_dirty<W: Write>(state: &mut State, writer: &mut W) {
    for uri in std::mem::take(&mut state.dirty) {
        if let Some(text) = state.documents.get(&uri).cloned() {
            publish(writer, &uri, &text, state);
        }
    }
}

/// Mark `uri` for [`publish_dirty`].
fn mark_dirty(state: &mut State, uri: &str) {
    if !state.dirty.iter().any(|u| u == uri) {
        state.dirty.push(uri.to_string());
    }
}

fn exit_code(state: &State) -> i32 {
    i32::from(!state.shutdown)
}

/// A catalog file as last read: the modification time and size it had, and
/// the catalog, or `None` if it could not be read as one.
struct CatalogFile {
    modified: Option<SystemTime>,
    len: u64,
    catalog: Option<Rc<Catalog>>,
}

/// Server-side session state: open documents and what's been negotiated.
#[derive(Default)]
struct State {
    documents: HashMap<String, String>,
    /// Each open document's latest version, from didOpen / didChange:
    /// a change with an older version is stale and ignored, and diagnostics
    /// say which version they are for.
    versions: HashMap<String, f64>,
    /// Documents changed since their diagnostics were last published.
    dirty: Vec<String>,
    encoding: Encoding,
    initialized: bool,
    shutdown: bool,
    /// `grebe.select`, normalised (trimmed, uppercased, unknown codes
    /// dropped). `None` means no selection is active -- the registry's
    /// default severities decide what fires, same as bare `grebe check`.
    /// `Some(_)` -- including `Some(vec![])` -- means only the listed codes
    /// fire, on or off by default alike, exactly `--select`'s semantics.
    select: Option<Vec<String>>,
    /// The `grebe.toml` error last reported via `window/showMessage`, per
    /// config file, so an error that keeps re-triggering (a document
    /// re-analysed on every keystroke) surfaces once, not on every request.
    /// Forgotten when that file next loads cleanly: fixing it and breaking
    /// it again is news again.
    reported_config_errors: HashMap<PathBuf, String>,
    /// The catalog the client last sent with `grebe/catalog` and no `uri`:
    /// the one every document is checked and completed against, unless it
    /// has one of its own.
    catalog: Option<Rc<Catalog>>,
    /// What `grebe/catalog` said about one document (a `uri`): an editor
    /// that runs different files in different sessions sends each file its
    /// session's catalog. `None` is "this document has no catalog" (its
    /// session is not running): it is not checked, and does not fall back
    /// to the shared one.
    document_catalogs: HashMap<String, Option<Rc<Catalog>>>,
    /// Catalogs the client sent under a name (`grebe/catalog` with
    /// `catalog: NAME`), and the documents told to use one by name: several
    /// files that run in one session share its catalog without it being
    /// sent once per file. A document whose named catalog is not there has
    /// no catalog.
    named_catalogs: HashMap<String, Rc<Catalog>>,
    document_catalog_names: HashMap<String, String>,
    /// Catalog files named by a `grebe.toml` (`catalog = "..."`), as last
    /// read, with the modification time and size they had: a client that
    /// sends no catalog still gets the one the project configured, and a
    /// rewritten file is picked up at the next lint without being re-read
    /// on every keystroke.
    catalog_files: HashMap<PathBuf, CatalogFile>,
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
            // Quick fixes, and one source action: the friendly SQL rewrite
            // (see `crate::friendly`). Its kind is listed so a client knows
            // it can be asked for by name, in a "source action" menu or on
            // save.
            (
                "codeActionProvider".into(),
                Json::object(vec![(
                    "codeActionKinds".into(),
                    Json::Array(vec![
                        Json::str("quickfix"),
                        Json::str(crate::friendly::ACTION_KIND),
                        // "Expand * to columns", with a catalog.
                        Json::str(crate::expand::ACTION_KIND),
                    ]),
                )]),
            ),
            // Whole-document formatting via `grebe_format::format`. There is
            // no range or on-type variant: the formatter only ever knows how
            // to lay out a complete document.
            ("documentFormattingProvider".into(), Json::Bool(true)),
            // Names from the catalog (when one was sent or configured), the
            // statement's own relations, keywords and functions. `.` asks
            // for a relation's columns or a schema's tables.
            (
                "completionProvider".into(),
                Json::object(vec![(
                    "triggerCharacters".into(),
                    Json::Array(vec![Json::str(".")]),
                )]),
            ),
            // What the catalog knows of the table, alias or column under
            // the cursor; nothing without a catalog.
            ("hoverProvider".into(), Json::Bool(true)),
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
/// A non-`file` URI, or a URI whose percent-escapes don't decode to valid
/// UTF-8, falls back to the current directory: `discover` still runs, just
/// rooted wherever the server process happens to be, which is a saner
/// failure than refusing to serve the request at all.
fn uri_to_path(uri: &str) -> PathBuf {
    file_uri_path(uri, cfg!(windows)).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    )
}

/// The path a `file://` URI names, on Windows or elsewhere.
///
/// - `file:///abs/path` (no authority): `/abs/path`, percent escapes
///   decoded.
/// - On Windows, `file:///c%3A/x.sql` or `file:///C:/x.sql`: `C:/x.sql`.
///   Left as `/c:/x.sql` it is no path at all, and `grebe.toml` was never
///   found.
/// - On Windows, `file://server/share/x.sql`: the UNC path
///   `\\server/share/x.sql`.
fn file_uri_path(uri: &str, windows: bool) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = percent_decode(rest)?;
    if !windows {
        return Some(path);
    }
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        return Some(path[1..].to_string());
    }
    if !path.is_empty() && !path.starts_with('/') {
        return Some(format!("\\\\{path}"));
    }
    Some(path)
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
fn report_config_error<W: Write>(
    e: &grebe_rules::config::ConfigError,
    state: &mut State,
    writer: &mut W,
) {
    let message = e.to_string();
    if state.reported_config_errors.get(&e.path) != Some(&message) {
        state
            .reported_config_errors
            .insert(e.path.clone(), message.clone());
        let params = Json::object(vec![
            ("type".into(), Json::num(1)),
            ("message".into(), Json::str(&message)),
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
        Ok(cfg) => {
            if let Some(c) = &cfg {
                state.reported_config_errors.remove(&c.path);
            }
            cfg
        }
        Err(e) => {
            report_config_error(&e, state, writer);
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
    catalog: Option<Rc<Catalog>>,
    catalog_options: grebe_rules::catalog::Options,
}

impl Resolved {
    fn selection(&self) -> Selection<'_> {
        Selection {
            select: self.select.as_deref(),
            severity: &self.severity,
            foreign_heads: &self.foreign_heads,
            catalog: self.catalog.as_deref(),
            catalog_options: self.catalog_options,
        }
    }
}

/// The catalog `uri` is checked and completed against: what was sent for
/// that document (which may be "none"), else the one sent for every
/// document, else the file its `grebe.toml` names. `None` when there is
/// none, and then the CAT rules do not run.
fn catalog_for_uri<W: Write>(
    uri: &str,
    config: Option<&Config>,
    state: &mut State,
    writer: &mut W,
) -> Option<Rc<Catalog>> {
    if let Some(name) = state.document_catalog_names.get(uri) {
        return state.named_catalogs.get(name).cloned();
    }
    if let Some(own) = state.document_catalogs.get(uri) {
        return own.clone();
    }
    if let Some(c) = &state.catalog {
        return Some(Rc::clone(c));
    }
    let path = config?.catalog.as_ref()?;
    let meta = std::fs::metadata(path).ok();
    let stamp = (
        meta.as_ref().and_then(|m| m.modified().ok()),
        meta.as_ref().map_or(0, std::fs::Metadata::len),
    );
    if let Some(f) = state.catalog_files.get(path) {
        if (f.modified, f.len) == stamp {
            return f.catalog.clone();
        }
    }
    let loaded = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| Catalog::from_json_text(&text));
    let catalog = match loaded {
        Ok(c) => Some(Rc::new(c)),
        Err(e) => {
            // Said once per version of the file, not once per keystroke.
            let params = Json::object(vec![
                ("type".into(), Json::num(2)),
                (
                    "message".into(),
                    Json::str(format!(
                        "grebe: could not read the catalog {}: {e}",
                        path.display()
                    )),
                ),
            ]);
            notify(writer, "window/showMessage", params);
            None
        }
    };
    state.catalog_files.insert(
        path.clone(),
        CatalogFile {
            modified: stamp.0,
            len: stamp.1,
            catalog: catalog.clone(),
        },
    );
    catalog
}

/// Merge `state`'s editor-level selection with `config`'s. The editor's
/// `grebe.select`, when the editor has set one at all (including an explicit
/// empty list), wins over the file's `select`, the same way `--select` wins
/// on the command line: it is the more specific, more recent choice.
/// `[severity]` and `foreign-heads` have no editor-level equivalent, so they
/// always come from the config alone.
fn resolve_selection<W: Write>(
    uri: &str,
    state: &mut State,
    config: Option<&Config>,
    writer: &mut W,
) -> Resolved {
    let catalog = catalog_for_uri(uri, config, state, writer);
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
        catalog,
        catalog_options: config.map(Config::catalog_options).unwrap_or_default(),
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
        function_paren_space: config
            .format
            .function_paren_space
            .unwrap_or(default.function_paren_space),
        keyword_paren_space: config
            .format
            .keyword_paren_space
            .unwrap_or(default.keyword_paren_space),
        semicolon: match config.format.semicolon {
            Some(grebe_rules::config::Semicolon::Attached) => grebe_format::Semicolon::Attached,
            Some(grebe_rules::config::Semicolon::Spaced) => grebe_format::Semicolon::Spaced,
            Some(grebe_rules::config::Semicolon::Newline) => grebe_format::Semicolon::Newline,
            None => default.semicolon,
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

fn handle_did_open(params: &Json, state: &mut State) {
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
    match doc.get("version").and_then(Json::as_f64) {
        Some(v) => state.versions.insert(uri.to_string(), v),
        None => state.versions.remove(uri),
    };
    mark_dirty(state, uri);
}

fn handle_did_change(params: &Json, state: &mut State) {
    let doc = params.get("textDocument");
    let Some(uri) = doc.and_then(|d| d.get("uri")).and_then(Json::as_str) else {
        return;
    };
    // An older version than the one we have arrived late: applying it
    // would replace newer text with older.
    let version = doc.and_then(|d| d.get("version")).and_then(Json::as_f64);
    if let (Some(v), Some(&have)) = (version, state.versions.get(uri)) {
        if v <= have {
            return;
        }
    }
    // A change to a document that is not open (it arrived after didClose)
    // must not bring it back and publish diagnostics for a closed file.
    if !state.documents.contains_key(uri) {
        return;
    }
    // Full sync: each element of `contentChanges` is the whole new text (no
    // `range` to apply incrementally); they apply in order, so the last one
    // is the document now.
    let Some(text) = params
        .get("contentChanges")
        .and_then(Json::as_array)
        .and_then(|arr| arr.last())
        .and_then(|c| c.get("text"))
        .and_then(Json::as_str)
    else {
        return;
    };
    state.documents.insert(uri.to_string(), text.to_string());
    if let Some(v) = version {
        state.versions.insert(uri.to_string(), v);
    }
    mark_dirty(state, uri);
}

fn handle_did_save(params: &Json, state: &mut State) {
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
    if state.documents.contains_key(uri) {
        mark_dirty(state, uri);
    }
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
    state.versions.remove(uri);
    state.document_catalogs.remove(uri);
    state.document_catalog_names.remove(uri);
    state.dirty.retain(|u| u != uri);
    // Publish an empty set so a closed document's squiggles don't linger.
    publish_diagnostics(writer, uri, &[], None);
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
    let resolved = resolve_selection(&uri, state, config.as_ref(), writer);
    let findings = grebe_rules::analysis::analyze(&text, &resolved.selection());
    let mut actions = crate::code_action::actions(&uri, &text, &findings, range, encoding);
    // The friendly SQL rewrite, only when the client asks for it by kind:
    // it rewrites the whole document, which is not what a lightbulb is for.
    let only = params.get("context").and_then(|c| c.get("only"));
    if crate::friendly::asked_for(only) {
        let whole = Json::object(vec![(
            "textDocument".into(),
            Json::object(vec![("uri".into(), Json::str(&uri))]),
        )]);
        let edits = friendly_edits(&whole, state, writer);
        actions.extend(crate::friendly::action(&uri, edits));
    }
    // A `SELECT *` written out as its columns: a rewrite, offered where
    // refactorings are, when the catalog knows every table of the query.
    if let Some(catalog) = resolved
        .catalog
        .as_deref()
        .filter(|_| crate::expand::asked_for(only))
    {
        actions.extend(crate::expand::action(&uri, &text, range, catalog, encoding));
    }
    Json::Array(actions)
}

/// `grebe/friendly`: the text edits that rewrite a document as friendly
/// SQL (see [`crate::friendly`]). `params.range` is optional: with one, the
/// statements it overlaps, picked as `grebe/statements` picks them; absent,
/// the whole document. `[]` when there is nothing to rewrite, or for a
/// document the server has never seen. Formatting follows the document's
/// `grebe.toml`, as `textDocument/formatting` does.
fn friendly_edits<W: Write>(params: &Json, state: &mut State, writer: &mut W) -> Vec<Json> {
    let Some(uri) = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
    else {
        return Vec::new();
    };
    let uri = uri.to_string();
    let Some(text) = state.documents.get(&uri).cloned() else {
        return Vec::new();
    };
    let encoding = state.encoding;
    let num = |v: Option<&Json>| v.and_then(Json::as_f64).unwrap_or(0.0) as u32;
    let to_byte = |p: &Json| {
        position::position_to_byte(&text, num(p.get("line")), num(p.get("character")), encoding)
    };
    let statements = params
        .get("range")
        .and_then(|r| Some((r.get("start")?, r.get("end")?)))
        .map(|(start, end)| {
            crate::statements::select(&text, Span::new(to_byte(start), to_byte(end)))
        });

    let config = config_for_uri(&uri, state, writer);
    let foreign_heads = config.as_ref().map_or(&[][..], |c| &c.foreign_heads[..]);
    let rewrites = crate::friendly::rewrite(
        &text,
        statements.as_deref(),
        foreign_heads,
        &format_options(config.as_ref()),
    );
    let lines = position::LineIndex::new(&text);
    rewrites
        .into_iter()
        .map(|r| {
            let (sl, sc) = lines.position(&text, r.span.start, encoding);
            let (el, ec) = lines.position(&text, r.span.end, encoding);
            Json::object(vec![
                (
                    "range".into(),
                    Json::object(vec![
                        ("start".into(), pos(sl, sc)),
                        ("end".into(), pos(el, ec)),
                    ]),
                ),
                ("newText".into(), Json::str(r.text)),
            ])
        })
        .collect()
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
    let resolved = resolve_selection(uri, state, config.as_ref(), writer);
    let selection = resolved.selection();
    let findings = grebe_rules::analysis::analyze(text, &selection);
    let lines = position::LineIndex::new(text);
    let diagnostics: Vec<Json> = findings
        .iter()
        .filter_map(|f| {
            let rule = grebe_rules::lookup(f.code)?;
            let (sl, sc) = lines.position(text, f.span.start, encoding);
            let (el, ec) = lines.position(text, f.span.end, encoding);
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
                // A CAT finding names the table or column it is about; every
                // other rule's message is the registry's fixed prose.
                (
                    "message".into(),
                    if rule.category == grebe_rules::Category::Cat {
                        Json::str(grebe_rules::catalog::message(f, text))
                    } else {
                        Json::str(rule.message)
                    },
                ),
            ]))
        })
        .collect();
    let version = state.versions.get(uri).copied();
    publish_diagnostics(writer, uri, &diagnostics, version);
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

/// `grebe/catalog`: the client's catalog snapshot (rows as in
/// [`grebe_rules::catalog`]).
///
/// Which catalog a notification is about:
///
/// - no `uri` and no `catalog`: the one for every document that has none
///   of its own;
/// - `uri`: that document's own;
/// - `catalog: NAME` (no `uri`): a catalog kept under that name, which
///   documents use when told to.
///
/// What it says about it:
///
/// - `{ columns: [row...], functions?: [...] }`: this is the whole of it.
///   An empty array is a catalog with nothing in it, and is taken at its
///   word. `{ columns: null }` takes it away; for a document that means it
///   has no catalog, and does not fall back to the shared one.
/// - `{ database: D, columns: [row...], functions?: [...] }`: this is the
///   part for database `D`, in place of the part `D` had; the other
///   databases' parts stay as they are. Rows may leave `"database"` out.
///   `tables: [{ schema, table, kind?, rows?, primary_key?, unique?,
///   indexes?, columns?, types?, nullable? }...]` may come beside
///   `columns` or instead of it: what is known of each table (see
///   [`Part::from_json`]), and, when it lists them, the table's columns in
///   a quarter of the bytes the rows take.
///   `{ database: D, columns: null }` (and no `tables`) drops that part.
///   A catalog whose last part is dropped is taken away.
/// - `{ uri, catalog: NAME }`: the document uses the catalog of that name
///   (none, while there is no such catalog).
///   `{ uri, default: true }`: it uses the shared one again.
///
/// Every open document a change reaches is linted again; a notification
/// that changes nothing (the same part again) reaches none.
fn handle_catalog<W: Write>(params: &Json, state: &mut State, writer: &mut W) {
    let text = |k: &str| params.get(k).and_then(Json::as_str).map(str::to_string);
    let (uri, name) = (text("uri"), text("catalog"));
    let complain = |writer: &mut W, e: String| {
        let p = Json::object(vec![
            ("type".into(), Json::num(2)),
            (
                "message".into(),
                Json::str(format!("grebe/catalog: not a catalog snapshot: {e}")),
            ),
        ]);
        notify(writer, "window/showMessage", p);
    };

    // Which catalog a document uses.
    if let Some(u) = &uri {
        let to_default = params.get("default") == Some(&Json::Bool(true));
        if name.is_some() || to_default {
            let was = (
                state.document_catalogs.remove(u).is_some(),
                state.document_catalog_names.remove(u),
            );
            if let Some(n) = &name {
                state.document_catalog_names.insert(u.clone(), n.clone());
            }
            let same = !was.0 && was.1 == name;
            if !same && state.documents.contains_key(u) {
                mark_dirty(state, u);
            }
            return;
        }
    }

    // The catalog as it is now, and as the notification leaves it.
    let before: Option<Rc<Catalog>> = match (&uri, &name) {
        (Some(u), _) => state.document_catalogs.get(u).cloned().flatten(),
        (None, Some(n)) => state.named_catalogs.get(n).cloned(),
        (None, None) => state.catalog.clone(),
    };
    let given = |k: &str| params.get(k).filter(|c| **c != Json::Null);
    let (columns, tables) = (given("columns"), given("tables"));
    let after: Option<Rc<Catalog>> = match (text("database"), columns.or(tables)) {
        (Some(database), Some(_)) => {
            let functions = params.get("functions");
            let part = match Part::from_json(&database, columns, tables, functions) {
                Ok(p) => p,
                Err(e) => return complain(writer, e),
            };
            let mut next = before.as_deref().cloned().unwrap_or_default();
            if next.set_part(part) {
                Some(Rc::new(next))
            } else {
                before.clone()
            }
        }
        (Some(database), None) => match &before {
            Some(have) if have.part(&database).is_some() => {
                let mut next = Catalog::clone(have);
                next.remove_part(&database);
                (!next.parts().is_empty()).then(|| Rc::new(next))
            }
            _ => before.clone(),
        },
        (None, Some(_)) => match Catalog::from_json(params) {
            Ok(c) if before.as_deref() == Some(&c) => before.clone(),
            Ok(c) => Some(Rc::new(c)),
            Err(e) => return complain(writer, e),
        },
        (None, None) => None,
    };

    let same = match (&before, &after) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    };
    let mut affected: Vec<String> = match (&uri, &name) {
        (Some(u), _) => {
            // "No catalog" is said of a document even when it had none:
            // from then on it does not fall back to the shared one.
            let fell_back = !state.document_catalogs.contains_key(u);
            state.document_catalogs.insert(u.clone(), after);
            if same && !fell_back {
                return;
            }
            state
                .documents
                .keys()
                .filter(|d| *d == u)
                .cloned()
                .collect()
        }
        (None, Some(n)) => {
            match after {
                Some(c) => state.named_catalogs.insert(n.clone(), c),
                None => state.named_catalogs.remove(n),
            };
            if same {
                return;
            }
            state
                .documents
                .keys()
                .filter(|d| state.document_catalog_names.get(*d) == Some(n))
                .cloned()
                .collect()
        }
        (None, None) => {
            state.catalog = after;
            if same {
                return;
            }
            // A document with a catalog of its own does not see the shared
            // one.
            state
                .documents
                .keys()
                .filter(|d| {
                    !state.document_catalogs.contains_key(*d)
                        && !state.document_catalog_names.contains_key(*d)
                })
                .cloned()
                .collect()
        }
    };
    affected.sort();
    for u in affected {
        mark_dirty(state, &u);
    }
}

/// What a request at a position is about: the document's text, the byte
/// the position is at, and the catalog the document is read against.
/// `None` for a document the server has not seen.
fn at_position<W: Write>(
    params: &Json,
    state: &mut State,
    writer: &mut W,
) -> Option<(String, u32, Option<Rc<Catalog>>)> {
    let uri = params
        .get("textDocument")
        .and_then(|d| d.get("uri"))
        .and_then(Json::as_str)
        .map(str::to_string)?;
    let text = state.documents.get(&uri).cloned()?;
    let num = |v: Option<&Json>| v.and_then(Json::as_f64).unwrap_or(0.0) as u32;
    let p = params.get("position");
    let offset = position::position_to_byte(
        &text,
        num(p.and_then(|p| p.get("line"))),
        num(p.and_then(|p| p.get("character"))),
        state.encoding,
    );
    // The config is only needed for a catalog file it may name, and only
    // when the client has not sent a catalog itself.
    let config = if state.document_catalogs.contains_key(&uri)
        || state.document_catalog_names.contains_key(&uri)
        || state.catalog.is_some()
    {
        None
    } else {
        config_for_uri(&uri, state, writer)
    };
    let catalog = catalog_for_uri(&uri, config.as_ref(), state, writer);
    Some((text, offset, catalog))
}

/// `textDocument/hover`: see [`crate::hover`]. `null` when there is
/// nothing to say: no catalog, or a name it cannot vouch for.
fn handle_hover<W: Write>(params: &Json, state: &mut State, writer: &mut W) -> Json {
    let Some((text, offset, Some(catalog))) = at_position(params, state, writer) else {
        return Json::Null;
    };
    let Some(found) = crate::hover::hover(&text, offset, &catalog) else {
        return Json::Null;
    };
    let encoding = state.encoding;
    let at = |byte: u32| {
        let (line, character) = position::byte_to_position(&text, byte, encoding);
        pos(line, character)
    };
    Json::object(vec![
        (
            "contents".into(),
            Json::object(vec![
                ("kind".into(), Json::str("markdown")),
                ("value".into(), Json::str(&found.contents)),
            ]),
        ),
        (
            "range".into(),
            Json::object(vec![
                ("start".into(), at(found.span.start)),
                ("end".into(), at(found.span.end)),
            ]),
        ),
    ])
}

/// `textDocument/completion`: see [`crate::completion`]. A document the
/// server has not seen answers with an empty list.
fn handle_completion<W: Write>(params: &Json, state: &mut State, writer: &mut W) -> Json {
    let empty = || {
        Json::object(vec![
            ("isIncomplete".into(), Json::Bool(false)),
            ("items".into(), Json::Array(vec![])),
        ])
    };
    let Some((text, offset, catalog)) = at_position(params, state, writer) else {
        return empty();
    };
    let found = crate::completion::complete(&text, offset, catalog.as_deref());
    Json::object(vec![
        ("isIncomplete".into(), Json::Bool(found.incomplete)),
        (
            "items".into(),
            Json::Array(
                found
                    .items
                    .iter()
                    .map(crate::completion::Item::to_json)
                    .collect(),
            ),
        ),
    ])
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
    let lines = position::LineIndex::new(text);
    Json::Array(
        spans
            .into_iter()
            .map(|s| {
                let (sl, sc) = lines.position(text, s.start, encoding);
                let (el, ec) = lines.position(text, s.end, encoding);
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

fn publish_diagnostics<W: Write>(
    writer: &mut W,
    uri: &str,
    diagnostics: &[Json],
    version: Option<f64>,
) {
    let mut fields = vec![
        ("uri".into(), Json::str(uri)),
        ("diagnostics".into(), Json::Array(diagnostics.to_vec())),
    ];
    if let Some(v) = version {
        fields.push(("version".into(), Json::num(v)));
    }
    let params = Json::object(fields);
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

    #[test]
    fn file_uris_become_paths_on_windows_and_elsewhere() {
        assert_eq!(
            file_uri_path("file:///home/a%20b/x.sql", false).as_deref(),
            Some("/home/a b/x.sql")
        );
        assert_eq!(
            file_uri_path("file:///c%3A/Users/x.sql", true).as_deref(),
            Some("c:/Users/x.sql")
        );
        assert_eq!(
            file_uri_path("file:///C:/x.sql", true).as_deref(),
            Some("C:/x.sql")
        );
        assert_eq!(
            file_uri_path("file://server/share/x.sql", true).as_deref(),
            Some("\\\\server/share/x.sql")
        );
        assert_eq!(file_uri_path("untitled:Untitled-1", true), None);
    }

    // -- batching, cancellation, versions, lifecycle -------------------------

    fn body(v: &str) -> Vec<u8> {
        v.as_bytes().to_vec()
    }

    fn init() -> Vec<u8> {
        body(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{}}}"#)
    }

    fn open(uri: &str, version: u32, text: &str) -> Vec<u8> {
        body(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{uri}","version":{version},"text":"{text}"}}}}}}"#
        ))
    }

    fn change(uri: &str, version: u32, texts: &[&str]) -> Vec<u8> {
        let changes: Vec<String> = texts
            .iter()
            .map(|t| format!(r#"{{"text":"{t}"}}"#))
            .collect();
        body(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{uri}","version":{version}}},"contentChanges":[{}]}}}}"#,
            changes.join(",")
        ))
    }

    /// Run `batches` in order on one session; every message written.
    fn session(batches: &[Vec<Vec<u8>>]) -> Vec<Json> {
        let mut state = State::default();
        let mut out: Vec<u8> = Vec::new();
        for b in batches {
            process_batch(b, &mut state, &mut out);
        }
        let mut cursor = std::io::Cursor::new(out);
        let mut msgs = Vec::new();
        while let Some(m) = transport::read_message(&mut cursor).unwrap() {
            msgs.push(json::parse(std::str::from_utf8(&m).unwrap()).unwrap());
        }
        msgs
    }

    fn publishes(msgs: &[Json]) -> Vec<&Json> {
        msgs.iter()
            .filter(|m| {
                m.get("method").and_then(Json::as_str) == Some("textDocument/publishDiagnostics")
            })
            .map(|m| m.get("params").unwrap())
            .collect()
    }

    fn error_code(msgs: &[Json], id: f64) -> Option<f64> {
        msgs.iter()
            .find(|m| m.get("id").and_then(Json::as_f64) == Some(id))?
            .get("error")?
            .get("code")?
            .as_f64()
    }

    #[test]
    fn changes_in_one_batch_are_linted_once_for_the_newest_text() {
        let u = "file:///a.sql";
        let mut batch = vec![init(), open(u, 1, "SELECT count(*) FROM t")];
        for v in 2..50 {
            batch.push(change(u, v, &["SELECT count(*) FROM t"]));
        }
        batch.push(change(u, 50, &["SELECT 1"]));
        let msgs = session(&[batch]);
        let p = publishes(&msgs);
        assert_eq!(p.len(), 1, "one lint for 50 changes");
        assert_eq!(p[0].get("version").and_then(Json::as_f64), Some(50.0));
        assert!(
            p[0].get("diagnostics")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_request_cancelled_in_its_batch_is_not_run() {
        let msgs = session(&[vec![
            init(),
            open("file:///a.sql", 1, "select 1"),
            body(
                r#"{"jsonrpc":"2.0","id":7,"method":"textDocument/formatting","params":{"textDocument":{"uri":"file:///a.sql"}}}"#,
            ),
            body(r#"{"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":7}}"#),
            body(
                r#"{"jsonrpc":"2.0","id":8,"method":"textDocument/formatting","params":{"textDocument":{"uri":"file:///a.sql"}}}"#,
            ),
        ]]);
        assert_eq!(error_code(&msgs, 7.0), Some(-32800.0));
        assert_eq!(
            error_code(&msgs, 8.0),
            None,
            "an uncancelled request still runs"
        );
    }

    #[test]
    fn a_stale_version_and_a_change_after_close_are_ignored() {
        let u = "file:///a.sql";
        let msgs = session(&[
            vec![init(), open(u, 5, "SELECT 1")],
            vec![change(u, 4, &["SELECT count(*) FROM t"])],
            vec![body(&format!(
                r#"{{"jsonrpc":"2.0","method":"textDocument/didClose","params":{{"textDocument":{{"uri":"{u}"}}}}}}"#
            ))],
            vec![change(u, 9, &["SELECT count(*) FROM t"])],
        ]);
        let p = publishes(&msgs);
        // didOpen's, then didClose's empty one; nothing for the stale change
        // or the change after close.
        assert_eq!(p.len(), 2);
        assert!(
            p.iter()
                .all(|p| p.get("diagnostics").unwrap().as_array().unwrap().is_empty())
        );
    }

    #[test]
    fn the_last_of_several_content_changes_is_the_document() {
        let u = "file:///a.sql";
        let msgs = session(&[vec![
            init(),
            open(u, 1, "SELECT 1"),
            change(u, 2, &["SELECT 1", "SELECT count(*) FROM t"]),
        ]]);
        let p = publishes(&msgs);
        let codes: Vec<_> = p[0]
            .get("diagnostics")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d.get("code").and_then(Json::as_str))
            .collect();
        assert_eq!(codes, ["MOD001"]);
    }

    #[test]
    fn the_lifecycle_is_enforced() {
        let req = |id: u32, method: &str| {
            body(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}"}}"#
            ))
        };
        let msgs = session(&[
            vec![
                req(1, "textDocument/formatting"),
                init(),
                req(2, "initialize"),
            ],
            vec![req(3, "shutdown"), req(4, "textDocument/formatting")],
            vec![body("{ not json")],
        ]);
        assert_eq!(error_code(&msgs, 1.0), Some(-32002.0), "before initialize");
        assert_eq!(error_code(&msgs, 2.0), Some(-32600.0), "initialize twice");
        assert_eq!(error_code(&msgs, 3.0), None, "shutdown itself succeeds");
        assert_eq!(error_code(&msgs, 4.0), Some(-32600.0), "after shutdown");
        assert!(msgs.iter().any(|m| {
            m.get("id") == Some(&Json::Null)
                && m.get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(Json::as_f64)
                    == Some(-32700.0)
        }));
    }

    #[test]
    fn a_config_error_is_shown_again_after_the_file_was_fixed() {
        let dir = std::env::temp_dir().join(format!("grebe-lsp-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("grebe.toml");
        let uri = format!("file://{}", dir.join("q.sql").display());
        let shown = |msgs: &[Json]| {
            msgs.iter()
                .filter(|m| m.get("method").and_then(Json::as_str) == Some("window/showMessage"))
                .count()
        };
        let mut state = State::default();
        let mut out: Vec<u8> = Vec::new();
        let mut step = |toml_text: &str, msg: Vec<u8>| {
            std::fs::write(&toml, toml_text).unwrap();
            process_batch(&[msg], &mut state, &mut out);
        };
        step("", init());
        step("[severity\n", open(&uri, 1, "select 1"));
        step("[severity\n", change(&uri, 2, &["select 2"])); // same error: not again
        step("", change(&uri, 3, &["select 3"])); // fixed
        step("[severity\n", change(&uri, 4, &["select 4"])); // broken again: shown
        let _ = std::fs::remove_dir_all(&dir);
        let mut cursor = std::io::Cursor::new(out);
        let mut msgs = Vec::new();
        while let Some(m) = transport::read_message(&mut cursor).unwrap() {
            msgs.push(json::parse(std::str::from_utf8(&m).unwrap()).unwrap());
        }
        assert_eq!(shown(&msgs), 2);
    }
}
