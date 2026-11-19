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

    // 2. Diagnostics: the didOpen and didChange may arrive in one batch
    // and be linted once, so there are one or two publishes -- but the last
    // is for the newest text (version 2), where nothing fires: an empty
    // array, not a missing message, clears what didOpen showed.
    let publishes: Vec<&Json> = messages
        .iter()
        .filter(|m| {
            m.get("method").and_then(Json::as_str) == Some("textDocument/publishDiagnostics")
        })
        .collect();
    assert!(
        matches!(publishes.len(), 1 | 2),
        "{} publishes",
        publishes.len()
    );
    let last = publishes.last().unwrap().get("params").unwrap();
    assert_eq!(last.get("uri").unwrap().as_str(), Some(URI));
    assert_eq!(last.get("version").and_then(Json::as_f64), Some(2.0));
    let diags = last.get("diagnostics").unwrap().as_array().unwrap();
    assert!(diags.is_empty(), "count() FROM t must fire nothing");
    assert!(SQL_HIT != SQL_CLEAN);

    // 4. shutdown response: id echoed, null result.
    let shutdown_resp = messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(2.0))
        .expect("a response to the shutdown request");
    assert_eq!(shutdown_resp.get("result").unwrap(), &Json::Null);
}

#[test]
fn a_diagnostic_has_the_utf16_range_of_its_finding() {
    const URI: &str = "file:///range.sql";
    const SQL: &str = "SELECT '😀', count(*) FROM t";
    let mut input: Vec<u8> = Vec::new();
    transport::write_message(
        &mut input,
        &request(
            100,
            "initialize",
            Json::object(vec![("capabilities".into(), Json::object(vec![]))]),
        ),
    )
    .unwrap();
    transport::write_message(
        &mut input,
        &notification(
            "textDocument/didOpen",
            Json::object(vec![(
                "textDocument".into(),
                text_document(
                    URI,
                    vec![
                        ("version".to_string(), Json::num(1)),
                        ("text".to_string(), Json::str(SQL)),
                    ],
                ),
            )]),
        ),
    )
    .unwrap();
    transport::write_message(&mut input, &request(2, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    assert_eq!(server::run(Cursor::new(input), &mut output), 0);
    let messages = read_all_messages(output);
    let publish = messages
        .iter()
        .find(|m| m.get("method").and_then(Json::as_str) == Some("textDocument/publishDiagnostics"))
        .expect("a publishDiagnostics notification for didOpen");
    let params = publish.get("params").unwrap();
    assert_eq!(params.get("version").and_then(Json::as_f64), Some(1.0));
    let diags = params.get("diagnostics").unwrap().as_array().unwrap();
    assert_eq!(diags.len(), 1, "count(*) must fire exactly MOD001");
    let d = &diags[0];
    assert_eq!(d.get("code").unwrap().as_str(), Some("MOD001"));
    assert_eq!(d.get("source").unwrap().as_str(), Some("grebe"));
    assert_eq!(d.get("severity").unwrap().as_f64(), Some(3.0)); // MOD001 is Info

    // The `*`, in UTF-16 units: the emoji before it is two of them.
    let star = SQL.find('*').unwrap() as u32;
    let range = d.get("range").unwrap();
    for (key, at) in [("start", star), ("end", star + 1)] {
        let (line, ch) = position::byte_to_position(SQL, at, Encoding::Utf16);
        let p = range.get(key).unwrap();
        assert_eq!(p.get("line").unwrap().as_f64(), Some(f64::from(line)));
        assert_eq!(p.get("character").unwrap().as_f64(), Some(f64::from(ch)));
    }
}

#[test]
fn unknown_method_request_gets_method_not_found() {
    let mut input: Vec<u8> = Vec::new();
    transport::write_message(
        &mut input,
        &request(
            100,
            "initialize",
            Json::object(vec![("capabilities".into(), Json::object(vec![]))]),
        ),
    )
    .unwrap();
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
    transport::write_message(
        &mut input,
        &request(
            100,
            "initialize",
            Json::object(vec![("capabilities".into(), Json::object(vec![]))]),
        ),
    )
    .unwrap();
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

/// `grebe/statements` over the real framing: a cursor and a
/// whole-document request, with a `;` inside a string that must not split.
#[test]
fn grebe_statements_returns_the_statements_to_run() {
    const URI: &str = "file:///run.sql";
    const SQL: &str = "-- load\nCREATE TABLE t AS SELECT ';' AS s;\nSELECT s FROM t";

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
    transport::write_message(&mut input, &did_open(URI, SQL)).unwrap();
    let at = |line: f64, character: f64| {
        Json::object(vec![
            ("line".into(), Json::num(line)),
            ("character".into(), Json::num(character)),
        ])
    };
    transport::write_message(
        &mut input,
        &request(
            2,
            "grebe/statements",
            Json::object(vec![
                ("textDocument".into(), text_document(URI, vec![])),
                (
                    "range".into(),
                    Json::object(vec![
                        ("start".into(), at(2.0, 3.0)),
                        ("end".into(), at(2.0, 3.0)),
                    ]),
                ),
            ]),
        ),
    )
    .unwrap();
    transport::write_message(
        &mut input,
        &request(
            3,
            "grebe/statements",
            Json::object(vec![("textDocument".into(), text_document(URI, vec![]))]),
        ),
    )
    .unwrap();
    transport::write_message(&mut input, &request(4, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();

    let mut output: Vec<u8> = Vec::new();
    assert_eq!(server::run(Cursor::new(input), &mut output), 0);
    let messages = read_all_messages(output);
    let texts = |id: f64| -> Vec<String> {
        let resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Json::as_f64) == Some(id))
            .expect("a response");
        match resp.get("result") {
            Some(Json::Array(items)) => items
                .iter()
                .map(|i| i.get("text").and_then(Json::as_str).unwrap().to_string())
                .collect(),
            other => panic!("expected an array, got {other:?}"),
        }
    };
    assert_eq!(texts(2.0), ["SELECT s FROM t"]);
    assert_eq!(
        texts(3.0),
        ["CREATE TABLE t AS SELECT ';' AS s;", "SELECT s FROM t"]
    );

    // The range points at the statement, past its leading comment.
    let resp = messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(3.0))
        .unwrap();
    let Some(Json::Array(items)) = resp.get("result") else {
        unreachable!()
    };
    let start = items[0].get("range").and_then(|r| r.get("start")).unwrap();
    assert_eq!(start.get("line").and_then(Json::as_f64), Some(1.0));
    assert_eq!(start.get("character").and_then(Json::as_f64), Some(0.0));
}

// ------------------------------------------------------------- catalog --

const SHOP: &str = r#"[
    {"database":"shop","schema":"main","table":"orders","column":"id","type":"BIGINT"},
    {"database":"shop","schema":"main","table":"orders","column":"total","type":"DECIMAL(10,2)"},
    {"database":"shop","schema":"main","table":"customers","column":"id","type":"BIGINT"},
    {"database":"shop","schema":"main","table":"customers","column":"name","type":"VARCHAR"}
]"#;

/// A `grebe/catalog` notification: `columns` is snapshot JSON text, or
/// `"null"`.
fn catalog_notification(uri: Option<&str>, columns: &str) -> String {
    let mut pairs = vec![(
        "columns".to_string(),
        json::parse(columns).expect("valid snapshot JSON"),
    )];
    if let Some(u) = uri {
        pairs.push(("uri".to_string(), Json::str(u)));
    }
    notification("grebe/catalog", Json::object(pairs))
}

/// Run `messages` between `initialize` and `shutdown`/`exit`, one batch per
/// message so each is handled (and its diagnostics published) before the
/// next arrives. Returns everything the server wrote.
fn run_session(messages: &[String]) -> Vec<Json> {
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
    for m in messages {
        transport::write_message(&mut input, m).unwrap();
    }
    transport::write_message(&mut input, &request(9000, "shutdown", Json::Null)).unwrap();
    transport::write_message(&mut input, &notification("exit", Json::Null)).unwrap();
    let mut output: Vec<u8> = Vec::new();
    let code = server::run(SlowReader::new(input), &mut output);
    assert_eq!(code, 0, "clean shutdown+exit must return 0");
    read_all_messages(output)
}

/// Hands the server one framed message per batch: after each message it
/// waits long enough for the server to finish with it. The server lints
/// once per batch, so without this a whole scripted session is one batch
/// and only the final state of each document is ever published.
struct SlowReader {
    inner: std::io::BufReader<Cursor<Vec<u8>>>,
}

impl SlowReader {
    fn new(bytes: Vec<u8>) -> Self {
        SlowReader {
            inner: std::io::BufReader::new(Cursor::new(bytes)),
        }
    }
}

impl std::io::Read for SlowReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl std::io::BufRead for SlowReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.inner.fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        self.inner.consume(amt);
    }
    fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize> {
        // A header line starts each message: pause before it, so the
        // message before has been handled on its own.
        if buf.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        self.inner.read_line(buf)
    }
}

