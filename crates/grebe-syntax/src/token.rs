//! Tokenizer — a port of DuckDB's `src/parser/peg/tokenizer/base_tokenizer.cpp`.
//!
//! Unlike `duckdb.tokenize()` (which is C++-only, absent from the stable C API,
//! and whose token *types* changed between 1.5.5 and the 2.0
//! preview), this is ours: stable, and every token carries a real
//! extent rather than a bare start offset that downstream scanners must guess
//! the end of. Dispatch is on leading bytes, never on an engine's type tags.
//!
//! # Port checklist
//!
//! The behaviours `base_tokenizer.cpp` implements that this must reproduce:
//!
//! - line comments `-- ...` and block comments `/* ... */`, **nesting**
//! - single-quoted strings with `''` doubling; `E'...'` escape strings
//! - double-quoted identifiers with `""` doubling
//! - dollar quoting, both `$$...$$` and tagged `$tag$...$tag$`
//! - the multi-byte operators `->>`, `::`, `:=`, `->`, `**`, `//`
//! - single-byte operators
//! - the PostgreSQL trailing-`+` trim on operator runs
//! - hex (`0x1F`) and binary (`0b1010`) literals, which DuckDB's lexer emits as
//!   two tokens, merged into one
//! - numeric literals with digit separators (`1_000_000`), leading/trailing
//!   dots (`.5`, `5.`), and exponents (`1e-3`, `1.5e+2`)
//!
//! # Losslessness
//!
//! Every byte of input belongs to exactly one token, trivia included. This is
//! the property the formatter and every span-bearing diagnostic rest on, and it
//! is a test: concatenating all token spans in order must reproduce the input
//! byte for byte, and no two spans may overlap. Bytes that fit no token kind
//! become explicit [`TokenKind::Unlexed`] tokens, so no layer may assume every
//! non-trivia byte lexed cleanly.

use crate::Span;

/// What a token is, lexically. Structure comes from the matcher, not from here.
///
/// Deliberately *not* a mirror of DuckDB's `SimplifiedTokenType`: those tags
/// are unstable across engine versions (`::`, `->` and `:` were `keyword` on
/// 1.5.5 and `operator` on the 2.0 preview), and dispatching on them is
/// unsound regardless.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum TokenKind {
    /// A bare word. Whether it is a keyword — and in which category — is the
    /// matcher's question, resolved against [`crate::keyword`], not the lexer's.
    Word,
    Number,
    /// Single-quoted, `E'...'`, or dollar-quoted.
    String,
    /// Double-quoted identifier.
    QuotedIdent,
    Operator,
    /// `(` `)` `[` `]` `,` `;` `.`
    Punct,
    Whitespace,
    LineComment,
    BlockComment,
    /// A byte the tokenizer could not classify. Never silently dropped — it is
    /// carried so losslessness holds and the matcher can reject precisely.
    Unlexed,
}

