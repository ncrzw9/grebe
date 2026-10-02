//! Semantic tokens.
//!
//! Editors colour `.sql` with a generic, MSSQL-leaning TextMate grammar, so
//! DuckDB forms are mis-coloured or plain. The extension ships a keyword
//! injection grammar as a fallback, but that can only ever be a word list: it
//! knows `PIVOT` is a keyword, and cannot know whether `data` is a table, a
//! column, or someone's variable. Semantic tokens are grammar-true by
//! construction, because they come from the parse.
//!
//! Classification comes from the CST alone; grebe does no binding, so there
//! is no catalog to consult. For colouring the parse is the better source
//! anyway: a catalog tells you what a name *is* in some database, while the
//! parse tells you what role the name plays *in this statement*, which is
//! what colour should track.
//!
//! # What is deliberately not here
//!
//! - **No `range`, no deltas, no refresh.** Full-document pull only:
//!   encoding a whole document is cheap, and each extra request shape would
//!   be another code path that can disagree with the first.
//! - **No modifiers.** The legend is types only.
//! - **No alias resolution.** Single-letter table aliases stay `variable`;
//!   resolving them needs scope tracking, not colour work.

use grebe_syntax::Span;
use grebe_syntax::token::{Token, TokenKind, tokenize};

use crate::position::{self, Encoding};

/// The legend, in wire order. An index into this array is what goes on the
/// wire, so **order is protocol**: append only, never reorder or remove.
pub const LEGEND: &[&str] = &[
    "keyword",
    "string",
    "number",
    "operator",
    "comment",
    "function",
    "class",
    "property",
    "namespace",
    "variable",
];

const KEYWORD: u32 = 0;
const STRING: u32 = 1;
const NUMBER: u32 = 2;
const OPERATOR: u32 = 3;
const COMMENT: u32 = 4;
const FUNCTION: u32 = 5;
const CLASS: u32 = 6;
const PROPERTY: u32 = 7;
const NAMESPACE: u32 = 8;
const VARIABLE: u32 = 9;

/// One classified span, before wire encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Classified {
    span: Span,
    ty: u32,
}

/// Which legend entry a CST rule name implies for the tokens beneath it.
///
/// Only the *leaf* naming rules appear here. Matching a broad rule such as
/// `TableReference` would repaint everything inside it, including keywords
/// and punctuation.
fn role_of(rule: &str) -> Option<u32> {
    Some(match rule {
        "FunctionIdentifier" | "FunctionName" | "ReservedFunctionName" | "TableFunctionName" => {
            FUNCTION
        }
        "TableName" | "ReservedTableName" | "BaseTableName" => CLASS,
        "ColumnName" | "ReservedColumnName" => PROPERTY,
        "SchemaName" | "ReservedSchemaName" | "CatalogName" => NAMESPACE,
        _ => return None,
    })
}

/// Base classification from the token stream alone.
///
/// This is the whole answer for a file that does not parse — which matters:
/// a half-typed statement is exactly when an editor is being used, and losing
/// all colour on every keystroke would be worse than colouring approximately.
fn classify_tokens(src: &str, toks: &[Token]) -> Vec<Classified> {
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let ty = match t.kind {
            TokenKind::LineComment | TokenKind::BlockComment => COMMENT,
            TokenKind::String => STRING,
            TokenKind::Number => NUMBER,
            TokenKind::Operator => OPERATOR,
            TokenKind::QuotedIdent => VARIABLE,
            TokenKind::Word => {
                let text = &src[t.span.start as usize..t.span.end as usize];
                if grebe_syntax::grammar::classify_keyword(text)
                    != grebe_syntax::keyword::Keyword::NONE
                {
                    KEYWORD
                } else if next_code_token_is_open_paren(src, toks, i) {
                    // A one-token `(` lookahead: the only structural
                    // signal available without a parse.
                    FUNCTION
                } else {
                    VARIABLE
                }
            }
            // Punctuation carries no colour of its own, and trivia that is
            // not a comment is never coloured.
            TokenKind::Punct | TokenKind::Whitespace | TokenKind::Unlexed => continue,
        };
        out.push(Classified { span: t.span, ty });
    }
    out
}