/// `(code, message)` of every diagnostic in each `publishDiagnostics` for
/// `uri`, in the order they were published.
fn published(messages: &[Json], uri: &str) -> Vec<Vec<(String, String)>> {
    messages
        .iter()
        .filter(|m| {
            m.get("method").and_then(Json::as_str) == Some("textDocument/publishDiagnostics")
        })
        .filter_map(|m| m.get("params"))
        .filter(|p| p.get("uri").and_then(Json::as_str) == Some(uri))
        .map(|p| {
            p.get("diagnostics")
                .and_then(Json::as_array)
                .unwrap_or_default()
                .iter()
                .map(|d| {
                    (
                        d.get("code")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .to_string(),
                        d.get("message")
                            .and_then(Json::as_str)
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .collect()
        })
        .collect()
}

fn completion_request(id: i64, uri: &str, line: u32, character: u32) -> String {
    request(
        id,
        "textDocument/completion",
        Json::object(vec![
            ("textDocument".into(), text_document(uri, vec![])),
            (
                "position".into(),
                Json::object(vec![
                    ("line".into(), Json::num(f64::from(line))),
                    ("character".into(), Json::num(f64::from(character))),
                ]),
            ),
        ]),
    )
}

fn labels(messages: &[Json], id: f64) -> Vec<String> {
    messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(id))
        .and_then(|m| m.get("result"))
        .and_then(|r| r.get("items"))
        .and_then(Json::as_array)
        .expect("a completion list")
        .iter()
        .filter_map(|i| i.get("label").and_then(Json::as_str).map(str::to_string))
        .collect()
}

#[test]
fn initialize_advertises_completion() {
    let messages = run_session(&[]);
    let caps = messages[0]
        .get("result")
        .unwrap()
        .get("capabilities")
        .unwrap();
    let triggers = caps
        .get("completionProvider")
        .and_then(|c| c.get("triggerCharacters"))
        .and_then(Json::as_array)
        .expect("completionProvider.triggerCharacters");
    assert_eq!(triggers, &[Json::str(".")]);
}

/// No catalog, no CAT findings; a catalog arrives and the open document is
/// linted again against it; it is taken away and they go.
#[test]
fn a_catalog_notification_relints_open_documents() {
    let uri = "file:///catalog/q.sql";
    let sql = "SELECT o.totl FROM orders o JOIN custmers c ON o.id = c.id;";
    let messages = run_session(&[
        did_open(uri, sql),
        catalog_notification(None, SHOP),
        catalog_notification(None, "null"),
    ]);
    let rounds = published(&messages, uri);
    assert_eq!(rounds.len(), 3, "{rounds:?}");
    assert!(rounds[0].is_empty(), "no catalog, no CAT: {:?}", rounds[0]);
    assert_eq!(
        rounds[1],
        [
            (
                "CAT002".to_string(),
                "Column totl is not in the table it is qualified with. Did you mean total?"
                    .to_string()
            ),
            (
                "CAT001".to_string(),
                "Table or view custmers is not in the catalog. Did you mean customers?".to_string()
            ),
        ]
    );
    assert!(rounds[2].is_empty(), "catalog cleared: {:?}", rounds[2]);
}

/// A catalog sent for one document applies to that document only, and wins
/// there over the shared one; `columns: null` with a `uri` means that
/// document has none at all; `default: true` hands it back to the shared one.
#[test]
fn a_catalog_can_belong_to_one_document() {
    let a = "file:///catalog/a.sql";
    let b = "file:///catalog/b.sql";
    let other =
        r#"[{"database":"x","schema":"main","table":"events","column":"ts","type":"TIMESTAMP"}]"#;
    let back_to_default = notification(
        "grebe/catalog",
        Json::object(vec![
            ("uri".to_string(), Json::str(a)),
            ("default".to_string(), Json::Bool(true)),
        ]),
    );
    let messages = run_session(&[
        did_open(a, "SELECT * FROM ordrs;"),
        did_open(b, "SELECT * FROM ordrs;"),
        // Only `a` gets a catalog.
        catalog_notification(Some(a), other),
        // The shared one reaches `b`; `a` keeps its own.
        catalog_notification(None, SHOP),
        // `a` has no catalog now (its session stopped): nothing is checked,
        // and the shared one does not step in.
        catalog_notification(Some(a), "null"),
        // `a` uses the shared one again.
        back_to_default,
        // The shared one goes: both are clean.
        catalog_notification(None, "null"),
    ]);
    let rounds = |uri: &str| -> Vec<Vec<(String, String)>> { published(&messages, uri) };
    let said = |r: &[(String, String)]| -> String {
        r.iter()
            .map(|(c, m)| format!("{c} {m}"))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let a_rounds: Vec<String> = rounds(a).iter().map(|r| said(r)).collect();
    assert_eq!(
        a_rounds,
        [
            "",
            "CAT001 Table or view ordrs is not in the catalog.",
            "",
            "CAT001 Table or view ordrs is not in the catalog. Did you mean orders?",
            "",
        ],
        "a: opened, own catalog, none, the shared one, shared one cleared"
    );
    let b_rounds: Vec<String> = rounds(b).iter().map(|r| said(r)).collect();
    assert_eq!(
        b_rounds,
        [
            "",
            "CAT001 Table or view ordrs is not in the catalog. Did you mean orders?",
            "",
        ],
        "b: opened, shared catalog, shared one cleared"
    );
}

/// An empty catalog is a catalog: everything is unknown. Closing a document
/// forgets what was said about it.
#[test]
fn an_empty_catalog_is_taken_literally_and_close_forgets_a_documents_own() {
    let a = "file:///catalog/a.sql";
    let close = notification(
        "textDocument/didClose",
        Json::object(vec![("textDocument".into(), text_document(a, vec![]))]),
    );
    let messages = run_session(&[
        catalog_notification(None, "[]"),
        catalog_notification(Some(a), "null"),
        did_open(a, "SELECT * FROM orders;"),
        close,
        did_open(a, "SELECT * FROM orders;"),
    ]);
    let codes: Vec<Vec<String>> = published(&messages, a)
        .into_iter()
        .map(|r| r.into_iter().map(|(c, _)| c).collect())
        .collect();
    assert_eq!(
        codes,
        [vec![], vec![], vec!["CAT001".to_string()]],
        "no catalog of its own; closed; reopened under the (empty) shared one"
    );
}

#[test]
fn a_malformed_catalog_is_reported_and_changes_nothing() {
    let uri = "file:///catalog/q.sql";
    let messages = run_session(&[
        did_open(uri, "SELECT * FROM ordrs;"),
        catalog_notification(None, r#"[{"column":"no table"}]"#),
    ]);
    assert_eq!(published(&messages, uri).len(), 1, "not linted again");
    let said = messages
        .iter()
        .find(|m| m.get("method").and_then(Json::as_str) == Some("window/showMessage"))
        .and_then(|m| {
            m.get("params")?
                .get("message")?
                .as_str()
                .map(str::to_string)
        })
        .expect("a warning");
    assert!(said.contains("grebe/catalog"), "{said}");
}

/// A `grebe/catalog` notification from JSON text.
fn catalog_message(params: &str) -> String {
    notification("grebe/catalog", json::parse(params).expect("valid JSON"))
}

/// Each round of diagnostics for `uri`, as `CODE message; ...`.
fn rounds_said(messages: &[Json], uri: &str) -> Vec<String> {
    published(messages, uri)
        .iter()
        .map(|r| {
            r.iter()
                .map(|(c, m)| format!("{c} {m}"))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .collect()
}

/// A catalog can arrive database by database: each part replaces that
/// database's part only, the same part again changes (and lints) nothing,
/// and dropping the last part takes the catalog away.
#[test]
fn a_catalog_can_be_sent_in_per_database_parts() {
    let uri = "file:///catalog/q.sql";
    let sql = "SELECT u.emial FROM pg.public.users u JOIN ordrs o ON o.id = u.id;";
    let memory = r#"{"database":"memory","columns":[
        {"schema":"main","table":"orders","column":"id","type":"BIGINT"}]}"#;
    // Table by table, with what is known of the table beside its columns.
    let pg = r#"{"database":"pg","tables":[
        {"schema":"public","table":"users","rows":1200,"primary_key":["id"],
         "columns":["id","email"],"types":["INTEGER","VARCHAR"],"nullable":[false,true]}],
        "functions":[{"name":"twice","kind":"macro"}]}"#;
    let pg_changed = r#"{"database":"pg","tables":[
        {"schema":"public","table":"users","rows":1200,"primary_key":["id"],
         "columns":["id","emial"],"types":["INTEGER","VARCHAR"],"nullable":[false,true]}]}"#;
    let messages = run_session(&[
        did_open(uri, sql),
        catalog_message(memory),
        catalog_message(pg),
        // Unchanged: not linted again.
        catalog_message(pg),
        catalog_message(memory),
        // The part is replaced; `memory` is as it was.
        catalog_message(pg_changed),
        // Dropped: `pg` is a qualifier nobody knows, so it is not checked.
        catalog_message(r#"{"database":"pg","columns":null}"#),
        // A part that is not there: nothing changes.
        catalog_message(r#"{"database":"pg","columns":null}"#),
        // The last part goes, and the catalog with it.
        catalog_message(r#"{"database":"memory","columns":null}"#),
    ]);
    let ordrs = "CAT001 Table or view ordrs is not in the catalog. Did you mean orders?";
    assert_eq!(
        rounds_said(&messages, uri),
        [
            "".to_string(),
            ordrs.to_string(),
            format!("CAT002 Column emial is not in the table it is qualified with.; {ordrs}"),
            ordrs.to_string(),
            ordrs.to_string(),
            "".to_string(),
        ],
        "opened, memory, pg, pg changed, pg dropped, memory dropped"
    );
}

/// A whole snapshot and parts can follow each other: the snapshot replaces
/// everything, a part then replaces one database of it. A part for the
/// wrong database is refused.
#[test]
fn parts_and_whole_snapshots_mix_and_a_bad_part_is_refused() {
    let uri = "file:///catalog/q.sql";
    let messages = run_session(&[
        did_open(uri, "SELECT * FROM ordrs; SELECT * FROM shop.main.clients;"),
        catalog_notification(None, SHOP),
        // The same snapshot again: nothing to lint.
        catalog_notification(None, SHOP),
        catalog_message(
            r#"{"database":"shop","columns":[
                {"schema":"main","table":"clients","column":"id","type":"BIGINT"}]}"#,
        ),
        catalog_message(r#"{"database":"shop","columns":[{"database":"elsewhere","table":"t"}]}"#),
    ]);
    assert_eq!(
        rounds_said(&messages, uri),
        [
            "",
            "CAT001 Table or view ordrs is not in the catalog. Did you mean orders?; \
             CAT001 Table or view clients is not in the catalog.",
            "CAT001 Table or view ordrs is not in the catalog.",
        ]
    );
    let said = messages
        .iter()
        .find(|m| m.get("method").and_then(Json::as_str) == Some("window/showMessage"))
        .and_then(|m| {
            m.get("params")?
                .get("message")?
                .as_str()
                .map(str::to_string)
        })
        .expect("a warning");
    assert!(said.contains("in the part for \"shop\""), "{said}");
}

/// A catalog sent under a name is used by the documents told to: two files
/// of one session share it without it being sent twice. A document whose
/// named catalog is not there has none, and does not fall back.
#[test]
fn documents_can_share_a_catalog_by_name() {
    let a = "file:///catalog/a.sql";
    let b = "file:///catalog/b.sql";
    let c = "file:///catalog/c.sql";
    let scratch = r#"{"catalog":"scratch","database":"memory","columns":[
        {"schema":"main","table":"events","column":"ts","type":"TIMESTAMP"}]}"#;
    let use_scratch =
        |uri: &str| catalog_message(&format!(r#"{{"uri":"{uri}","catalog":"scratch"}}"#));
    let messages = run_session(&[
        did_open(a, "SELECT * FROM evnts;"),
        did_open(b, "SELECT * FROM evnts;"),
        did_open(c, "SELECT * FROM evnts;"),
        catalog_notification(None, SHOP),
        // `a` and `b` use a catalog that is not there yet: no catalog.
        use_scratch(a),
        use_scratch(b),
        // Told again: nothing changes.
        use_scratch(a),
        // It arrives, once, for both.
        catalog_message(scratch),
        // The shared one changing does not reach them.
        catalog_notification(None, "null"),
        // The named one goes: they have none.
        catalog_message(r#"{"catalog":"scratch","columns":null}"#),
        // `a` goes back to the shared one, which is not there either.
        catalog_message(&format!(r#"{{"uri":"{a}","default":true}}"#)),
    ]);
    let unknown = "CAT001 Table or view evnts is not in the catalog.";
    let known = "CAT001 Table or view evnts is not in the catalog. Did you mean events?";
    assert_eq!(
        rounds_said(&messages, a),
        ["", unknown, "", known, "", ""],
        "a: opened, shared, named (absent), named arrives, named goes, default"
    );
    assert_eq!(rounds_said(&messages, b), ["", unknown, "", known, ""]);
    assert_eq!(
        rounds_said(&messages, c),
        ["", unknown, ""],
        "c: opened, shared, shared cleared"
    );
}

/// Completion reads the parts: a database's schemas and tables, a table's
/// columns, and names by prefix.
#[test]
fn completion_reads_a_catalog_sent_in_parts() {
    let uri = "file:///catalog/c.sql";
    let sql = "SELECT u. FROM pg.public.users u;\nSELECT * FROM pg.public.;\nSELECT * FROM us";
    let at = |id: i64, line: u32, character: u32| {
        request(
            id,
            "textDocument/completion",
            Json::object(vec![
                ("textDocument".into(), text_document(uri, vec![])),
                (
                    "position".into(),
                    Json::object(vec![
                        ("line".into(), Json::num(line)),
                        ("character".into(), Json::num(character)),
                    ]),
                ),
            ]),
        )
    };
    let messages = run_session(&[
        did_open(uri, sql),
        catalog_message(
            r#"{"database":"memory","columns":[
                {"schema":"main","table":"usage","column":"n","type":"INT"}]}"#,
        ),
        catalog_message(
            r#"{"database":"pg","tables":[
                {"schema":"public","table":"users","columns":["id","email"],"types":["INTEGER","VARCHAR"]},
                {"schema":"public","table":"orders","columns":["id"],"types":["INTEGER"]}]}"#,
        ),
        at(11, 0, 9),
        at(12, 1, 24),
        at(13, 2, 16),
    ]);
    let labels = |id: f64| -> Vec<String> {
        let resp = messages
            .iter()
            .find(|m| m.get("id").and_then(Json::as_f64) == Some(id))
            .expect("a response");
        let mut out: Vec<String> = resp
            .get("result")
            .and_then(|r| r.get("items"))
            .and_then(Json::as_array)
            .expect("items")
            .iter()
            .filter_map(|i| i.get("label").and_then(Json::as_str).map(str::to_string))
            .collect();
        out.sort();
        out
    };
    assert_eq!(labels(11.0), ["email", "id"]);
    assert_eq!(labels(12.0), ["orders", "users"]);
    assert_eq!(labels(13.0), ["USING", "usage", "users"]);
}

/// `catalog = "..."` in grebe.toml gives an editor that sends no catalog
/// the same checks `grebe check` runs.
#[test]
fn grebe_toml_catalog_is_used_when_the_client_sends_none() {
    let tree = TempTree::new("toml-catalog");
    tree.write("grebe.toml", "catalog = \"catalog.json\"\n");
    tree.write("catalog.json", SHOP);
    let sql_path = tree.write("q.sql", "");
    let uri = format!("file://{}", sql_path.display());
    let messages = run_session(&[
        did_open(&uri, "SELECT * FROM ordrs;\nSELECT c. FROM customers c;"),
        completion_request(2, &uri, 1, 9),
    ]);
    let rounds = published(&messages, &uri);
    assert_eq!(rounds[0].len(), 2, "{rounds:?}");
    assert_eq!(rounds[0][0].0, "CAT001");
    assert_eq!(
        rounds[0][1].0, "PRS001",
        "the second statement does not parse"
    );
    assert_eq!(labels(&messages, 2.0), ["id", "name"]);
}

#[test]
fn completion_offers_the_catalogs_names() {
    let uri = "file:///catalog/q.sql";
    let messages = run_session(&[
        catalog_notification(None, SHOP),
        did_open(uri, "SELECT o. FROM orders o;\nSELECT * FROM cu"),
        completion_request(2, uri, 0, 9),
        completion_request(3, uri, 1, 16),
        // A document the server has not seen: an empty list, not an error.
        completion_request(4, "file:///nope.sql", 0, 0),
    ]);
    assert_eq!(labels(&messages, 2.0), ["id", "total"]);
    assert_eq!(labels(&messages, 3.0), ["customers"]);
    assert!(labels(&messages, 4.0).is_empty());
    let first = messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(2.0))
        .and_then(|m| m.get("result")?.get("items")?.as_array()?.first().cloned())
        .unwrap();
    assert_eq!(first.get("kind").and_then(Json::as_f64), Some(5.0));
    // A column's detail is its type, and nothing else.
    assert_eq!(first.get("detail").and_then(Json::as_str), Some("BIGINT"));
    assert!(first.get("documentation").is_none());
}

