//! `-- grebe: format off` / `-- grebe: format on`: where the formatter is
//! switched off.
//!
//! This finds the regions; [`crate::format`] emits them as written and
//! formats what is between them.
//!
//! A marker is a line comment on a line of its own, and nothing else: not a
//! comment after code, not a block comment, not the same words inside a
//! string. `-- fmt: off` and `-- fmt: on` are accepted too, and letter case
//! does not matter. An `on` with no `off` before it is an ordinary comment;
//! an `off` with no `on` after it runs to the end of the file.
//!
//! These are not the linter's `-- grebe: ignore[...]` comments, which
//! silence findings and have nothing to say about layout. A suppression
//! comment inside a region is kept like every other byte of it; one outside
//! is a comment like any other.

use grebe_syntax::token::{Token, TokenKind, split_statements, tokenize};

/// `s` without `prefix`, compared without regard to ASCII case.
fn strip_prefix_ignore_case<'s>(s: &'s str, prefix: &str) -> Option<&'s str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Is this line comment a marker? `Some(true)` switches the formatter off,
/// `Some(false)` back on.
fn marker(comment: &str) -> Option<bool> {
    let rest = comment.strip_prefix("--")?.trim();
    let rest = match strip_prefix_ignore_case(rest, "grebe:") {
        Some(rest) => {
            let rest = strip_prefix_ignore_case(rest.trim_start(), "format")?;
            // `format off`, not `formatoff`.
            if !rest.starts_with(char::is_whitespace) {
                return None;
            }
            rest
        }
        None => strip_prefix_ignore_case(rest, "fmt:")?,
    };
    let rest = rest.trim_start();
    if rest.eq_ignore_ascii_case("off") {
        Some(true)
    } else if rest.eq_ignore_ascii_case("on") {
        Some(false)
    } else {
        None
    }
}

