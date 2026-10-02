//! Conversion between byte offsets and LSP `{line, character}` positions,
//! done once at the protocol boundary. grebe spans are byte offsets
//! throughout (`grebe_syntax::Span`); this module is the only place that
//! converts them.
//!
//! LSP 3.17 lets a client negotiate `positionEncoding: "utf-8"`, in which
//! case `character` is a byte offset within the line instead of a UTF-16
//! code-unit offset. Both are implemented and tested, and `negotiate` never
//! picks any other encoding, so no third one is ever advertised.

/// Which unit `character` counts in, as negotiated with the client during
/// `initialize`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Encoding {
    /// LSP's default: UTF-16 code units. Every client supports this.
    #[default]
    Utf16,
    /// Negotiated only when the client's `general.positionEncodings`
    /// offers it — then `character` is a UTF-8 byte offset within the
    /// line, which happens to make the conversion a no-op past line
    /// splitting.
    Utf8,
}

impl Encoding {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Utf16 => "utf-16",
            Encoding::Utf8 => "utf-8",
        }
    }
}

/// Pick an encoding from the client's offered list (`general.positionEncodings`
/// on `initialize`, in the client's preference order). Falls back to the LSP
/// default, UTF-16, when the client didn't send the field or offered nothing
/// we support.
#[must_use]
pub fn negotiate(offered: &[&str]) -> Encoding {
    for enc in offered {
        match *enc {
            "utf-8" => return Encoding::Utf8,
            "utf-16" => return Encoding::Utf16,
            _ => {}
        }
    }
    Encoding::Utf16
}

/// Convert a byte offset into `src` to a 0-based `(line, character)` pair
/// in the given encoding.
///
/// `\r\n` is treated as a single line terminator: an offset that lands on
/// the `\r` of a `\r\n` pair (or exactly between the two, i.e. right after
/// the `\r`) never counts that `\r` toward `character`.
#[must_use]
pub fn byte_to_position(src: &str, byte_offset: u32, enc: Encoding) -> (u32, u32) {
    let bytes = src.as_bytes();
    let offset = (byte_offset as usize).min(bytes.len());

    let mut line = 0u32;
    let mut line_start = 0usize;
    for (i, &b) in bytes[..offset].iter().enumerate() {
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }

    let mut line_bytes = &src[line_start..offset];
    // The offset points exactly at the '\n' of a CRLF pair (i.e. right
    // after the lone '\r') -- don't let that trailing '\r' count.
    if line_bytes.ends_with('\r') && bytes.get(offset) == Some(&b'\n') {
        line_bytes = &line_bytes[..line_bytes.len() - 1];
    }

    let character = match enc {
        Encoding::Utf8 => line_bytes.len() as u32,
        Encoding::Utf16 => line_bytes.chars().map(|c| c.len_utf16() as u32).sum(),
    };
    (line, character)
}