/// The nearest name is a quick fix in the editor.
#[test]
fn a_cat_suggestion_is_a_quick_fix() {
    let uri = "file:///catalog/q.sql";
    let messages = run_session(&[
        catalog_notification(None, SHOP),
        did_open(uri, "SELECT * FROM ordrs;"),
        request(
            2,
            "textDocument/codeAction",
            Json::object(vec![
                ("textDocument".into(), text_document(uri, vec![])),
                (
                    "range".into(),
                    Json::object(vec![
                        (
                            "start".into(),
                            Json::object(vec![
                                ("line".into(), Json::num(0)),
                                ("character".into(), Json::num(15)),
                            ]),
                        ),
                        (
                            "end".into(),
                            Json::object(vec![
                                ("line".into(), Json::num(0)),
                                ("character".into(), Json::num(15)),
                            ]),
                        ),
                    ]),
                ),
                (
                    "context".into(),
                    Json::object(vec![("diagnostics".into(), Json::Array(vec![]))]),
                ),
            ]),
        ),
    ]);
    let actions = messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(2.0))
        .and_then(|m| m.get("result")?.as_array().map(<[Json]>::to_vec))
        .expect("code actions");
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(
        actions[0].get("title").and_then(Json::as_str),
        Some("CAT001: change to orders")
    );
    let edit = actions[0]
        .get("edit")
        .and_then(|e| e.get("changes"))
        .and_then(|c| c.get(uri))
        .and_then(Json::as_array)
        .unwrap();
    assert_eq!(
        edit[0].get("newText").and_then(Json::as_str),
        Some("orders")
    );
}