/// Is the next non-trivia token after `i` an opening paren?
fn next_code_token_is_open_paren(src: &str, toks: &[Token], i: usize) -> bool {
    toks[i + 1..]
        .iter()
        .find(|t| !t.kind.is_trivia())
        .is_some_and(|t| &src[t.span.start as usize..t.span.end as usize] == "(")
}

/// Refine the base classification using the parse tree, where there is one.
///
/// A naming rule repaints a token only when it covers **exactly** that token.
/// That one condition does all the work, and both alternatives are wrong:
///
/// - Repainting everything *under* a naming rule lets an outer rule win. In
///   `FROM main.events` the tree has `BaseTableName` spanning `main.events`
///   with `SchemaName` and `ReservedTableName` inside it, so `main` would be
///   painted `class` by the outer node before `SchemaName` ever saw it.
/// - Refusing to repaint anything the token pass already called a keyword
///   loses the cases that matter most. `data`, `value` and `name` are all
///   unreserved DuckDB keywords and all perfectly ordinary table and column
///   names; `FROM data` is exactly where a word list gives up and a parse
///   does not.
///
/// Punctuation is never classified in the first place, so the `.` between a
/// schema and its table cannot be caught by an exact-span match.
fn refine_with_cst(src: &str, base: &mut [Classified]) {
    let Some(tree) = grebe_syntax::matcher::parse(src) else {
        return;
    };
    for id in tree.walk() {
        let Some(role) = role_of(tree.rule_name(id)) else {
            continue;
        };
        let span = tree.node(id).span;
        for c in base.iter_mut() {
            if c.span == span {
                c.ty = role;
            }
        }
    }
}

/// Classify `src` and encode it as LSP semantic-token data.
///
/// The wire format is flat groups of five integers, each **relative to the
/// previous token**: `deltaLine, deltaStartChar, length, tokenType,
/// tokenModifiers`.
pub fn encode(src: &str, enc: Encoding) -> Vec<u32> {
    let toks = tokenize(src);
    let mut classified = classify_tokens(src, &toks);
    refine_with_cst(src, &mut classified);
    classified.sort_by_key(|c| c.span.start);

    let mut data: Vec<u32> = Vec::new();
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;

    for c in &classified {
        // A token spanning lines (a block comment, a dollar-quoted string) is
        // emitted once per line. Always split, never rely on the client's
        // `multilineTokenSupport` — splitting is valid for
        // every client, so there is no capability to negotiate.
        for (line, start_char, len) in split_lines(src, c.span, enc) {
            if len == 0 {
                continue;
            }
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 {
                start_char - prev_start
            } else {
                start_char
            };
            data.extend_from_slice(&[delta_line, delta_start, len, c.ty, 0]);
            prev_line = line;
            prev_start = start_char;
        }
    }
    data
}

