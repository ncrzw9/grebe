//! `grebe-lsp` — a language server for DuckDB SQL over stdio.
//!
//! Standard library only, `Content-Length` framing, JSON-RPC 2.0, positions
//! converted to/from bytes at the boundary. Framing and dispatch are a small,
//! fixed amount of code and analysis is where request time goes, so a
//! protocol framework would add dependencies without making anything faster.
//! The server is a thin front end over the same analysis crates the CLI uses,
//! and exchanges nothing with them but source text and byte spans.
//!
//! It handles the `initialize`/`initialized`/`shutdown`/`exit` lifecycle;
//! `textDocument/didOpen|didChange|didSave|didClose`, each publishing the
//! diagnostics `grebe check` would report; quick fixes
//! (`textDocument/codeAction`); whole-document formatting; semantic tokens;
//! `workspace/didChangeConfiguration` for the `grebe.select` setting; and
//! `grebe/statements`, which tells an editor where each statement of a
//! document begins and ends so it can run them one at a time.
//!
//! Everything but [`serve`] is `pub(crate)`-shaped in spirit but kept
//! `pub` where tests in `tests/` need it; [`serve`] is the only API the
//! CLI crate is meant to call.

pub mod code_action;
pub mod json;
pub mod position;
pub mod semantic;
pub mod server;
pub mod statements;
pub mod transport;

/// Run the LSP server on the real `stdin`/`stdout`. Blocks until the
/// client ends the connection. Returns a process exit code: `0` only for
/// a clean `shutdown` request followed by an `exit` notification; `1`
/// otherwise (premature EOF, `exit` without `shutdown`).
#[must_use]
pub fn serve() -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    server::run(stdin.lock(), stdout.lock())
}