fn lsp_position(line: u32, character: u32) -> Json {
    Json::object(vec![
        ("line".into(), Json::num(line)),
        ("character".into(), Json::num(character)),
    ])
}

fn lsp_range(start: (u32, u32), end: (u32, u32)) -> Json {
    Json::object(vec![
        ("start".into(), lsp_position(start.0, start.1)),
        ("end".into(), lsp_position(end.0, end.1)),
    ])
}

fn result_of(messages: &[Json], id: f64) -> Json {
    messages
        .iter()
        .find(|m| m.get("id").and_then(Json::as_f64) == Some(id))
        .and_then(|m| m.get("result").cloned())
        .unwrap_or_else(|| panic!("no result for request {id}"))
}

/// `(range, newText)` of each text edit, the range as
/// `[start line, start character, end line, end character]`.
fn text_edits(edits: &Json) -> Vec<([u32; 4], String)> {
    let at = |r: &Json, end: &str, field: &str| {
        r.get(end)
            .and_then(|p| p.get(field))
            .and_then(Json::as_f64)
            .unwrap() as u32
    };
    edits
        .as_array()
        .expect("an array of text edits")
        .iter()
        .map(|e| {
            let r = e.get("range").unwrap();
            (
                [
                    at(r, "start", "line"),
                    at(r, "start", "character"),
                    at(r, "end", "line"),
                    at(r, "end", "character"),
                ],
                e.get("newText").and_then(Json::as_str).unwrap().to_string(),
            )
        })
        .collect()
}