/// The inverse of [`byte_to_position`]: an LSP position back to a byte offset.
///
/// Needed by `textDocument/codeAction`, the one request in which the client
/// tells *us* about a location rather than the other way round.
///
/// Out-of-range input is clamped rather than refused. A client can legitimately
/// send a position past the end of a line (an empty selection at end-of-line,
/// a document edited between request and handling), and answering "no actions
/// here" beats answering with an error.
#[must_use]
pub fn position_to_byte(src: &str, line: u32, character: u32, enc: Encoding) -> u32 {
    // Walk to the start of `line`.
    let mut line_start = 0usize;
    let mut seen = 0u32;
    for (i, b) in src.bytes().enumerate() {
        if seen == line {
            break;
        }
        if b == b'\n' {
            seen += 1;
            line_start = i + 1;
        }
    }
    if seen < line {
        return src.len() as u32; // past the last line
    }

    let rest = &src[line_start..];
    let line_text = rest.split('\n').next().unwrap_or("");
    // A CRLF line's terminator is not addressable content.
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);

    let mut units = 0u32;
    for (off, ch) in line_text.char_indices() {
        if units >= character {
            return (line_start + off) as u32;
        }
        units += match enc {
            Encoding::Utf8 => ch.len_utf8() as u32,
            Encoding::Utf16 => ch.len_utf16() as u32,
        };
    }
    (line_start + line_text.len()) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_to_byte_inverts_byte_to_position() {
        // The five fixture strings (see below), round-tripped.
        //
        // The CRLF seam is skipped, and that is not a gap in the inverse: an
        // offset landing on the '\n' of a "\r\n" pair is deliberately given the
        // same position as the offset before it (see byte_to_position), so two
        // offsets share one position and nothing can invert both. The next
        // test pins that collapse directly.
        for src in ["abc\ndef", "é\nx", "𝄞 y", "a\r\nb", "\tx"] {
            let bytes = src.as_bytes();
            for enc in [Encoding::Utf8, Encoding::Utf16] {
                for off in 0..=src.len() as u32 {
                    let i = off as usize;
                    if !src.is_char_boundary(i) {
                        continue;
                    }
                    if i > 0 && bytes[i - 1] == b'\r' && bytes.get(i) == Some(&b'\n') {
                        continue;
                    }
                    let (l, c) = byte_to_position(src, off, enc);
                    assert_eq!(
                        position_to_byte(src, l, c, enc),
                        off,
                        "round trip failed at {off} in {src:?} ({enc:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_crlf_seam_collapses_and_the_inverse_picks_the_earlier_offset() {
        let src = "a\r\nb";
        // Both the '\r' and the '\n' report as (0, 1) ...
        assert_eq!(byte_to_position(src, 1, Encoding::Utf8), (0, 1));
        assert_eq!(byte_to_position(src, 2, Encoding::Utf8), (0, 1));
        // ... and (0, 1) maps back to the first of them, which is the offset
        // an editor means by "end of line 0".
        assert_eq!(position_to_byte(src, 0, 1, Encoding::Utf8), 1);
    }

    #[test]
    fn position_to_byte_clamps_out_of_range() {
        let src = "ab\ncd";
        assert_eq!(
            position_to_byte(src, 99, 0, Encoding::Utf16),
            src.len() as u32
        );
        assert_eq!(position_to_byte(src, 0, 99, Encoding::Utf16), 2);
    }

    // The five fixture strings:
    // ASCII, 'é' (2 bytes -> 1 UTF-16 unit), '𝄞' (4 bytes -> 2 UTF-16
    // units), CRLF, and tab.

    #[test]
    fn ascii() {
        let src = "select 1";
        // Byte offset 7 is the '1'.
        assert_eq!(byte_to_position(src, 7, Encoding::Utf16), (0, 7));
        assert_eq!(byte_to_position(src, 7, Encoding::Utf8), (0, 7));
    }

    #[test]
    fn two_byte_char_e_acute() {
        // "é" is U+00E9: 2 bytes in UTF-8, 1 unit in UTF-16.
        let src = "select 'é' x"; // s(0)e(1)l(2)e(3)c(4)t(5) (6)'(7)é(8-9)'(10) (11)x(12)
        // Byte offset 10 is the closing quote, right after the 2-byte 'é'.
        assert_eq!(byte_to_position(src, 10, Encoding::Utf16), (0, 9));
        // UTF-8 encoding: character counts bytes, so it's 10.
        assert_eq!(byte_to_position(src, 10, Encoding::Utf8), (0, 10));
    }

    #[test]
    fn four_byte_astral_char() {
        // U+1D11E MUSICAL SYMBOL G CLEF: 4 bytes in UTF-8, a UTF-16
        // surrogate pair (2 units).
        let src = "select '𝄞' x";
        let quote_byte = src.find("' x").unwrap(); // the closing quote
        // Ground truth computed independently of `byte_to_position`'s own
        // arithmetic: the prefix's real byte length and its real UTF-16
        // unit count (`encode_utf16`, not the `len_utf16` the function
        // under test uses).
        let prefix = &src[..quote_byte];
        let want_utf16 = prefix.encode_utf16().count() as u32;
        let want_utf8 = prefix.len() as u32;
        assert_eq!(
            byte_to_position(src, quote_byte as u32, Encoding::Utf16),
            (0, want_utf16)
        );
        assert_eq!(
            byte_to_position(src, quote_byte as u32, Encoding::Utf8),
            (0, want_utf8)
        );
        // Sanity: the surrogate pair really does add 2 UTF-16 units for 4
        // UTF-8 bytes relative to the ASCII-only prefix "select '".
        assert_eq!(want_utf8, 8 + 4);
        assert_eq!(want_utf16, 8 + 2);
    }

    #[test]
    fn crlf_does_not_count_the_carriage_return() {
        let src = "select 1\r\nselect 2";
        // Offset of the '\r' itself: line 0, character 8 (after "select 1").
        assert_eq!(byte_to_position(src, 8, Encoding::Utf16), (0, 8));
        // Offset of the '\n' (right after the '\r'): still character 8,
        // not 9 -- the '\r' must not land in the count.
        assert_eq!(byte_to_position(src, 9, Encoding::Utf16), (0, 8));
        // Start of the second line.
        assert_eq!(byte_to_position(src, 10, Encoding::Utf16), (1, 0));
        // Same three checks under UTF-8 encoding.
        assert_eq!(byte_to_position(src, 8, Encoding::Utf8), (0, 8));
        assert_eq!(byte_to_position(src, 9, Encoding::Utf8), (0, 8));
        assert_eq!(byte_to_position(src, 10, Encoding::Utf8), (1, 0));
    }

    #[test]
    fn tab_counts_as_one_character_not_a_tab_stop() {
        let src = "\tselect 1"; // tab, then 8 more bytes/chars.
        assert_eq!(byte_to_position(src, 1, Encoding::Utf16), (0, 1));
        assert_eq!(byte_to_position(src, 1, Encoding::Utf8), (0, 1));
        assert_eq!(byte_to_position(src, 9, Encoding::Utf16), (0, 9));
    }

    #[test]
    fn negotiate_prefers_client_order() {
        assert_eq!(negotiate(&["utf-8", "utf-16"]), Encoding::Utf8);
        assert_eq!(negotiate(&["utf-16", "utf-8"]), Encoding::Utf16);
        assert_eq!(negotiate(&["utf-32", "utf-8"]), Encoding::Utf8);
        assert_eq!(negotiate(&[]), Encoding::Utf16);
        assert_eq!(negotiate(&["utf-32"]), Encoding::Utf16);
    }
}