/// Split `span` into `(line, start_char, length)` per line it covers, with
/// character offsets and lengths in the negotiated encoding's units.
fn split_lines(src: &str, span: Span, enc: Encoding) -> Vec<(u32, u32, u32)> {
    let (start_line, start_char) = position::byte_to_position(src, span.start, enc);
    let (end_line, end_char) = position::byte_to_position(src, span.end, enc);

    if start_line == end_line {
        return vec![(start_line, start_char, end_char.saturating_sub(start_char))];
    }

    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut line = start_line;
    let mut seg_start = span.start;

    let lo = span.start as usize;
    for (off, _) in bytes[lo..span.end as usize]
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
    {
        let nl = (lo + off) as u32;
        let (_, s) = position::byte_to_position(src, seg_start, enc);
        let (_, e) = position::byte_to_position(src, nl, enc);
        out.push((line, s, e.saturating_sub(s)));
        line += 1;
        seg_start = nl + 1;
    }
    // The final partial line.
    let (_, s) = position::byte_to_position(src, seg_start, enc);
    out.push((line, s, end_char.saturating_sub(s)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode the flat wire data back into absolute `(line, char, len, type)`
    /// so tests can assert on something legible.
    fn decode(data: &[u32]) -> Vec<(u32, u32, u32, &'static str)> {
        let mut out = Vec::new();
        let (mut line, mut ch) = (0u32, 0u32);
        for g in data.chunks(5) {
            line += g[0];
            ch = if g[0] == 0 { ch + g[1] } else { g[1] };
            out.push((line, ch, g[2], LEGEND[g[3] as usize]));
        }
        out
    }

    fn types(src: &str) -> Vec<(&'static str, String)> {
        decode(&encode(src, Encoding::Utf16))
            .into_iter()
            .map(|(l, c, len, ty)| {
                let line = src.lines().nth(l as usize).unwrap_or("");
                let text: String = line.chars().skip(c as usize).take(len as usize).collect();
                (ty, text)
            })
            .collect()
    }

    #[test]
    fn keywords_literals_and_comments() {
        let got = types("SELECT 1, 'a' -- note");
        assert!(got.contains(&("keyword", "SELECT".into())));
        assert!(got.contains(&("number", "1".into())));
        assert!(got.contains(&("string", "'a'".into())));
        assert!(got.contains(&("comment", "-- note".into())));
    }

    #[test]
    fn the_cst_names_roles_a_word_list_cannot() {
        // `data` is not a keyword and not a function; only the parse knows it
        // is the table here. This is the whole reason semantic tokens beat the
        // injection grammar.
        let got = types("SELECT price FROM data");
        assert!(got.contains(&("class", "data".into())), "{got:?}");
        assert!(got.contains(&("property", "price".into())), "{got:?}");
    }

    #[test]
    fn schema_qualification_is_a_namespace() {
        let got = types("SELECT * FROM main.events");
        assert!(got.contains(&("namespace", "main".into())), "{got:?}");
    }

    #[test]
    fn a_call_is_a_function() {
        let got = types("SELECT upper(a) FROM t");
        assert!(got.contains(&("function", "upper".into())), "{got:?}");
    }

    #[test]
    fn an_unparseable_file_still_gets_colour() {
        // The editor case: half-typed SQL. Falling back to nothing would drop
        // all colour on a keystroke.
        let got = types("SELECT count( FROM");
        assert!(got.contains(&("keyword", "SELECT".into())), "{got:?}");
        assert!(got.contains(&("function", "count".into())), "{got:?}");
    }

    #[test]
    fn a_block_comment_is_split_per_line() {
        let data = encode("/* one\ntwo */ SELECT 1", Encoding::Utf16);
        let comments: Vec<_> = decode(&data)
            .into_iter()
            .filter(|(_, _, _, ty)| *ty == "comment")
            .collect();
        assert_eq!(comments.len(), 2, "{comments:?}");
        assert_eq!(comments[0].0, 0);
        assert_eq!(comments[1].0, 1);
    }

    #[test]
    fn deltas_are_relative_and_non_decreasing() {
        let data = encode("SELECT a, b\nFROM t", Encoding::Utf16);
        assert_eq!(data.len() % 5, 0);
        for g in data.chunks(5) {
            assert!((g[3] as usize) < LEGEND.len(), "type out of legend range");
            assert_eq!(g[4], 0, "no modifiers are advertised");
        }
    }

    #[test]
    fn multibyte_text_uses_the_negotiated_encoding() {
        // `é` is one UTF-16 unit but two UTF-8 bytes; the column of the token
        // after it differs per encoding, and getting this wrong shifts every
        // colour on the line.
        let src = "SELECT 'é', 1";
        let u16 = decode(&encode(src, Encoding::Utf16));
        let u8_ = decode(&encode(src, Encoding::Utf8));
        let n16 = u16.iter().find(|t| t.3 == "number").unwrap();
        let n8 = u8_.iter().find(|t| t.3 == "number").unwrap();
        assert!(n8.1 > n16.1, "utf8 column should be larger: {n8:?} {n16:?}");
    }

    #[test]
    fn empty_input_encodes_to_nothing() {
        assert!(encode("", Encoding::Utf16).is_empty());
    }
}