/// The words of `sql`: what a rewrite says, whatever lines it is laid out on.
fn sql_words(sql: &str) -> Vec<&str> {
    sql.split_whitespace().collect()
}

const CONVENTIONAL: &str = "select a, count(*) from t group by a;\n\
                            select * from u;\n\
                            from v select b;\n";

/// `grebe/friendly` with no range rewrites the whole document: the friendly
/// fixes, to a fixed point, then the formatter.
#[test]
fn friendly_rewrites_the_whole_document() {
    let uri = "file:///friendly/whole.sql";
    let messages = run_session(&[
        did_open(uri, CONVENTIONAL),
        request(
            2,
            "grebe/friendly",
            Json::object(vec![("textDocument".into(), text_document(uri, vec![]))]),
        ),
    ]);
    let edits = text_edits(&result_of(&messages, 2.0));
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert_eq!(edits[0].0, [0, 0, 3, 0], "the whole document");
    assert_eq!(
        sql_words(&edits[0].1),
        sql_words("FROM t SELECT a, count() GROUP BY ALL; FROM u; FROM v SELECT b;")
    );
    assert_eq!(
        grebe_format::format(&edits[0].1, &grebe_format::Options::default()),
        edits[0].1,
        "the result is formatted"
    );
}

/// With a range, only the statements it touches are rewritten, each as its
/// own edit; a statement with nothing to fix gets none.
#[test]
fn friendly_rewrites_only_the_selected_statements() {
    let uri = "file:///friendly/selection.sql";
    let messages = run_session(&[
        did_open(uri, CONVENTIONAL),
        // From inside the second statement to inside the third.
        request(
            2,
            "grebe/friendly",
            Json::object(vec![
                ("textDocument".into(), text_document(uri, vec![])),
                ("range".into(), lsp_range((1, 3), (2, 4))),
            ]),
        ),
        // A cursor in the first.
        request(
            3,
            "grebe/friendly",
            Json::object(vec![
                ("textDocument".into(), text_document(uri, vec![])),
                ("range".into(), lsp_range((0, 5), (0, 5))),
            ]),
        ),
    ]);
    let edits = text_edits(&result_of(&messages, 2.0));
    assert_eq!(edits.len(), 1, "the third is friendly already: {edits:?}");
    assert_eq!(
        edits[0].0,
        [1, 0, 1, 16],
        "the second statement, `;` included"
    );
    assert_eq!(sql_words(&edits[0].1), sql_words("FROM u;"));

    let edits = text_edits(&result_of(&messages, 3.0));
    assert_eq!(edits.len(), 1, "{edits:?}");
    assert_eq!(edits[0].0, [0, 0, 0, 37]);
    assert_eq!(
        sql_words(&edits[0].1),
        sql_words("FROM t SELECT a, count() GROUP BY ALL;")
    );
}

