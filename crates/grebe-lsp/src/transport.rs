//! stdio framing: `Content-Length: N\r\n\r\n<N bytes of body>`, per LSP's
//! base protocol.
//!
//! Generic over `BufRead`/`Write` so the dispatch loop in [`crate::server`]
//! can be driven by an in-memory buffer in tests, with the real
//! `stdin`/`stdout` wired in only by [`crate::serve`].

use std::io::{self, BufRead, Write};

/// Read one message's body from `reader`.
///
/// Returns `Ok(None)` on a clean end-of-stream *before* any header bytes
/// were read — the normal way a client closes the pipe after `exit`.
/// Anything else short of a well-formed header block plus exactly
/// `Content-Length` body bytes is an `Err`; the caller decides whether that
/// is fatal to the connection (see `server::run`: a bad body is skipped, a
/// broken header block ends the loop, since framing itself can't recover).
pub fn read_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    let mut header_seen = false;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            if header_seen {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended mid-headers",
                ));
            }
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // Blank line ends the header block. If we never saw a header
            // line before it, there is nothing to frame — malformed.
            break;
        }
        header_seen = true;
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let Some(len) = content_length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing or unparsable Content-Length header",
        ));
    };
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Write one message: the `Content-Length` header, a blank line, then the
/// body verbatim (byte length, not char length — the body may contain
/// multi-byte UTF-8).
pub fn write_message<W: Write>(writer: &mut W, body: &str) -> io::Result<()> {
    let bytes = body.as_bytes();
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
    writer.write_all(bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_a_simple_body() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, r#"{"a":1}"#).unwrap();
        let mut cursor = Cursor::new(buf);
        let got = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(got, br#"{"a":1}"#);
    }

    #[test]
    fn body_may_contain_crlf_inside_a_string() {
        // The framing header's blank-line terminator must not be confused
        // with a `\r\n` that appears *inside* the JSON body text.
        let body = "{\"message\":\"line1\\r\\nline2\"}";
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, body).unwrap();
        let mut cursor = Cursor::new(buf);
        let got = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(got, body.as_bytes());
    }

    #[test]
    fn reads_two_consecutive_messages() {
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, "1").unwrap();
        write_message(&mut buf, "22").unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(read_message(&mut cursor).unwrap().unwrap(), b"1");
        assert_eq!(read_message(&mut cursor).unwrap().unwrap(), b"22");
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn clean_eof_before_any_header_is_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn missing_content_length_is_an_error() {
        let mut cursor = Cursor::new(b"X-Other: 1\r\n\r\n".to_vec());
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn extra_headers_before_content_length_are_ignored() {
        let mut cursor = Cursor::new(
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\nok"
                .to_vec(),
        );
        assert_eq!(read_message(&mut cursor).unwrap().unwrap(), b"ok");
    }

    #[test]
    fn truncated_body_is_an_error() {
        let mut cursor = Cursor::new(b"Content-Length: 10\r\n\r\nshort".to_vec());
        assert!(read_message(&mut cursor).is_err());
    }
}
