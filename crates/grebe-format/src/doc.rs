//! The `Doc` IR and its printer — Wadler's "prettier printer", in the shape
//! Prettier gave it.
//!
//! The one idea: a [`Doc::Group`] is printed flat if it fits in the remaining
//! width, and broken otherwise. Deciding a break by whether a whole subtree
//! fits is what a line-string rewriter cannot do, and it is
//! the entire reason the formatter has a tree.
//!
//! Three refinements carried over from Prettier, each earning its keep:
//!
//! - **`fits` looks past the group.** A group that fits on its own but is
//!   followed by `)` or `,` that would not is measured with that trailing
//!   text included, up to the next line break in the enclosing (already
//!   broken) mode. Without this, a nested call's closing paren dangles past
//!   the width.
//! - **Hard breaks propagate.** A `HardLine` anywhere inside a group means
//!   the group cannot be flat, and neither can any group containing it. The
//!   flag is computed once, at construction, so printing never re-walks.
//! - **Line suffixes.** A trailing `-- comment` must stay at the end of its
//!   line, whatever the layout decides. It is deferred to the next newline
//!   and, since a line comment swallows everything after it, the next line
//!   opportunity after it always breaks, even in a flat group. A caller that
//!   needs the whole enclosing group broken adds a [`Doc::BreakParent`].

use std::borrow::Cow;

