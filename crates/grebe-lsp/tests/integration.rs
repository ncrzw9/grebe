//! Drives the real lifecycle end to end: build raw
//! `Content-Length`-framed message bytes for `initialize` -> `didOpen`
//! (SQL that fires MOD001) -> assert `publishDiagnostics` with the
//! byte-exact range -> `didChange` to SQL that fires nothing -> assert
//! diagnostics clear -> `shutdown` -> `exit`.
//!
//! `server::run` is generic over `impl BufRead` + `impl Write` precisely so
//! this test can drive it over in-memory buffers instead of real stdio.

use grebe_lsp::json::{self, Json};
use grebe_lsp::position::{self, Encoding};
use grebe_lsp::{server, transport};
use std::io::Cursor;

fn request(id: i64, method: &str, params: Json) -> String {
    let msg = Json::object(vec![
        ("jsonrpc".into(), Json::str("2.0")),
        ("id".into(), Json::num(id as f64)),
        ("method".into(), Json::str(method)),
        ("params".into(), params),
    ]);
    json::to_string(&msg)
}

fn notification(method: &str, params: Json) -> String {
    let msg = Json::object(vec![
        ("jsonrpc".into(), Json::str("2.0")),
        ("method".into(), Json::str(method)),
        ("params".into(), params),
    ]);
    json::to_string(&msg)
}

fn text_document(uri: &str, extra: Vec<(String, Json)>) -> Json {
    let mut pairs = vec![("uri".to_string(), Json::str(uri))];
    pairs.extend(extra);
    Json::object(pairs)
}

/// Parse every framed message out of a response/notification byte stream.
fn read_all_messages(bytes: Vec<u8>) -> Vec<Json> {
    let mut cursor = Cursor::new(bytes);
    let mut out = Vec::new();
    while let Some(body) = transport::read_message(&mut cursor).expect("well-framed output") {
        let text = String::from_utf8(body).expect("UTF-8 output");
        out.push(json::parse(&text).expect("valid JSON output"));
    }
    out
}

