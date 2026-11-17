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

const SINGLE_BYTE: &[char] = &['(', ')', '{', '}', '[', ']', ',', '?', '$', '#'];
/// The characters an operator is made of (`Tokenizer::CharacterIsOperator`
/// in DuckDB 2.0).
const OPRUN: &[char] = &[
    '+', '-', '*', '/', '%', '^', '<', '>', '=', '~', '!', '@', '&', '|',
];
const CONTROL: &[char] = &['\'', ';', '"', '`', '.', ':'];
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

/// Where a single-quoted string ends, and whether its closing quote was
/// found. An unclosed one runs to the end of the input.
fn string_end(bytes: &[u8], mut j: usize, esc: bool) -> (usize, bool) {
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
            return (j + 1, true);
        }
        j += 1;
    }
    (n, false)
}

/// A string, quoted identifier or block comment missing its closing
/// delimiter runs to the end of the input as [`TokenKind::Unlexed`], which
/// no grammar rule accepts: DuckDB rejects it, so the statement must fail to
/// parse rather than be linted or formatted as if it ended somewhere.
const fn closed_or_unlexed(kind: TokenKind, closed: bool) -> TokenKind {
    if closed { kind } else { TokenKind::Unlexed }
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
    // A UTF-8 byte order mark, as some Windows editors write, is not part
    // of the first word: DuckDB skips it, so it is leading whitespace.
    if src.starts_with('\u{feff}') {
        tokens.push(Token {
            kind: TokenKind::Whitespace,
            span: Span::new(0, 3),
        });
        i = 3;
    }
    while i < n {
        let start = i;
        let c = bytes[i] as char;

        // String
        if c == '\'' {
            let (end, closed) = string_end(bytes, i + 1, false);
            tokens.push(Token {
                kind: closed_or_unlexed(TokenKind::String, closed),
                span: Span::new(start as u32, end as u32),
            });
            i = end;
            continue;
        }

        // Quoted identifier. Double quotes are the standard delimiter;
        // DuckDB 2.0 also takes backticks, with the same doubling rule.
        if c == '"' || c == '`' {
            let delim = bytes[i];
            let mut j = i + 1;
            while j < n {
                if bytes[j] == delim {
                    if j + 1 < n && bytes[j + 1] == delim {
                        j += 2;
                        continue;
                    }
                    break;
                }
                j += 1;
            }
            let end = if j < n { j + 1 } else { n };
            tokens.push(Token {
                kind: closed_or_unlexed(TokenKind::QuotedIdent, j < n),
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
                let mut closed = false;
                let mut k = j + 1;
                while k + marker.len() <= n {
                    if &bytes[k..k + marker.len()] == marker {
                        end = k + marker.len();
                        closed = true;
                        break;
                    }
                    k += 1;
                }
                tokens.push(Token {
                    kind: closed_or_unlexed(TokenKind::String, closed),
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
                kind: closed_or_unlexed(TokenKind::BlockComment, depth == 0),
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
            let (end, closed) = string_end(bytes, i + 2, is_escape);
            tokens.push(Token {
                kind: closed_or_unlexed(TokenKind::String, closed),
                span: Span::new(start as u32, end as u32),
            });
            i = end;
            continue;
        }

        // Colons: `::` (cast) and `:=` (named argument) are one token each;
        // any other `:` stands alone (slices, `$name:`-style parameters).
        if c == ':' {
            let len = if i + 1 < n && (bytes[i + 1] == b':' || bytes[i + 1] == b'=') {
                2
            } else {
                1
            };
            tokens.push(Token {
                kind: TokenKind::Operator,
                span: Span::new(start as u32, (start + len) as u32),
            });
            i += len;
            continue;
        }

        // Operator runs. As in DuckDB's tokenizer (and PostgreSQL's lexer
        // before it), a run of operator characters is ONE token, so `->>`,
        // `<->`, `||` and `!~~*` need no table of known operators. Two
        // exceptions: a comment opener ends the run (`a+/* c */b`), and a run
        // may not end in `+` or `-` unless it contains one of `OP_SPECIALS`,
        // so `*-` and `=-` lex as the operator followed by a prefix sign.
        if OPRUN.contains(&c) {
            let mut j = i;
            while j < n && OPRUN.contains(&(bytes[j] as char)) {
                if j + 1 < n && (&bytes[j..j + 2] == b"--" || &bytes[j..j + 2] == b"/*") {
                    break;
                }
                j += 1;
            }
            let mut end = j;
            if !bytes[i..j]
                .iter()
                .any(|&b| OP_SPECIALS.contains(&(b as char)))
            {
                while end > i + 1 && (bytes[end - 1] == b'+' || bytes[end - 1] == b'-') {
                    end -= 1;
                }
            }
            tokens.push(Token {
                kind: TokenKind::Operator,
                span: Span::new(i as u32, end as u32),
            });
            for extra in end..j {
                tokens.push(Token {
                    kind: TokenKind::Operator,
                    span: Span::new(extra as u32, (extra + 1) as u32),
                });
            }
            i = j;
            continue;
        }

        // Bare word
        // `$` is identifier material after the first character (`a$b`), as
        // in PostgreSQL; at the start it is a parameter or a dollar quote.
        let mut j = i;
        while j < n && (is_word_char(bytes[j] as char) || (j > i && bytes[j] == b'$')) {
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

/// Byte ranges of top-level statements, split on every `;` outside
/// strings and comments.
///
/// A linter must not lose a whole file because one statement in it does not
/// parse — SQL files in the wild often contain the odd unparseable statement,
/// and dropping the file would discard every finding in it. That includes a
/// statement with an unclosed `(`: no DuckDB statement has a `;` inside
/// brackets, so counting brackets across a `;` would only carry the rest
/// of the file into one broken statement.
/// Splitting uses the tokenizer, so semicolons inside strings, comments and
/// dollar-quoted bodies are never split points.
#[must_use]
pub fn split_statements(src: &str) -> Vec<crate::Span> {
    let toks = tokenize(src);
    let mut out = Vec::new();
    let mut start: Option<u32> = None;
    for t in &toks {
        if t.kind.is_trivia() {
            continue;
        }
        let raw = t.span.slice(src.as_bytes());
        if start.is_none() {
            start = Some(t.span.start);
        }
        if raw == b";" {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The code tokens of `src`, as text.
    fn code(src: &str) -> Vec<&str> {
        tokenize(src)
            .into_iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| &src[t.span.start as usize..t.span.end as usize])
            .collect()
    }

    #[test]
    fn a_run_of_operator_characters_is_one_token() {
        assert_eq!(code("a <-> b"), ["a", "<->", "b"]);
        assert_eq!(code("j->>'k'"), ["j", "->>", "'k'"]);
        assert_eq!(code("a!~~*b"), ["a", "!~~*", "b"]);
        assert_eq!(code("a||b"), ["a", "||", "b"]);
        assert_eq!(code("x <@ y"), ["x", "<@", "y"]);
    }

    #[test]
    fn a_run_does_not_end_in_a_sign_unless_it_has_a_special_character() {
        // PostgreSQL's rule, which DuckDB keeps: `=-` is `=` and a prefix `-`.
        assert_eq!(code("a=-1"), ["a", "=", "-", "1"]);
        assert_eq!(code("a*-b"), ["a", "*", "-", "b"]);
        assert_eq!(code("a**-b"), ["a", "**", "-", "b"]);
        assert_eq!(code("a<=+b"), ["a", "<=", "+", "b"]);
        // `|` is one of the special characters, so the sign stays in the run.
        assert_eq!(code("a||-b"), ["a", "||-", "b"]);
        assert_eq!(code("a - -b"), ["a", "-", "-", "b"]);
    }

    #[test]
    fn a_comment_opener_ends_an_operator_run() {
        assert_eq!(code("a+--c\nb"), ["a", "+", "b"]);
        assert_eq!(code("8//**/2"), ["8", "/", "2"]);
    }

    #[test]
    fn colons_are_their_own_tokens() {
        assert_eq!(code("x::INT"), ["x", "::", "INT"]);
        assert_eq!(code("f(a := 1)"), ["f", "(", "a", ":=", "1", ")"]);
        assert_eq!(code("l[1:-1]"), ["l", "[", "1", ":", "-", "1", "]"]);
    }

    #[test]
    fn backticks_quote_an_identifier() {
        let toks = tokenize("`a``b` `x");
        assert_eq!(toks[0].kind, TokenKind::QuotedIdent);
        assert_eq!(toks[0].span, Span::new(0, 6));
        // Unclosed: runs to the end of the input and matches nothing.
        assert_eq!(toks.last().unwrap().kind, TokenKind::Unlexed);
    }

    #[test]
    fn a_dollar_continues_a_word_but_does_not_start_one() {
        assert_eq!(code("a$b a$1"), ["a$b", "a$1"]);
        assert_eq!(code("$1"), ["$", "1"]);
        assert_eq!(code("$$x$$"), ["$$x$$"]);
    }
}