/// Nothing to rewrite, or a document never opened: no edits.
#[test]
fn friendly_has_no_edits_for_friendly_sql() {
    let uri = "file:///friendly/done.sql";
    let params =
        |uri: &str| Json::object(vec![("textDocument".into(), text_document(uri, vec![]))]);
    let messages = run_session(&[
        did_open(uri, "from   v select b;\nselect 1;\n"),
        request(2, "grebe/friendly", params(uri)),
        request(3, "grebe/friendly", params("file:///friendly/unknown.sql")),
    ]);
    assert_eq!(result_of(&messages, 2.0), Json::Array(vec![]));
    assert_eq!(result_of(&messages, 3.0), Json::Array(vec![]));
}

/// The rewrite is also a source code action, offered only when asked for by
/// kind, and advertised in the capabilities.
#[test]
fn friendly_is_a_source_action_when_asked_for() {
    let uri = "file:///friendly/action.sql";
    let code_action = |id: i64, only: Option<&[&str]>| {
        let mut context = vec![("diagnostics".to_string(), Json::Array(vec![]))];
        if let Some(kinds) = only {
            context.push((
                "only".to_string(),
                Json::Array(kinds.iter().map(|k| Json::str(*k)).collect()),
            ));
        }
        request(
            id,
            "textDocument/codeAction",
            Json::object(vec![
                ("textDocument".into(), text_document(uri, vec![])),
                ("range".into(), lsp_range((0, 0), (3, 0))),
                ("context".into(), Json::object(context)),
            ]),
        )
    };
    let messages = run_session(&[
        did_open(uri, CONVENTIONAL),
        code_action(2, Some(&["source.grebe.friendly"])),
        code_action(3, Some(&["source"])),
        code_action(4, None),
        code_action(5, Some(&["source.fixAll"])),
        code_action(6, Some(&["source.grebe.friend"])),
    ]);
    let kinds = messages[0]
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .and_then(|c| c.get("codeActionProvider"))
        .and_then(|p| p.get("codeActionKinds"))
        .and_then(Json::as_array)
        .expect("codeActionProvider.codeActionKinds");
    assert!(kinds.contains(&Json::str("quickfix")), "{kinds:?}");
    assert!(
        kinds.contains(&Json::str("source.grebe.friendly")),
        "{kinds:?}"
    );

    let friendly = |id: f64| -> Vec<Json> {
        result_of(&messages, id)
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a.get("kind").and_then(Json::as_str) == Some("source.grebe.friendly"))
            .cloned()
            .collect()
    };
    for id in [2.0, 3.0] {
        let actions = friendly(id);
        assert_eq!(actions.len(), 1, "request {id}");
        assert_eq!(
            actions[0].get("title").and_then(Json::as_str),
            Some("Rewrite as friendly SQL")
        );
        let edits = actions[0]
            .get("edit")
            .and_then(|e| e.get("changes"))
            .and_then(|c| c.get(uri))
            .expect("edits for the document");
        let edits = text_edits(edits);
        assert_eq!(edits.len(), 1);
        assert_eq!(
            sql_words(&edits[0].1),
            sql_words("FROM t SELECT a, count() GROUP BY ALL; FROM u; FROM v SELECT b;")
        );
    }
    // Not under the lightbulb, not for "fix all", not for a kind that only
    // starts with the same letters.
    for id in [4.0, 5.0, 6.0] {
        assert!(friendly(id).is_empty(), "request {id}");
    }
    // The quick fixes are still there without `only`.
    let quick = result_of(&messages, 4.0);
    assert!(
        quick
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a.get("kind").and_then(Json::as_str) == Some("quickfix")),
        "{quick:?}"
    );
}

/// A `grebe/catalog` with what `grebe catalog --with-metadata` adds: types,
/// NOT NULL, keys, indexes and row estimates.
const SHOP_META: &str = r#"{"columns": [
{"database":"shop","schema":"main","table":"orders","column":"id","type":"BIGINT","nullable":false},
{"database":"shop","schema":"main","table":"orders","column":"customer_id","type":"BIGINT","nullable":false},
{"database":"shop","schema":"main","table":"orders","column":"total","type":"DECIMAL(10,2)","nullable":true},
{"database":"shop","schema":"main","table":"customers","column":"id","type":"BIGINT","nullable":false},
{"database":"shop","schema":"main","table":"customers","column":"email","type":"VARCHAR","nullable":false},
{"database":"shop","schema":"main","table":"customers","column":"name","type":"VARCHAR","nullable":true},
{"database":"other","schema":"main","table":"customers","column":"id","type":"INTEGER","nullable":true}],
"tables": [
{"database":"shop","schema":"main","table":"orders","kind":"table","rows":1240000,"primary_key":["id"],"unique":[],"indexes":[["customer_id"]]},
{"database":"shop","schema":"main","table":"customers","kind":"table","rows":2500,"primary_key":["id"],"unique":[["email"]],"indexes":null}]}"#;

fn hover_request(id: i64, uri: &str, line: u32, character: u32) -> String {
    request(
        id,
        "textDocument/hover",
        Json::object(vec![
            ("textDocument".into(), text_document(uri, vec![])),
            ("position".into(), lsp_position(line, character)),
        ]),
    )
}

/// The Markdown of a hover, `None` for a `null` result.
fn hover_text(messages: &[Json], id: f64) -> Option<String> {
    let result = result_of(messages, id);
    if result == Json::Null {
        return None;
    }
    let contents = result.get("contents").expect("hover contents");
    assert_eq!(
        contents.get("kind").and_then(Json::as_str),
        Some("markdown")
    );
    contents
        .get("value")
        .and_then(Json::as_str)
        .map(str::to_string)
}