/// The offset of the start of the line `at` is on.
fn line_start(src: &str, at: usize) -> usize {
    src[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The offset of the line break that ends the line `at` is on, or the end
/// of the buffer.
fn line_end(src: &str, at: usize) -> usize {
    src[at..].find('\n').map_or(src.len(), |i| at + i)
}

/// The marker a token is, if it is one: a marker comment with nothing but
/// white space before it on its line.
fn marker_token(src: &str, t: &Token) -> Option<bool> {
    if t.kind != TokenKind::LineComment {
        return None;
    }
    let start = t.span.start as usize;
    let off = marker(&src[start..t.span.end as usize])?;
    src[line_start(src, start)..start]
        .trim()
        .is_empty()
        .then_some(off)
}

/// The byte ranges to emit as written, in order and apart from each other,
/// or `None` when the formatter is not switched off anywhere -- which is
/// decided without tokenizing, so a file with no marker pays a scan for
/// `--` and nothing more.
///
/// A range starts at the start of a line, or at the first token of a
/// statement that has code before it on its line. It ends before the line
/// break that ends its last line, or right after a statement's `;` when
/// more code follows on that line, or at the end of the buffer.
pub(crate) fn regions(src: &str) -> Option<Vec<(usize, usize)>> {
    let candidate = src
        .match_indices("--")
        .any(|(at, _)| marker(&src[at..line_end(src, at)]) == Some(true));
    if !candidate {
        return None;
    }
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<usize> = None;
    for t in &tokenize(src) {
        match (marker_token(src, t), open) {
            (Some(true), None) => open = Some(line_start(src, t.span.start as usize)),
            (Some(false), Some(start)) => {
                regions.push((start, line_end(src, t.span.end as usize)));
                open = None;
            }
            _ => {}
        }
    }
    if let Some(start) = open {
        regions.push((start, src.len()));
    }
    if regions.is_empty() {
        return None;
    }
    // A statement that reaches into a region is in it, all of it.
    let statements = split_statements(src);
    for region in &mut regions {
        for sp in &statements {
            let (s, e) = (sp.start as usize, sp.end as usize);
            if s >= region.1 || e <= region.0 {
                continue;
            }
            if s < region.0 {
                let line = line_start(src, s);
                region.0 = if src[line..s].trim().is_empty() {
                    line
                } else {
                    s
                };
            }
            if e > region.1 {
                // The rest of the terminator's line goes with the statement
                // when it is nothing, or a comment that trails it.
                let eol = line_end(src, e);
                let rest = src[e..eol].trim();
                region.1 = if rest.is_empty() || rest.starts_with("--") {
                    eol
                } else {
                    e
                };
            }
        }
    }
    // Two regions one statement reaches into are one region.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(regions.len());
    for region in regions {
        match merged.last_mut() {
            Some(last) if region.0 <= last.1 => last.1 = last.1.max(region.1),
            _ => merged.push(region),
        }
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_recognised_in_both_spellings_and_any_case() {
        for (text, want) in [
            ("-- grebe: format off", Some(true)),
            ("-- grebe: format on", Some(false)),
            ("--grebe:format off", Some(true)),
            ("-- GREBE: FORMAT OFF  ", Some(true)),
            ("-- fmt: off", Some(true)),
            ("-- fmt: on", Some(false)),
            ("--fmt:off", Some(true)),
            ("-- Fmt: On", Some(false)),
            ("-- grebe: formatoff", None),
            ("-- grebe: format off please", None),
            ("-- grebe: format", None),
            ("-- grebe: ignore[MOD001]", None),
            ("-- grebe: ignore", None),
            ("-- fmt: skip", None),
            ("-- format off", None),
            ("/* fmt: off */", None),
        ] {
            assert_eq!(marker(text), want, "{text:?}");
        }
    }

    #[test]
    fn a_file_without_a_marker_has_no_regions() {
        assert_eq!(regions("select 1; -- a comment\n"), None);
        // An `on` alone, a marker after code, one in a string, one in a
        // block comment: none of them switches anything off.
        assert_eq!(regions("-- fmt: on\nselect 1;\n"), None);
        assert_eq!(regions("select 1; -- fmt: off\nselect 2;\n"), None);
        assert_eq!(regions("select '\n-- fmt: off\n';\n"), None);
        assert_eq!(regions("/*\n-- fmt: off\n*/\nselect 1;\n"), None);
    }

    #[test]
    fn a_region_is_whole_lines_and_whole_statements() {
        let slices = |src: &'static str| -> Vec<&'static str> {
            regions(src)
                .unwrap_or_default()
                .into_iter()
                .map(|(s, e)| &src[s..e])
                .collect()
        };
        assert_eq!(
            slices("select 1;\n  -- fmt: off\nselect   2;\n-- fmt: on  \nselect 3;\n"),
            ["  -- fmt: off\nselect   2;\n-- fmt: on  "]
        );
        // Unterminated: to the end of the file.
        assert_eq!(
            slices("select 1;\n-- fmt: off\nselect   2;\n\n"),
            ["-- fmt: off\nselect   2;\n\n"]
        );
        // A marker inside a statement takes the whole statement, and the
        // comment after its terminator.
        assert_eq!(
            slices("select 1;\n  select a,\n-- fmt: off\n  b from t; -- why\nselect 3;\n"),
            ["  select a,\n-- fmt: off\n  b from t; -- why\nselect 3;\n"]
        );
        assert_eq!(
            slices("select 1; select a,\n-- fmt: off\n b\n-- fmt: on\n from t; select 3;\n"),
            ["select a,\n-- fmt: off\n b\n-- fmt: on\n from t;"]
        );
        // The comment after the terminator goes with the statement; a
        // block comment there does not.
        assert_eq!(
            slices("-- fmt: off\nselect a\n-- fmt: on\n from t; -- why\nselect 3;\n"),
            ["-- fmt: off\nselect a\n-- fmt: on\n from t; -- why"]
        );
        assert_eq!(
            slices("-- fmt: off\nselect a\n-- fmt: on\n from t; /* why */\nselect 3;\n"),
            ["-- fmt: off\nselect a\n-- fmt: on\n from t;"]
        );
        // A second `off` inside a region changes nothing; two regions stay
        // two.
        assert_eq!(
            slices(
                "-- fmt: off\na;\n-- fmt: off\nb;\n-- fmt: on\nc;\n-- fmt: off\nd;\n-- fmt: on\n"
            ),
            [
                "-- fmt: off\na;\n-- fmt: off\nb;\n-- fmt: on",
                "-- fmt: off\nd;\n-- fmt: on"
            ]
        );
    }
}