impl TokenKind {
    /// Trivia is everything the grammar never sees but the formatter must keep.
    #[must_use]
    pub const fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::LineComment | Self::BlockComment
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

const SINGLE_BYTE: &[char] = &['(', ')', '{', '}', '[', ']', ',', '?', '$', '-', '#'];
const OPRUN: &[char] = &[
    '+', '*', '/', '<', '>', '=', '~', '!', '@', '%', '^', '&', '|', '`',
];
const CONTROL: &[char] = &['\'', '-', ';', '"', '.'];
const SPECIAL_OPS: &[&str] = &["->>", "::", ":=", "->", "**", "//"];
const OP_SPECIALS: &[char] = &['~', '!', '@', '#', '%', '^', '&', '|', '`', '?'];

fn is_word_char(c: char) -> bool {
    // Any non-ASCII byte is identifier material. Casting a raw UTF-8
    // continuation byte to `char` yields a Latin-1 codepoint, and two of
    // those (U+0085 NEL, U+00A0 NBSP) are Unicode whitespace -- testing them
    // as characters would split `à` (0xC3 0xA0) mid-character and produce a
    // token span off a char boundary, which panics the first `from_utf8`
    // downstream.
    if !c.is_ascii() {
        return true;
    }
    !c.is_whitespace()
        && !SINGLE_BYTE.contains(&c)
        && !OPRUN.contains(&c)
        && !CONTROL.contains(&c)
        && c != ':'
}

fn string_end(bytes: &[u8], mut j: usize, esc: bool) -> usize {
    let n = bytes.len();
    while j < n {
        if esc && bytes[j] == b'\\' {
            j += 2;
            continue;
        }
        if bytes[j] == b'\'' {
            if j + 1 < n && bytes[j + 1] == b'\'' {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    n
}

fn parse_number(bytes: &[u8], start: usize) -> usize {
    let n = bytes.len();
    let mut j = start;
    while j < n {
        let c = bytes[j] as char;
        if c.is_ascii_digit() || c == '.' {
            j += 1;
            continue;
        }
        if c == '_'
            && j + 1 < n
            && ((bytes[j + 1] as char).is_ascii_digit() || bytes[j + 1] == b'.')
        {
            j += 1;
            continue;
        }
        if (c == 'e' || c == 'E')
            && j > start
            && (bytes[j - 1] as char).is_ascii_digit()
            && j + 1 < n
            && ((bytes[j + 1] as char).is_ascii_digit()
                || (bytes[j + 1] == b'+' || bytes[j + 1] == b'-'))
        {
            if bytes[j + 1] == b'+' || bytes[j + 1] == b'-' {
                if j + 2 < n && (bytes[j + 2] as char).is_ascii_digit() {
                    j += 3;
                    continue;
                }
            } else {
                j += 2;
                continue;
            }
        }
        break;
    }
    while j > start + 1 && !(bytes[j - 1] as char).is_ascii_digit() && bytes[j - 1] != b'.' {
        j -= 1;
    }
    j
}

/// Tokenize the SQL input losslessly, preserving all trivia (whitespace, comments).
#[must_use]
pub fn tokenize(src: &str) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        let start = i;
        let c = bytes[i] as char;

        // String
        if c == '\'' {
            let end = string_end(bytes, i + 1, false);
            tokens.push(Token {
                kind: TokenKind::String,
                span: Span::new(start as u32, end as u32),
            });
            i = end;
            continue;
        }

        // Quoted Identifier
        if c == '"' {
            let mut j = i + 1;
            while j < n {
                if bytes[j] == b'"' {
                    if j + 1 < n && bytes[j + 1] == b'"' {
                        j += 2;
                        continue;
                    }
                    break;
                }
                j += 1;
            }
            let end = if j < n { j + 1 } else { n };
            tokens.push(Token {
                kind: TokenKind::QuotedIdent,
                span: Span::new(start as u32, end as u32),
            });
            i = end;
            continue;
        }

        // Punctuation
        if c == ';'
            || c == ','
            || c == '('
            || c == ')'
            || c == '['
            || c == ']'
            || (c == '.' && !(i + 1 < n && (bytes[i + 1] as char).is_ascii_digit()))
        {
            tokens.push(Token {
                kind: TokenKind::Punct,
                span: Span::new(start as u32, (start + 1) as u32),
            });
            i += 1;
            continue;
        }

        // Dollar quoting or operator $
        if c == '$' {
            // `$1` is a positional parameter, not a dollar-quote tag: emit the
            // `$` alone and let the digits lex as a number.
            if i + 1 < n && (bytes[i + 1] as char).is_ascii_digit() {
                tokens.push(Token {
                    kind: TokenKind::Operator,
                    span: Span::new(start as u32, (start + 1) as u32),
                });
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < n
                && ((bytes[j] as char).is_alphanumeric() || bytes[j] == b'_' || bytes[j] >= 0x80)
            {
                j += 1;
            }
            if j < n && bytes[j] == b'$' && j > i {
                let marker = &bytes[i..=j];
                let mut end = n;
                let mut k = j + 1;
                while k + marker.len() <= n {
                    if &bytes[k..k + marker.len()] == marker {
                        end = k + marker.len();
                        break;
                    }
                    k += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::String,
                    span: Span::new(start as u32, end as u32),
                });
                i = end;
                continue;
            }
            tokens.push(Token {
                kind: TokenKind::Operator,
                span: Span::new(start as u32, (start + 1) as u32),
            });
            i += 1;
            continue;
        }

        // Comments
        if c == '-' && i + 1 < n && bytes[i + 1] == b'-' {
            let mut j = i + 2;
            while j < n && bytes[j] != b'\n' {
                j += 1;
            }
            tokens.push(Token {
                kind: TokenKind::LineComment,
                span: Span::new(start as u32, j as u32),
            });
            i = j;
            continue;
        }

        if c == '/' && i + 1 < n && bytes[i + 1] == b'*' {
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if j + 1 < n && &bytes[j..j + 2] == b"/*" {
                    depth += 1;
                    j += 2;
                } else if j + 1 < n && &bytes[j..j + 2] == b"*/" {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            tokens.push(Token {
                kind: TokenKind::BlockComment,
                span: Span::new(start as u32, j as u32),
            });
            i = j;
            continue;
        }

        // Whitespace -- ASCII only; a non-ASCII byte is never whitespace here.
        if c.is_ascii_whitespace() {
            let mut j = i + 1;
            while j < n && (bytes[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Whitespace,
                span: Span::new(start as u32, j as u32),
            });
            i = j;
            continue;
        }

        // Special multi-byte operators
        let mut special_op_matched = false;
        for op in SPECIAL_OPS.iter() {
            let op_bytes = op.as_bytes();
            if i + op_bytes.len() <= n && &bytes[i..i + op_bytes.len()] == op_bytes {
                tokens.push(Token {
                    kind: TokenKind::Operator,
                    span: Span::new(start as u32, (start + op_bytes.len()) as u32),
                });
                i += op_bytes.len();
                special_op_matched = true;
                break;
            }
        }
        if special_op_matched {
            continue;
        }

        // Number
        if c.is_ascii_digit() || (c == '.' && i + 1 < n && (bytes[i + 1] as char).is_ascii_digit())
        {
            let end = parse_number(bytes, i);
            tokens.push(Token {
                kind: TokenKind::Number,
                span: Span::new(start as u32, end as u32),
            });
            i = end;
            continue;
        }

        // Prefix string (N', X', E', B')
        if (c == 'n'
            || c == 'N'
            || c == 'x'
            || c == 'X'
            || c == 'e'
            || c == 'E'
            || c == 'b'
            || c == 'B')
            && i + 1 < n
            && bytes[i + 1] == b'\''
        {
            let is_escape = c == 'e' || c == 'E';
            let end = string_end(bytes, i + 2, is_escape);
            tokens.push(Token {
                kind: TokenKind::String,
                span: Span::new(start as u32, end as u32),
            });
            i = end;
            continue;
        }

        // Operator runs or colons
        if OPRUN.contains(&c) || c == ':' {
            let mut j = i;
            while j < n && (OPRUN.contains(&(bytes[j] as char)) || bytes[j] == b':') {
                // A comment opener ends the run, so `a+/* c */b` keeps its comment.
                if j + 1 < n && &bytes[j..j + 2] == b"--" {
                    break;
                }
                if j + 1 < n && &bytes[j..j + 2] == b"/*" {
                    break;
                }
                j += 1;
            }
            let run_len = j - i;
            let run_start = i;
            let mut k = 0;
            while k < run_len {
                let current_idx = run_start + k;
                let remaining_bytes = &bytes[current_idx..j];
                let mut matched = false;
                for op in SPECIAL_OPS.iter() {
                    let op_bytes = op.as_bytes();
                    if remaining_bytes.starts_with(op_bytes) {
                        tokens.push(Token {
                            kind: TokenKind::Operator,
                            span: Span::new(
                                current_idx as u32,
                                (current_idx + op_bytes.len()) as u32,
                            ),
                        });
                        k += op_bytes.len();
                        matched = true;
                        break;
                    }
                }
                if matched {
                    continue;
                }
                if bytes[current_idx] == b':' {
                    tokens.push(Token {
                        kind: TokenKind::Operator,
                        span: Span::new(current_idx as u32, (current_idx + 1) as u32),
                    });
                    k += 1;
                    continue;
                }
                let mut m = k;
                while m < run_len
                    && OPRUN.contains(&(bytes[run_start + m] as char))
                    && bytes[run_start + m] != b':'
                {
                    let sub_rem = &bytes[(run_start + m)..j];
                    if SPECIAL_OPS
                        .iter()
                        .any(|op| sub_rem.starts_with(op.as_bytes()))
                    {
                        break;
                    }
                    m += 1;
                }
                // The PostgreSQL rule: a multi-byte operator may not end in
                // `+` unless it contains one of `OP_SPECIALS`, so `*+` lexes
                // as `*` then `+` (a prefix plus on the next operand).
                let mut piece_len = m - k;
                while piece_len > 1 && bytes[run_start + k + piece_len - 1] == b'+' {
                    let piece_str =
                        std::str::from_utf8(&bytes[run_start + k..run_start + k + piece_len])
                            .unwrap();
                    if !piece_str.chars().any(|ch| OP_SPECIALS.contains(&ch)) {
                        piece_len -= 1;
                    } else {
                        break;
                    }
                }
                tokens.push(Token {
                    kind: TokenKind::Operator,
                    span: Span::new((run_start + k) as u32, (run_start + k + piece_len) as u32),
                });
                for extra in (k + piece_len)..m {
                    tokens.push(Token {
                        kind: TokenKind::Operator,
                        span: Span::new((run_start + extra) as u32, (run_start + extra + 1) as u32),
                    });
                }
                k = m;
            }
            i = j;
            continue;
        }

        // Bare word
        let mut j = i;
        while j < n && is_word_char(bytes[j] as char) {
            j += 1;
        }
        if j == i {
            tokens.push(Token {
                kind: TokenKind::Unlexed,
                span: Span::new(start as u32, (start + 1) as u32),
            });
            i += 1;
        } else {
            tokens.push(Token {
                kind: TokenKind::Word,
                span: Span::new(start as u32, j as u32),
            });
            i = j;
        }
    }

    // Hex/bin merge
    let mut merged = Vec::new();
    let mut k = 0;
    while k < tokens.len() {
        let t = tokens[k];
        if t.kind == TokenKind::Number && k + 1 < tokens.len() {
            let next_t = tokens[k + 1];
            let num_text = std::str::from_utf8(t.span.slice(bytes)).unwrap();
            let next_text = std::str::from_utf8(next_t.span.slice(bytes)).unwrap();
            if num_text == "0" && next_t.kind == TokenKind::Word && next_t.span.start == t.span.end
            {
                if let Some(first_char) = next_text.chars().next() {
                    if first_char == 'x'
                        || first_char == 'X'
                        || first_char == 'b'
                        || first_char == 'B'
                    {
                        merged.push(Token {
                            kind: TokenKind::Number,
                            span: Span::new(t.span.start, next_t.span.end),
                        });
                        k += 2;
                        continue;
                    }
                }
            }
        }
        merged.push(t);
        k += 1;
    }
    merged
}

/// Byte ranges of top-level statements, split on `;` outside parens.
///
/// A linter must not lose a whole file because one statement in it does not
/// parse — SQL files in the wild often contain the odd unparseable statement,
/// and dropping the file would discard every finding in it.
/// Splitting uses the tokenizer, so semicolons inside strings, comments and
/// dollar-quoted bodies are never split points.
#[must_use]
pub fn split_statements(src: &str) -> Vec<crate::Span> {
    let toks = tokenize(src);
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut start: Option<u32> = None;
    for t in &toks {
        if t.kind.is_trivia() {
            continue;
        }
        let raw = t.span.slice(src.as_bytes());
        match raw {
            b"(" | b"[" => depth += 1,
            b")" | b"]" => depth -= 1,
            _ => {}
        }
        if start.is_none() {
            start = Some(t.span.start);
        }
        // `<= 0` rather than `== 0`: a stray closer must not stop every later
        // statement from splitting.
        if raw == b";" && depth <= 0 {
            if let Some(s) = start.take() {
                if t.span.end > s {
                    out.push(crate::Span::new(s, t.span.end));
                }
            }
        }
    }
    // A trailing statement with no terminating semicolon still counts.
    if let Some(s) = start {
        let end = src.len() as u32;
        if end > s {
            out.push(crate::Span::new(s, end));
        }
    }
    out
}