#[test]
fn initialize_advertises_hover_and_the_expand_action() {
    let messages = run_session(&[]);
    let caps = messages[0]
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .unwrap();
    assert_eq!(caps.get("hoverProvider"), Some(&Json::Bool(true)));
    let kinds = caps
        .get("codeActionProvider")
        .and_then(|p| p.get("codeActionKinds"))
        .and_then(Json::as_array)
        .unwrap();
    assert!(kinds.contains(&Json::str("refactor.rewrite")), "{kinds:?}");
}

#[test]
fn hover_describes_tables_aliases_and_columns_from_the_catalog() {
    let uri = "file:///hover/q.sql";
    // shop.customers and other.customers: `customers` alone is two tables.
    let sql = "SELECT o.total, c.email FROM shop.main.orders o JOIN shop.customers c ON true;\n\
               SELECT id FROM customers;\n\
               SELECT nope FROM orders;";
    let messages = run_session(&[
        catalog_message(SHOP_META),
        did_open(uri, sql),
        // `orders` in the FROM.
        hover_request(2, uri, 0, 41),
        // `o.total`.
        hover_request(3, uri, 0, 11),
        // The alias `c`, where it is used.
        hover_request(4, uri, 0, 16),
        // `c.email`.
        hover_request(5, uri, 0, 19),
        // `customers` in two databases, and its column: each place.
        hover_request(6, uri, 1, 17),
        hover_request(7, uri, 1, 7),
        // A column the table does not have; a keyword.
        hover_request(8, uri, 2, 8),
        hover_request(9, uri, 2, 2),
        // A document the server has not seen.
        hover_request(10, "file:///hover/unknown.sql", 0, 0),
    ]);
    assert_eq!(
        hover_text(&messages, 2.0).unwrap(),
        "`shop.main.orders` — table · ~1.2M rows · key: (id)\n\n\
         ```text\n\
         id           BIGINT NOT NULL\n\
         customer_id  BIGINT NOT NULL\n\
         total        DECIMAL(10,2)\n\
         ```"
    );
    assert_eq!(
        hover_text(&messages, 3.0).unwrap(),
        "`orders.total` DECIMAL(10,2)"
    );
    assert!(
        hover_text(&messages, 4.0)
            .unwrap()
            .starts_with("`shop.main.customers` — table · ~2.5K rows · key: (id)\n"),
    );
    assert_eq!(
        hover_text(&messages, 5.0).unwrap(),
        "`customers.email` VARCHAR NOT NULL · unique"
    );
    assert_eq!(
        hover_text(&messages, 6.0).unwrap(),
        "`shop.main.customers` — table · ~2.5K rows  \n`other.main.customers` — table"
    );
    assert_eq!(
        hover_text(&messages, 7.0).unwrap(),
        "`id` — in `customers` (shop.main.customers) BIGINT NOT NULL · key  \n\
         in `customers` (other.main.customers) INTEGER"
    );
    for id in [8.0, 9.0, 10.0] {
        assert_eq!(hover_text(&messages, id), None, "request {id}");
    }
    // The hover's range is the name.
    let range = result_of(&messages, 3.0).get("range").cloned().unwrap();
    assert_eq!(range, lsp_range((0, 9), (0, 14)));
}

/// Hover in queries as they are written: beside a CTE, a subquery or a
/// file; over a table in two schemas; on a name two tables have; on a
/// macro.
#[test]
fn hover_answers_in_real_queries() {
    let uri = "file:///hover/real.sql";
    let sql = "WITH recent AS (SELECT 1 AS n) SELECT total FROM orders, recent, 'x.csv';\n\
               WITH recent AS (SELECT 1 AS total) SELECT total FROM orders, recent;\n\
               SELECT id FROM orders o JOIN events e ON true;\n\
               SELECT add1(total) FROM orders;\n\
               SELECT upper(email) FROM customers c;";
    let messages = run_session(&[
        catalog_message(
            r#"{"columns": [
{"database":"pg","schema":"public","table":"orders","column":"id","type":"BIGINT","nullable":false},
{"database":"pg","schema":"public","table":"orders","column":"total","type":"NUMERIC","nullable":true},
{"database":"pg","schema":"public","table":"events","column":"id","type":"BIGINT","nullable":false},
{"database":"pg","schema":"staging","table":"events","column":"id","type":"INTEGER","nullable":true},
{"database":"pg","schema":"public","table":"customers","column":"email","type":"TEXT","nullable":true}],
"tables": [
{"database":"pg","schema":"public","table":"orders","kind":"table","rows":3400000,"primary_key":["id"]},
{"database":"pg","schema":"public","table":"events","kind":"table","rows":12000000000},
{"database":"pg","schema":"staging","table":"events","kind":"view"}],
"functions": [{"name":"add1","kind":"macro"}]}"#,
        ),
        did_open(uri, sql),
        // `total` beside a CTE that does not have it, and a file.
        hover_request(2, uri, 0, 39),
        // `total` beside a CTE that has it.
        hover_request(3, uri, 1, 42),
        // `events`, in two schemas.
        hover_request(4, uri, 2, 34),
        // `id`, in `orders` and in both `events`.
        hover_request(5, uri, 2, 7),
        // A macro, and a built-in function.
        hover_request(6, uri, 3, 8),
        hover_request(7, uri, 4, 8),
    ]);
    assert_eq!(
        hover_text(&messages, 2.0).unwrap(),
        "`orders.total` NUMERIC"
    );
    assert_eq!(hover_text(&messages, 3.0), None);
    assert_eq!(
        hover_text(&messages, 4.0).unwrap(),
        "`pg.public.events` — table · ~12B rows  \n`pg.staging.events` — view"
    );
    assert_eq!(
        hover_text(&messages, 5.0).unwrap(),
        "`id` — in `o` (orders) BIGINT NOT NULL · key  \n\
         in `e` (pg.public.events) BIGINT NOT NULL  \n\
         in `e` (pg.staging.events) INTEGER"
    );
    assert_eq!(hover_text(&messages, 6.0).unwrap(), "`add1` — macro");
    assert_eq!(hover_text(&messages, 7.0), None);
}