#[test]
fn full_lifecycle_over_stdio_framing() {
    const URI: &str = "file:///test.sql";
    const SQL_HIT: &str = "SELECT count(*) FROM t";
    const SQL_CLEAN: &str = "SELECT count() FROM t";

    let mut input: Vec<u8> = Vec::new();

    transport::write_message(
        &mut input,
        &request(
            1,
            "initialize",
            Json::object(vec![("capabilities".into(), Json::object(vec![]))]),
        ),
    )
    .unwrap();
    transport::write_message(&mut input, &notification("initialized", Json::Null)).unwrap();
    transport::write_message(
        &mut input,
        &notification(
            "textDocument/didOpen",
            Json::object(vec![(
                "textDocument".into(),
                text_document(
                    URI,
                    vec![
                        ("languageId".to_string(), Json::str("sql")),
                        ("version".to_string(), Json::num(1)),
                        ("text".to_string(), Json::str(SQL_HIT)),
                    ],
                ),
            )]),
        ),
    )
    .unwrap();
    transport::write_message(
        &mut input,
        &notification(
            "textDocument/didChange",
            Json::object(vec![
                (
                    "textDocument".into(),
                    text_document(URI, vec![("version".to_string(), Json::num(2))]),
                ),
                (
                    "contentChanges".into(),
                    Json::Array(vec![Json::object(vec![(
                        "text".into(),
                        Json::str(SQL_CLEAN),
                    )])]),
                ),
            ]),
        ),
    )
    .unwrap();
    transport::write_message(&mut input, &request(2, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    let code = server::run(Cursor::new(input), &mut output);
    assert_eq!(code, 0, "clean shutdown+exit must return 0");

    let messages = read_all_messages(output);

    // 1. initialize response.
    let init_resp = &messages[0];
    assert_eq!(init_resp.get("id").unwrap().as_f64(), Some(1.0));
    let caps = init_resp
        .get("result")
        .unwrap()
        .get("capabilities")
        .unwrap();
    assert_eq!(caps.get("textDocumentSync").unwrap().as_f64(), Some(1.0));
    assert_eq!(
        caps.get("positionEncoding").unwrap().as_str(),
        Some("utf-16")
    );

    // 2. publishDiagnostics from didOpen: exactly one MOD001 finding, at
    // the byte-exact UTF-16 range of the `*`.
    let diag1 = messages
        .iter()
        .find(|m| m.get("method").and_then(Json::as_str) == Some("textDocument/publishDiagnostics"))
        .expect("a publishDiagnostics notification for didOpen");
    let params1 = diag1.get("params").unwrap();
    assert_eq!(params1.get("uri").unwrap().as_str(), Some(URI));
    let diags1 = params1.get("diagnostics").unwrap().as_array().unwrap();
    assert_eq!(diags1.len(), 1, "count(*) must fire exactly MOD001");
    let d = &diags1[0];
    assert_eq!(d.get("code").unwrap().as_str(), Some("MOD001"));
    assert_eq!(d.get("source").unwrap().as_str(), Some("grebe"));
    assert_eq!(d.get("severity").unwrap().as_f64(), Some(3.0)); // MOD001 is Info

    let star = SQL_HIT.find('*').unwrap() as u32;
    let (want_start_line, want_start_char) =
        position::byte_to_position(SQL_HIT, star, Encoding::Utf16);
    let (want_end_line, want_end_char) =
        position::byte_to_position(SQL_HIT, star + 1, Encoding::Utf16);
    let range = d.get("range").unwrap();
    let start = range.get("start").unwrap();
    let end = range.get("end").unwrap();
    assert_eq!(
        start.get("line").unwrap().as_f64(),
        Some(f64::from(want_start_line))
    );
    assert_eq!(
        start.get("character").unwrap().as_f64(),
        Some(f64::from(want_start_char))
    );
    assert_eq!(
        end.get("line").unwrap().as_f64(),
        Some(f64::from(want_end_line))
    );
    assert_eq!(
        end.get("character").unwrap().as_f64(),
        Some(f64::from(want_end_char))
    );

    // 3. publishDiagnostics from didChange: SQL that fires nothing clears
    // the previous diagnostics (an empty array, not a missing message).
    let diag2 = messages
        .iter()
        .filter(|m| {
            m.get("method").and_then(Json::as_str) == Some("textDocument/publishDiagnostics")
        })
        .nth(1)
        .expect("a second publishDiagnostics notification for didChange");
    let diags2 = diag2
        .get("params")
        .unwrap()
        .get("diagnostics")
        .unwrap()
        .as_array()
        .unwrap();
    assert!(diags2.is_empty(), "count() FROM t must fire nothing");

    // 4. shutdown response: id echoed, null result.
    let shutdown_resp = messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(2.0))
        .expect("a response to the shutdown request");
    assert_eq!(shutdown_resp.get("result").unwrap(), &Json::Null);
}

#[test]
fn unknown_method_request_gets_method_not_found() {
    let mut input: Vec<u8> = Vec::new();
    transport::write_message(&mut input, &request(1, "totally/unknown", Json::Null)).unwrap();
    transport::write_message(&mut input, &request(2, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    let code = server::run(Cursor::new(input), &mut output);
    assert_eq!(code, 0);

    let messages = read_all_messages(output);
    let err_resp = messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(1.0))
        .unwrap();
    let error = err_resp
        .get("error")
        .expect("an error object for an unknown method");
    assert_eq!(error.get("code").unwrap().as_f64(), Some(-32601.0));
}

#[test]
fn malformed_json_does_not_kill_the_server() {
    let mut input: Vec<u8> = Vec::new();
    // A syntactically broken body, well-framed at the transport level.
    transport::write_message(&mut input, "{ this is not json").unwrap();
    transport::write_message(&mut input, &request(1, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    let code = server::run(Cursor::new(input), &mut output);
    assert_eq!(code, 0, "the broken message must be skipped, not fatal");

    let messages = read_all_messages(output);
    assert!(
        messages
            .iter()
            .any(|m| m.get("id").and_then(Json::as_f64) == Some(1.0))
    );
}

#[test]
fn exit_without_shutdown_returns_nonzero() {
    let mut input: Vec<u8> = Vec::new();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    let code = server::run(Cursor::new(input), &mut output);
    assert_eq!(code, 1);
}

// -- textDocument/formatting, and grebe.toml wiring --------------------

/// A unique scratch directory under the OS temp dir, removed on drop --
/// mirrors `grebe_rules::config`'s own `TempTree` test helper, since this
/// crate cannot depend on that one's `#[cfg(test)]`-only code.
struct TempTree {
    root: std::path::PathBuf,
}

impl TempTree {
    fn new(tag: &str) -> Self {
        let unique = format!(
            "grebe-lsp-integration-{tag}-{}-{:?}",
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

    fn write(&self, rel: &str, contents: &str) -> std::path::PathBuf {
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

fn formatting_request(id: i64, uri: &str) -> String {
    request(
        id,
        "textDocument/formatting",
        Json::object(vec![
            ("textDocument".into(), text_document(uri, vec![])),
            // The server ignores these on purpose (`grebe.toml` is the knob
            // source); sending a divergent tabSize proves that.
            (
                "options".into(),
                Json::object(vec![
                    ("tabSize".into(), Json::num(8)),
                    ("insertSpaces".into(), Json::Bool(true)),
                ]),
            ),
        ]),
    )
}

fn did_open(uri: &str, text: &str) -> String {
    notification(
        "textDocument/didOpen",
        Json::object(vec![(
            "textDocument".into(),
            text_document(
                uri,
                vec![
                    ("languageId".to_string(), Json::str("sql")),
                    ("version".to_string(), Json::num(1)),
                    ("text".to_string(), Json::str(text)),
                ],
            ),
        )]),
    )
}

/// Drive `initialize` -> `didOpen(text)` -> `textDocument/formatting` ->
/// `shutdown`/`exit`, and return every message the server wrote.
fn run_formatting_session(uri: &str, text: &str) -> Vec<Json> {
    let mut input: Vec<u8> = Vec::new();
    transport::write_message(
        &mut input,
        &request(
            1,
            "initialize",
            Json::object(vec![("capabilities".into(), Json::object(vec![]))]),
        ),
    )
    .unwrap();
    transport::write_message(&mut input, &notification("initialized", Json::Null)).unwrap();
    transport::write_message(&mut input, &did_open(uri, text)).unwrap();
    transport::write_message(&mut input, &formatting_request(2, uri)).unwrap();
    transport::write_message(&mut input, &request(3, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    let code = server::run(Cursor::new(input), &mut output);
    assert_eq!(code, 0, "clean shutdown+exit must return 0");
    read_all_messages(output)
}

fn formatting_response(messages: &[Json]) -> &Json {
    messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(2.0))
        .expect("a response to the formatting request")
}

#[test]
fn initialize_advertises_document_formatting() {
    let mut input: Vec<u8> = Vec::new();
    transport::write_message(
        &mut input,
        &request(
            1,
            "initialize",
            Json::object(vec![("capabilities".into(), Json::object(vec![]))]),
        ),
    )
    .unwrap();
    transport::write_message(&mut input, &request(2, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    server::run(Cursor::new(input), &mut output);
    let messages = read_all_messages(output);
    let caps = messages[0]
        .get("result")
        .unwrap()
        .get("capabilities")
        .unwrap();
    assert_eq!(
        caps.get("documentFormattingProvider").unwrap(),
        &Json::Bool(true)
    );
}

#[test]
fn formatting_a_messy_document_returns_one_edit_with_the_formatted_text() {
    const URI: &str = "file:///messy.sql";
    const SRC: &str = "select   1;";

    let messages = run_formatting_session(URI, SRC);
    let resp = formatting_response(&messages);
    let edits = resp.get("result").unwrap().as_array().unwrap();
    assert_eq!(edits.len(), 1, "{edits:?}");

    let want = grebe_format::format(SRC, &grebe_format::Options::default());
    assert_ne!(want, SRC, "fixture must actually need formatting");
    assert_eq!(
        edits[0].get("newText").unwrap().as_str(),
        Some(want.as_str())
    );

    let range = edits[0].get("range").unwrap();
    let start = range.get("start").unwrap();
    assert_eq!(start.get("line").unwrap().as_f64(), Some(0.0));
    assert_eq!(start.get("character").unwrap().as_f64(), Some(0.0));
    let (want_line, want_char) = position::byte_to_position(SRC, SRC.len() as u32, Encoding::Utf16);
    let end = range.get("end").unwrap();
    assert_eq!(
        end.get("line").unwrap().as_f64(),
        Some(f64::from(want_line))
    );
    assert_eq!(
        end.get("character").unwrap().as_f64(),
        Some(f64::from(want_char))
    );
}

#[test]
fn formatting_an_already_formatted_document_returns_no_edits() {
    const URI: &str = "file:///clean.sql";
    let clean = grebe_format::format("select 1;", &grebe_format::Options::default());

    let messages = run_formatting_session(URI, &clean);
    let resp = formatting_response(&messages);
    let edits = resp.get("result").unwrap().as_array().unwrap();
    assert!(edits.is_empty(), "{edits:?}");
}

#[test]
fn grebe_toml_indent_size_changes_the_formatted_output() {
    let tree = TempTree::new("format-indent");
    tree.write("grebe.toml", "[format]\nindent_size = 2\n");
    let sql_path = tree.write("q.sql", "");
    let uri = format!("file://{}", sql_path.display());

    // A column list long enough to exceed the default 100-column inline
    // threshold, so the formatter breaks it onto indented lines -- short
    // enough to fit on one line either way tells us nothing about the knob.
    const SRC: &str = "SELECT column_one, column_two, column_three, column_four, column_five, column_six, column_seven, column_eight FROM some_table_name;";

    let messages = run_formatting_session(&uri, SRC);
    let resp = formatting_response(&messages);
    let edits = resp.get("result").unwrap().as_array().unwrap();
    assert_eq!(edits.len(), 1, "{edits:?}");
    let got = edits[0].get("newText").unwrap().as_str().unwrap();

    let two_space = grebe_format::Options {
        indent_size: 2,
        ..grebe_format::Options::default()
    };
    let want_two_space = grebe_format::format(SRC, &two_space);
    let want_default = grebe_format::format(SRC, &grebe_format::Options::default());
    assert_ne!(
        want_two_space, want_default,
        "fixture must actually differ between 2- and 4-space indent"
    );
    assert_eq!(got, want_two_space);
}

#[test]
fn grebe_toml_severity_override_changes_the_published_diagnostic_severity() {
    let tree = TempTree::new("severity-override");
    tree.write("grebe.toml", "[severity]\nMOD001 = \"error\"\n");
    let sql_path = tree.write("q.sql", "");
    let uri = format!("file://{}", sql_path.display());

    let mut input: Vec<u8> = Vec::new();
    transport::write_message(
        &mut input,
        &request(
            1,
            "initialize",
            Json::object(vec![("capabilities".into(), Json::object(vec![]))]),
        ),
    )
    .unwrap();
    transport::write_message(&mut input, &notification("initialized", Json::Null)).unwrap();
    transport::write_message(&mut input, &did_open(&uri, "SELECT count(*) FROM t;")).unwrap();
    transport::write_message(&mut input, &request(2, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    let code = server::run(Cursor::new(input), &mut output);
    assert_eq!(code, 0);

    let messages = read_all_messages(output);
    let diag = messages
        .iter()
        .find(|m| m.get("method").and_then(Json::as_str) == Some("textDocument/publishDiagnostics"))
        .expect("a publishDiagnostics notification");
    let diags = diag
        .get("params")
        .unwrap()
        .get("diagnostics")
        .unwrap()
        .as_array()
        .unwrap();
    let mod001 = diags
        .iter()
        .find(|d| d.get("code").unwrap().as_str() == Some("MOD001"))
        .expect("MOD001 must still fire");
    assert_eq!(
        mod001.get("severity").unwrap().as_f64(),
        Some(1.0),
        "grebe.toml's [severity] override must win over MOD001's registry default"
    );
}