/// A layout description. Build these bottom-up; `Doc::group` computes its
/// forced-break flag from the finished subtree.
///
/// Text is borrowed where it can be: nearly every token prints exactly as
/// the source spells it, and a `String` apiece was a tenth of the time to
/// format a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Doc<'a> {
    Nil,
    /// Literal text. Newlines inside are honoured but not planned around —
    /// they occur only in multi-line block comments and dollar-quoted strings.
    Text(Cow<'a, str>),
    /// A space when flat, a newline when broken.
    Line,
    /// Nothing when flat, a newline when broken.
    SoftLine,
    /// Always a newline. Forces every enclosing group to break.
    HardLine,
    /// A newline unless the current line holds nothing but indentation.
    /// Forces every enclosing group to break. This is how a comment gets a
    /// line of its own without ever producing a blank line next to a break
    /// the layout was going to make anyway.
    OwnLine,
    Concat(Vec<Doc<'a>>),
    /// One more indent level for every newline inside.
    Indent(Box<Doc<'a>>),
    /// Flat if it fits, broken otherwise. `forced` is true if the subtree
    /// contains a `HardLine`, `OwnLine` or `BreakParent` on a path that would
    /// print flat.
    Group {
        doc: Box<Doc<'a>>,
        forced: bool,
    },
    /// Chooses by the mode of the nearest enclosing group.
    IfBreak {
        broken: Box<Doc<'a>>,
        flat: Box<Doc<'a>>,
    },
    /// Emitted just before the next newline, whatever comes between.
    LineSuffix(String),
    /// Zero-width; marks every enclosing group as broken.
    BreakParent,
}

impl<'a> Doc<'a> {
    pub fn text(s: impl Into<Cow<'a, str>>) -> Self {
        Self::Text(s.into())
    }

    #[must_use]
    pub fn concat(parts: Vec<Self>) -> Self {
        Self::Concat(parts)
    }

    #[must_use]
    pub fn indent(doc: Self) -> Self {
        Self::Indent(Box::new(doc))
    }

    #[must_use]
    pub fn group(doc: Self) -> Self {
        let forced = doc.forces_break();
        Self::Group {
            doc: Box::new(doc),
            forced,
        }
    }

    #[must_use]
    pub fn if_break(broken: Self, flat: Self) -> Self {
        Self::IfBreak {
            broken: Box::new(broken),
            flat: Box::new(flat),
        }
    }

    /// Does printing this doc flat ever emit a hard newline?
    ///
    /// `IfBreak` counts only its `flat` branch: the `broken` branch is by
    /// definition never printed flat, so a `HardLine` there is not a reason
    /// to break (it is how "blank line, but only when broken" is spelled).
    #[must_use]
    pub fn forces_break(&self) -> bool {
        match self {
            Self::HardLine | Self::OwnLine | Self::BreakParent => true,
            Self::Nil | Self::Text(_) | Self::Line | Self::SoftLine | Self::LineSuffix(_) => false,
            Self::Concat(v) => v.iter().any(Self::forces_break),
            Self::Indent(d) => d.forces_break(),
            Self::Group { forced, .. } => *forced,
            Self::IfBreak { flat, .. } => flat.forces_break(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Flat,
    Break,
}

/// Display width of a string, in characters. Good enough: SQL is not where
/// double-width glyphs live, and the width is a soft target anyway.
fn width(s: &str) -> usize {
    s.chars().count()
}

/// Render `doc` at `width` columns, `indent_size` spaces per level.
///
/// The output never carries trailing spaces and ends without a newline; the
/// caller decides how a file ends.
#[must_use]
pub fn print(doc: &Doc<'_>, width_limit: usize, indent_size: usize) -> String {
    let mut printer = Printer::new(width_limit, indent_size);
    printer.print(doc);
    printer.finish()
}

/// The printer, for output that is built from more than one `Doc`: printing
/// documents one after the other gives what printing their concatenation
/// gives, as long as each is followed by a hard line break or by nothing --
/// `fits` never looks past either. A file is printed a statement at a time
/// this way, and no more than one statement's `Doc` exists at once.
pub struct Printer {
    out: String,
    pos: usize,
    suffix: Vec<String>,
    /// A line comment has been deferred to the end of this line. Whatever
    /// follows it must start on the next line, even inside a group that was
    /// going to stay flat: otherwise the comment ends up after code it did
    /// not trail, and re-formatting the output attaches it there -- not
    /// idempotent.
    pending_break: bool,
    width_limit: usize,
    indent_size: usize,
}

impl Printer {
    #[must_use]
    pub fn new(width_limit: usize, indent_size: usize) -> Self {
        Self {
            out: String::new(),
            pos: 0,
            suffix: Vec::new(),
            pending_break: false,
            width_limit,
            indent_size,
        }
    }

    /// Everything printed so far, with any deferred line suffix placed.
    #[must_use]
    pub fn finish(mut self) -> String {
        self.flush_suffix();
        self.trim_line_end();
        self.out
    }

    /// A line that holds nothing but indentation. `Line`/`SoftLine` breaks and
    /// `OwnLine` do nothing on a fresh line: the newline they wanted is already
    /// there. Only `HardLine` insists, which is how a blank line is spelled.
    fn fresh(&self) -> bool {
        self.suffix.is_empty()
            && self
                .out
                .rsplit('\n')
                .next()
                .is_some_and(|l| l.trim().is_empty())
    }

    /// Start a new line at `ind` levels. Indentation is always spaces, never
    /// tabs, so the output looks the same in every editor.
    fn newline(&mut self, ind: usize) {
        self.flush_suffix();
        self.trim_line_end();
        self.out.push('\n');
        for _ in 0..ind * self.indent_size {
            self.out.push(' ');
        }
        self.pos = ind * self.indent_size;
        self.pending_break = false;
    }

    fn flush_suffix(&mut self) {
        for s in self.suffix.drain(..) {
            self.out.push_str(&s);
        }
    }

    fn trim_line_end(&mut self) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
    }

    fn push_text(&mut self, s: &str) {
        self.out.push_str(s);
        self.pos = match s.rfind('\n') {
            Some(i) => width(&s[i + 1..]),
            None => self.pos + width(s),
        };
    }

    /// An unconditional line break at the left margin, as a `HardLine`
    /// printed on its own would make.
    pub fn hard_line(&mut self) {
        self.newline(0);
    }

    /// Print `doc` after what has been printed already.
    pub fn print(&mut self, doc: &Doc<'_>) {
        let mut stack: Vec<(usize, Mode, &Doc<'_>)> = vec![(0, Mode::Break, doc)];
        // `fits` runs for every group that is not already flat; its stack
        // is kept between calls.
        let mut scratch: Vec<(Mode, &Doc<'_>)> = Vec::new();

        while let Some((ind, mode, d)) = stack.pop() {
            match d {
                Doc::Nil | Doc::BreakParent => {}
                // A deferred line comment must end its line before the next
                // code, not after it: with no break between them (`WHERE -- c`
                // then `x`, joined by a plain space), the comment would land
                // after `x`, and the next run would attach it there -- then lay
                // the statement out differently. The space is dropped; the code
                // starts the next line. A `,` or `;` still closes the line it
                // ends, before the comment: `a, -- c` is where the next run
                // finds that comment, so it stays put. So does a `;` set off
                // by a space, which is one text so that the space stays too.
                Doc::Text(s) if self.pending_break && s.trim().is_empty() => {}
                Doc::Text(s)
                    if self.pending_break && !matches!(s.as_ref(), "" | "," | ";" | " ;") =>
                {
                    self.newline(ind);
                    self.push_text(s);
                }
                Doc::Text(s) => self.push_text(s),
                Doc::Line => match mode {
                    Mode::Flat if !self.pending_break => {
                        self.out.push(' ');
                        self.pos += 1;
                    }
                    _ => {
                        if !self.fresh() {
                            self.newline(ind);
                        }
                    }
                },
                Doc::SoftLine => {
                    if (mode == Mode::Break || self.pending_break) && !self.fresh() {
                        self.newline(ind);
                    }
                }
                Doc::OwnLine => {
                    if !self.fresh() {
                        self.newline(ind);
                    }
                }
                Doc::HardLine => self.newline(ind),
                Doc::Concat(v) => {
                    for part in v.iter().rev() {
                        stack.push((ind, mode, part));
                    }
                }
                Doc::Indent(inner) => stack.push((ind + 1, mode, inner)),
                Doc::Group { doc: inner, forced } => {
                    let m = if *forced {
                        Mode::Break
                    } else if mode == Mode::Flat {
                        // Already measured as part of a flat parent.
                        Mode::Flat
                    } else if fits(
                        inner,
                        self.width_limit.saturating_sub(self.pos),
                        &stack,
                        &mut scratch,
                    ) {
                        Mode::Flat
                    } else {
                        Mode::Break
                    };
                    stack.push((ind, m, inner));
                }
                Doc::IfBreak { broken, flat } => {
                    let pick = if mode == Mode::Break { broken } else { flat };
                    stack.push((ind, mode, pick));
                }
                Doc::LineSuffix(s) => {
                    self.suffix.push(s.clone());
                    self.pending_break = true;
                }
            }
        }
    }
}

/// Would `doc`, printed flat, plus whatever follows it up to the next
/// newline, fit in `remaining` columns?
fn fits<'d, 'a>(
    doc: &'d Doc<'a>,
    remaining: usize,
    rest: &[(usize, Mode, &'d Doc<'a>)],
    stack: &mut Vec<(Mode, &'d Doc<'a>)>,
) -> bool {
    let mut left = remaining as isize;
    stack.clear();
    stack.push((Mode::Flat, doc));
    let mut rest_idx = rest.len();
    loop {
        let (mode, d) = match stack.pop() {
            Some(x) => x,
            None => {
                if rest_idx == 0 {
                    return true;
                }
                rest_idx -= 1;
                let (_, m, d) = rest[rest_idx];
                (m, d)
            }
        };
        match d {
            Doc::Nil | Doc::BreakParent | Doc::LineSuffix(_) => {}
            Doc::Text(s) => {
                if s.contains('\n') {
                    // The line ends inside the text; nothing after it matters.
                    return true;
                }
                left -= width(s) as isize;
                if left < 0 {
                    return false;
                }
            }
            Doc::Line => match mode {
                Mode::Break => return true,
                Mode::Flat => {
                    left -= 1;
                    if left < 0 {
                        return false;
                    }
                }
            },
            Doc::SoftLine => {
                if mode == Mode::Break {
                    return true;
                }
            }
            Doc::HardLine | Doc::OwnLine => return true,
            Doc::Concat(v) => {
                for part in v.iter().rev() {
                    stack.push((mode, part));
                }
            }
            Doc::Indent(inner) => stack.push((mode, inner)),
            Doc::Group { doc: inner, forced } => {
                let m = if *forced { Mode::Break } else { mode };
                stack.push((m, inner));
            }
            Doc::IfBreak { broken, flat } => {
                let pick = if mode == Mode::Break { broken } else { flat };
                stack.push((mode, pick));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Doc<'_> {
        Doc::text(s)
    }

    fn list<'a>(items: &[&'a str]) -> Doc<'a> {
        let mut parts = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if i > 0 {
                parts.push(t(","));
                parts.push(Doc::Line);
            }
            parts.push(t(it));
        }
        Doc::concat(parts)
    }

    #[test]
    fn a_group_that_fits_stays_flat() {
        let d = Doc::group(Doc::concat(vec![
            t("SELECT"),
            Doc::indent(Doc::concat(vec![Doc::Line, list(&["a", "b"])])),
        ]));
        assert_eq!(print(&d, 40, 4), "SELECT a, b");
    }

    #[test]
    fn a_group_that_does_not_fit_breaks_every_line_in_it() {
        let d = Doc::group(Doc::concat(vec![
            t("SELECT"),
            Doc::indent(Doc::concat(vec![
                Doc::Line,
                list(&["alpha", "beta", "gamma"]),
            ])),
        ]));
        assert_eq!(print(&d, 12, 4), "SELECT\n    alpha,\n    beta,\n    gamma");
    }

    #[test]
    fn nested_groups_decide_independently() {
        let inner = Doc::group(Doc::concat(vec![
            t("f("),
            Doc::indent(Doc::concat(vec![Doc::SoftLine, list(&["x", "y"])])),
            Doc::SoftLine,
            t(")"),
        ]));
        let outer = Doc::group(Doc::concat(vec![
            t("SELECT"),
            Doc::indent(Doc::concat(vec![Doc::Line, inner])),
            Doc::Line,
            t("FROM t"),
        ]));
        // Outer breaks (too long), inner still fits on its own line.
        assert_eq!(print(&outer, 14, 4), "SELECT\n    f(x, y)\nFROM t");
    }

    #[test]
    fn fits_looks_past_the_group_to_the_next_break() {
        let inner = Doc::group(Doc::concat(vec![
            t("g("),
            Doc::indent(Doc::concat(vec![Doc::SoftLine, list(&["a", "b"])])),
            Doc::SoftLine,
            t(")"),
        ]));
        let outer = Doc::group(Doc::concat(vec![
            t("f("),
            Doc::indent(Doc::concat(vec![Doc::SoftLine, inner])),
            Doc::SoftLine,
            t(")"),
        ]));
        assert_eq!(
            print(&outer, 8, 4),
            "f(\n    g(\n        a,\n        b\n    )\n)"
        );
        assert_eq!(print(&outer, 10, 4), "f(g(a, b))");

        // `g(a, b)` is 7 columns and fits in 8 on its own, but the `))` that
        // follows it on the same line pushes the line to 9.
        let tail = Doc::concat(vec![inner_call(), t("))")]);
        assert_eq!(print(&tail, 8, 4), "g(\n    a,\n    b\n)))");
        assert_eq!(print(&tail, 9, 4), "g(a, b)))");
    }

    fn inner_call() -> Doc<'static> {
        Doc::group(Doc::concat(vec![
            t("g("),
            Doc::indent(Doc::concat(vec![Doc::SoftLine, list(&["a", "b"])])),
            Doc::SoftLine,
            t(")"),
        ]))
    }

    #[test]
    fn a_hard_line_forces_the_enclosing_groups() {
        let d = Doc::group(Doc::concat(vec![
            t("a"),
            Doc::Line,
            Doc::group(Doc::concat(vec![t("-- c"), Doc::HardLine, t("b")])),
        ]));
        assert!(d.forces_break());
        assert_eq!(print(&d, 80, 4), "a\n-- c\nb");
    }

    #[test]
    fn if_break_hard_line_does_not_force() {
        let d = Doc::group(Doc::concat(vec![
            t("a"),
            Doc::Line,
            Doc::if_break(Doc::HardLine, Doc::Nil),
            t("b"),
        ]));
        assert!(!d.forces_break());
        assert_eq!(print(&d, 80, 4), "a b");
        assert_eq!(print(&d, 1, 4), "a\n\nb");
    }

    #[test]
    fn line_suffix_lands_before_the_newline_and_breaks_the_group() {
        let d = Doc::group(Doc::concat(vec![
            t("a,"),
            Doc::LineSuffix(" -- one".into()),
            Doc::BreakParent,
            Doc::Line,
            t("b"),
        ]));
        assert_eq!(print(&d, 80, 4), "a, -- one\nb");
    }

    /// What the formatter relies on to print a file a statement at a time:
    /// documents separated by hard line breaks print the same one by one
    /// as they do concatenated, a deferred comment and a group measured up
    /// to the end of its line included.
    #[test]
    fn printing_one_by_one_is_printing_the_concatenation() {
        let statement = |name: &'static str| {
            Doc::concat(vec![
                Doc::group(Doc::concat(vec![
                    t(name),
                    Doc::indent(Doc::concat(vec![
                        Doc::Line,
                        list(&["alpha", "beta", "gamma"]),
                    ])),
                    Doc::LineSuffix(" -- tail".into()),
                ])),
                t(";"),
            ])
        };
        let parts = vec![
            statement("SELECT"),
            Doc::HardLine,
            Doc::HardLine,
            statement("VALUES"),
            Doc::OwnLine,
            t("-- footer"),
        ];
        for width in [8, 20, 21, 22, 80] {
            let mut printer = Printer::new(width, 4);
            for part in &parts {
                printer.print(part);
            }
            assert_eq!(
                printer.finish(),
                print(&Doc::concat(parts.clone()), width, 4),
                "width {width}"
            );
        }
        let mut printer = Printer::new(80, 4);
        printer.print(&t("a"));
        printer.hard_line();
        printer.print(&t("b"));
        assert_eq!(printer.finish(), "a\nb");
    }

    #[test]
    fn no_trailing_spaces_before_a_newline() {
        let d = Doc::concat(vec![t("a"), t(" "), Doc::HardLine, t("b")]);
        assert_eq!(print(&d, 80, 4), "a\nb");
    }

    #[test]
    fn indent_applies_only_after_newlines() {
        let d = Doc::indent(Doc::concat(vec![t("x"), Doc::HardLine, t("y")]));
        assert_eq!(print(&d, 80, 2), "x\n  y");
    }
}