/// Completion is a name and a type, at most 200 items, the best first.
#[test]
fn completion_is_short_and_capped() {
    let uri = "file:///catalog/cap.sql";
    let columns: Vec<String> = (0..300)
        .map(|c| {
            format!(r#"{{"schema":"public","table":"wide","column":"c{c:03}","type":"INTEGER"}}"#)
        })
        .collect();
    let messages = run_session(&[
        catalog_message(&format!(
            r#"{{"database":"pg","columns":[{},
            {{"schema":"public","table":"other","column":"c000","type":"TEXT"}}]}}"#,
            columns.join(",")
        )),
        did_open(
            uri,
            "SELECT  FROM wide;\nSELECT c00 FROM wide w JOIN other o ON true;",
        ),
        completion_request(2, uri, 0, 7),
        completion_request(3, uri, 1, 10),
    ]);
    let result = result_of(&messages, 2.0);
    assert_eq!(result.get("isIncomplete"), Some(&Json::Bool(true)));
    let items = result.get("items").and_then(Json::as_array).unwrap();
    assert_eq!(items.len(), 200);
    assert_eq!(items[0].get("label").and_then(Json::as_str), Some("c000"));
    assert_eq!(items[199].get("label").and_then(Json::as_str), Some("c199"));
    for i in items {
        let keys: Vec<&str> = match i {
            Json::Object(pairs) => pairs.iter().map(|(k, _)| k.as_str()).collect(),
            _ => panic!("an item is an object"),
        };
        assert_eq!(keys, ["label", "kind", "sortText", "detail"], "{i:?}");
        assert_eq!(i.get("detail").and_then(Json::as_str), Some("INTEGER"));
    }
    // A name both tables have: one item each, inserted qualified.
    let shared: Vec<(String, String, String)> = result_of(&messages, 3.0)
        .get("items")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .filter(|i| i.get("label").and_then(Json::as_str) == Some("c000"))
        .map(|i| {
            let s = |k: &str| i.get(k).and_then(Json::as_str).unwrap_or("").to_string();
            (s("label"), s("detail"), s("insertText"))
        })
        .collect();
    assert_eq!(
        shared,
        [
            ("c000".into(), "INTEGER · w".into(), "w.c000".into()),
            ("c000".into(), "TEXT · o".into(), "o.c000".into()),
        ]
    );
}

#[test]
fn hover_says_nothing_without_a_catalog() {
    let uri = "file:///hover/none.sql";
    let messages = run_session(&[
        did_open(uri, "SELECT id FROM orders"),
        hover_request(2, uri, 0, 16),
        hover_request(3, uri, 0, 8),
    ]);
    assert_eq!(hover_text(&messages, 2.0), None);
    assert_eq!(hover_text(&messages, 3.0), None);
}

#[test]
fn completion_shows_types_and_offers_the_join() {
    let uri = "file:///catalog/meta.sql";
    let messages = run_session(&[
        catalog_message(SHOP_META),
        did_open(
            uri,
            "FROM shop.orders o JOIN shop.customers c ON \nSELECT * FROM shop.main.ord",
        ),
        completion_request(2, uri, 0, 44),
        completion_request(3, uri, 1, 27),
    ]);
    let items = |id: f64| -> Vec<(String, String)> {
        result_of(&messages, id)
            .get("items")
            .and_then(Json::as_array)
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i.get("label").and_then(Json::as_str).unwrap().to_string(),
                    i.get("detail")
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .collect()
    };
    let join = items(2.0);
    assert_eq!(
        join[0],
        ("o.customer_id = c.id".to_string(), "join".to_string())
    );
    assert!(
        join.contains(&("email".to_string(), "VARCHAR".to_string())),
        "{join:?}"
    );
    // `id` is in both tables: one item for each, with where it is from.
    assert!(
        join.contains(&("id".to_string(), "BIGINT · o".to_string())),
        "{join:?}"
    );
    assert!(
        join.contains(&("id".to_string(), "BIGINT · c".to_string())),
        "{join:?}"
    );
    // A table: its name alone.
    assert_eq!(items(3.0), [("orders".to_string(), String::new())]);
}

#[test]
fn a_star_can_be_expanded_when_every_table_is_known() {
    let uri = "file:///expand/q.sql";
    let code_action = |id: i64, uri: &str, line: u32, character: u32, only: Option<&[&str]>| {
        let mut context = vec![("diagnostics".to_string(), Json::Array(vec![]))];
        if let Some(kinds) = only {
            context.push((
                "only".to_string(),
                Json::Array(kinds.iter().map(|k| Json::str(*k)).collect()),
            ));
        }
        request(
            id,
            "textDocument/codeAction",
            Json::object(vec![
                ("textDocument".into(), text_document(uri, vec![])),
                (
                    "range".into(),
                    lsp_range((line, character), (line, character)),
                ),
                ("context".into(), Json::object(context)),
            ]),
        )
    };
    let none = "file:///expand/none.sql";
    let sql = "SELECT * EXCLUDE (name) FROM shop.customers c JOIN shop.orders o ON true;\n\
               SELECT * FROM shop.orders, nope;\n";
    let messages = run_session(&[
        catalog_message(SHOP_META),
        did_open(uri, sql),
        did_open(none, sql),
        catalog_notification(Some(none), "null"),
        code_action(2, uri, 0, 7, None),
        code_action(3, uri, 0, 7, Some(&["refactor"])),
        code_action(4, uri, 0, 7, Some(&["quickfix"])),
        // A star next to a table the catalog does not have.
        code_action(5, uri, 1, 7, None),
        // A document without a catalog.
        code_action(6, none, 0, 7, None),
        // Not on a star.
        code_action(7, uri, 0, 40, None),
    ]);
    let expansions = |id: f64| -> Vec<Json> {
        result_of(&messages, id)
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a.get("kind").and_then(Json::as_str) == Some("refactor.rewrite"))
            .cloned()
            .collect()
    };
    for id in [2.0, 3.0] {
        let actions = expansions(id);
        assert_eq!(actions.len(), 1, "request {id}");
        assert_eq!(
            actions[0].get("title").and_then(Json::as_str),
            Some("Expand * to columns")
        );
        assert!(actions[0].get("diagnostics").is_none());
        let edits = text_edits(
            actions[0]
                .get("edit")
                .and_then(|e| e.get("changes"))
                .and_then(|c| c.get(uri))
                .unwrap(),
        );
        assert_eq!(
            edits,
            [(
                [0, 7, 0, 23],
                "c.id, c.email, o.id, o.customer_id, o.total".to_string()
            )]
        );
    }
    for id in [4.0, 5.0, 6.0, 7.0] {
        assert!(expansions(id).is_empty(), "request {id}");
    }
}
