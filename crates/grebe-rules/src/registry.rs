//! The rule registry — pure data. Classification keys on structured fields,
//! never on message prose: DuckDB's message text drifts between versions
//! while structured fields stay stable. [`Rule::message`] is for humans only,
//! and nothing downstream ever matches on it.
//!
//! # Categories
//!
//! grebe does no binding, so there are no semantic (`SEM`) rules and no
//! `SRC001 unverifiable-source`. The categories:
//!
//! | category | meaning | source |
//! |---|---|---|
//! | [`Category::Prs`] | statement does not parse | our PEG matcher |
//! | [`Category::Src`] | statement deliberately not analyzed | head match + parse failure |
//! | [`Category::Mod`] | opinionated rule on valid SQL | our CST |
//!
//! There is no `PRS900 unclassified-parse-error` either: our PEG matcher's
//! reject IS `PRS001` — there is no second engine underneath it to disagree
//! with, so no parse failure goes unclassified.
//!
//! The opt-in band defaults to [`Severity::Off`] (MOD010, MOD017-020,
//! MOD022-023, MOD025, MOD031, MOD033-034).

/// Which layer of the tool produced the rule.
///
/// There is no `SEM` category: grebe does no binding.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Category {
    /// Statement does not parse — our PEG matcher's reject.
    Prs,
    /// Statement deliberately not analyzed: template markup, foreign
    /// dialect, or a parsed-but-not-yet-linted statement type.
    Src,
    /// An opinionated rule over valid SQL, read off our own CST.
    Mod,
}

/// Default severity a rule ships at, absent a `[severity]` override.
///
/// `Error` is reserved for `PRS001`: only a statement that does not parse is
/// an error. `Warning` marks MOD rules whose hit more likely signals a
/// mistake than a style choice; `Info` marks modernizations and readability
/// opinions on code that is already correct.
///
/// `Off` is a real registry entry, not an omission: the rule exists but
/// fires only once a `grebe.toml` `[severity]` entry or `--select` turns it
/// on. The opt-in band (MOD010, MOD017-020, MOD022-023, MOD025, MOD031,
/// MOD033-034) is either too noisy on idiomatic DuckDB SQL to default on,
/// unproven on SQL written by others, or house style by nature; each row in
/// [`RULES`] says which.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Off,
}

/// Fix-safety taxonomy: `--fix` applies `Safe` edits only, `--fix --unsafe`
/// includes `Unsafe` too.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum FixSafety {
    /// The edit preserves semantics — e.g. `count(*)` → `count()`.
    Safe,
    /// The edit changes semantics, even when wrong becomes right — e.g.
    /// `= NULL` → `IS NULL` is a behavior change, not a rewording.
    Unsafe,
    /// No mechanical fix: the rewrite needs human judgement or knowledge
    /// grebe does not have, such as schemas. The rule is report-only.
    None,
}

/// One registry row. Adding a rule means adding a row here — never a new
/// code path.
#[derive(Clone, Copy, Debug)]
pub struct Rule {
    /// e.g. `"MOD001"`. Matches `^(PRS|SRC|MOD)\d{3}$` — asserted in tests,
    /// not just documented, because [`lookup`] and every diagnostic
    /// downstream key on this string.
    pub code: &'static str,
    /// Kebab-case, e.g. `"count-star"`.
    pub name: &'static str,
    pub category: Category,
    pub default_severity: Severity,
    pub fix_safety: FixSafety,
    /// Human-facing prose. Never matched on downstream.
    pub message: &'static str,
}

/// The full rule table.
///
/// Ordered `PRS`, `SRC`, `MOD`. Within `MOD`, the default-on rules come
/// first in code order (with the opt-in MOD010 among them, marked at its
/// row), then the rest of the opt-in band. Each detector's doc comment
/// carries the full guard list and the reasoning behind its fix.
pub static RULES: &[Rule] = &[
    Rule {
        code: "PRS001",
        name: "syntax-error",
        category: Category::Prs,
        default_severity: Severity::Error,
        fix_safety: FixSafety::None,
        message: "Statement does not parse.",
    },
    // SRC002-004. There is no SRC001: flagging unverifiable file-backed
    // sources only matters to a tool that binds, and grebe does not.
    Rule {
        code: "SRC002",
        name: "unrenderable-template",
        category: Category::Src,
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "File contains template markup and is not valid SQL until rendered.",
    },
    Rule {
        code: "SRC003",
        name: "foreign-dialect",
        category: Category::Src,
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "File contains statements from another SQL dialect; skipped.",
    },
    // --- MOD, default-on (MOD010 aside) ---
    Rule {
        code: "MOD001",
        name: "count-star",
        category: Category::Mod,
        // Safe: DuckDB defines `count()` as the same row count as `count(*)`.
        default_severity: Severity::Info,
        fix_safety: FixSafety::Safe,
        message: "count(*) can be count() (friendly SQL).",
    },
    Rule {
        code: "MOD002",
        name: "null-comparison",
        category: Category::Mod,
        // Warning: `= NULL` is always NULL, so it is almost always a live bug.
        // Unsafe: the fix changes results, from wrong to right.
        default_severity: Severity::Warning,
        fix_safety: FixSafety::Unsafe,
        message: "Comparison with NULL via =/!=/<> is always NULL; use IS [NOT] NULL.",
    },
    Rule {
        code: "MOD003",
        name: "group-by-all",
        category: Category::Mod,
        // Safe: fires only when the explicit list is set-equal to what
        // `GROUP BY ALL` infers.
        default_severity: Severity::Info,
        fix_safety: FixSafety::Safe,
        message: "Explicit GROUP BY list equals the non-aggregate select columns; \
                   GROUP BY ALL says the same without repetition.",
    },
    Rule {
        code: "MOD004",
        name: "subquery-order-by",
        category: Category::Mod,
        // No fix: the ORDER BY may be deliberate documentation of the
        // expected order, and deleting it is a human call.
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "ORDER BY in a subquery without LIMIT: the order is not guaranteed \
                   to survive the outer query.",
    },
    Rule {
        code: "MOD005",
        name: "distinct-group-by",
        category: Category::Mod,
        // Warning: the combination is either redundant or hiding a mistake,
        // and which of the two clauses is wrong is a human call.
        default_severity: Severity::Warning,
        fix_safety: FixSafety::None,
        message: "DISTINCT combined with GROUP BY: one is redundant or masking a bug.",
    },
    Rule {
        code: "MOD006",
        name: "ifnull-coalesce",
        category: Category::Mod,
        // Safe: IFNULL and NVL are two-argument aliases of COALESCE.
        default_severity: Severity::Info,
        fix_safety: FixSafety::Safe,
        message: "IFNULL/NVL can be the standard COALESCE.",
    },
    Rule {
        code: "MOD007",
        name: "natural-join",
        category: Category::Mod,
        // Warning: the join condition changes silently when either side's
        // columns change. No fix: spelling out the keys needs both schemas.
        default_severity: Severity::Warning,
        fix_safety: FixSafety::None,
        message: "NATURAL JOIN silently re-pairs when column names change; \
                   spell the join keys out.",
    },
    Rule {
        code: "MOD008",
        name: "bare-union",
        category: Category::Mod,
        // No fix: ALL and DISTINCT give different results, and only the
        // author knows which was meant.
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "UNION without ALL/DISTINCT dedups implicitly; say which you mean.",
    },
    Rule {
        code: "MOD009",
        name: "self-alias",
        category: Category::Mod,
        // Safe: fires only on a bare column reference whose own name matches
        // the alias, ignoring case and quotes.
        default_severity: Severity::Info,
        fix_safety: FixSafety::Safe,
        message: "Aliasing a column to its own name is redundant.",
    },
    // Opt-in: comma joins are idiomatic DuckDB (`range(...) a, range(...) b`)
    // and appear throughout TPC-H; about 408 hits on the TPC-H/TPC-DS and
    // DuckDB-documentation corpus. No fix: an explicit JOIN needs join keys.
    Rule {
        code: "MOD010",
        name: "implicit-cross-join",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::None,
        message: "Comma join in FROM; prefer explicit JOIN ... ON / CROSS JOIN. \
                   Opt-in: set MOD010 severity in grebe.toml.",
    },
    Rule {
        code: "MOD011",
        name: "ordinal-reference",
        category: Category::Mod,
        // No fix: resolving an ordinal to the item it names, and maybe
        // inventing an alias for it, is a human call.
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "GROUP BY/ORDER BY by position breaks silently when the select \
                   list changes; name the columns.",
    },
    Rule {
        code: "MOD012",
        name: "view-order-by",
        category: Category::Mod,
        // No fix: same reasoning as MOD004.
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "ORDER BY in a view body without LIMIT: the order is not part of \
                   the view's contract.",
    },
    Rule {
        code: "MOD013",
        name: "order-by-all",
        category: Category::Mod,
        // Safe: fires only when the list equals the select list position by
        // position, with no DESC or NULLS modifier.
        default_severity: Severity::Info,
        fix_safety: FixSafety::Safe,
        message: "Explicit ORDER BY list equals the whole select list; \
                   ORDER BY ALL says the same.",
    },
    Rule {
        code: "MOD014",
        name: "case-to-filter",
        category: Category::Mod,
        default_severity: Severity::Info,
        // Unsafe: the rewrite drops the CASE's ELSE value. With a non-NULL
        // ELSE the result changes: `sum(CASE WHEN c THEN x ELSE 0 END)` is 0
        // where `sum(x) FILTER (WHERE c)` is NULL when no row matches, and
        // `count(CASE ... ELSE 0 END)` counts every row, not just matches.
        fix_safety: FixSafety::Unsafe,
        message: "agg(CASE WHEN c THEN x END) can be agg(x) FILTER (WHERE c).",
    },
    Rule {
        code: "MOD015",
        name: "case-to-switch",
        category: Category::Mod,
        // Safe: DuckDB defines the switch form as this same equality chain.
        default_severity: Severity::Info,
        fix_safety: FixSafety::Safe,
        message: "Same-subject CASE WHEN chain can be the CASE <expr> WHEN form.",
    },
    Rule {
        code: "MOD016",
        name: "unused-cte",
        category: Category::Mod,
        // Warning: a CTE nothing uses is dead code or a reference that was
        // meant to use it. Safe: deleting it cannot change results.
        default_severity: Severity::Warning,
        fix_safety: FixSafety::Safe,
        message: "CTE is defined but never referenced.",
    },
    Rule {
        code: "MOD021",
        name: "qualify-rewrite",
        category: Category::Mod,
        // No fix: folding the wrapper into QUALIFY moves whole clauses
        // between query levels, beyond a byte-range edit.
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "The subquery/CTE exists only to filter on a window alias; \
                   QUALIFY does it in place.",
    },
    // --- MOD, opt-in: idiom noise or house style ---
    // MOD017: `CREATE TABLE x AS SELECT * FROM 'f.csv'` is the idiomatic
    //   DuckDB load pattern. MOD018: the perf premise is real, but key
    //   constraints in DDL are pervasive in valid SQL. MOD019: the payoff is
    //   unmeasured, so the advice stays speculative. MOD020, MOD022-023: pure
    //   style. MOD025: `*` is idiomatic for exploration; opt in where queries
    //   should name their columns.
    // The report-only rows (MOD017-019, MOD025) have no mechanical fix:
    // naming columns, dropping constraints or building a VALUES join needs
    // the schema or the author's intent.
    Rule {
        code: "MOD017",
        name: "select-star-ctas",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::None,
        message: "SELECT * into CTAS/INSERT materializes every column. Opt-in.",
    },
    Rule {
        code: "MOD018",
        name: "constraint-load-cost",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::None,
        message: "PK/UNIQUE/FK constraints slow bulk loads 2-4x and do not speed \
                   queries (perf guide). Opt-in.",
    },
    Rule {
        code: "MOD019",
        name: "large-in-list",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::None,
        message: "Large literal IN list; consider JOIN against VALUES. Opt-in.",
    },
    Rule {
        code: "MOD020",
        name: "plain-big-number",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::None,
        message: "Long numeric literal without underscores (1_000_000). Opt-in.",
    },
    Rule {
        code: "MOD022",
        name: "from-first",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::Safe,
        message: "SELECT * FROM t can be FROM-first: FROM t. Opt-in house style.",
    },
    Rule {
        code: "MOD023",
        name: "prefix-alias",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::Safe,
        message: "expr AS x can be the prefix form x: expr. Opt-in house style.",
    },
    Rule {
        code: "MOD025",
        name: "select-star",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::None,
        message: "Bare * in the outermost select list. Opt-in.",
    },
    // --- tier 4: rows coming back wrong or in whatever order the engine
    // felt like, with nothing in the query's own text saying that was
    // expected. Detect-only throughout -- see crate::tier4's module doc.
    Rule {
        code: "MOD026",
        name: "limit-no-orderby",
        category: Category::Mod,
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "LIMIT/OFFSET with no ORDER BY: which rows come back is not guaranteed.",
    },
    Rule {
        code: "MOD027",
        name: "window-no-orderby",
        category: Category::Mod,
        default_severity: Severity::Warning,
        fix_safety: FixSafety::None,
        message: "row_number()/rank()/lag() etc. with no ORDER BY in OVER: the partition's \
                   row order is not guaranteed.",
    },
    Rule {
        code: "MOD028",
        name: "not-in-subquery",
        category: Category::Mod,
        default_severity: Severity::Warning,
        fix_safety: FixSafety::None,
        message: "NOT IN (subquery): a single NULL in the subquery's result makes this \
                   match zero rows, silently. Consider NOT EXISTS.",
    },
    Rule {
        code: "MOD029",
        name: "filter-defeats-outer-join",
        category: Category::Mod,
        default_severity: Severity::Warning,
        fix_safety: FixSafety::None,
        message: "WHERE filters on the LEFT JOIN's right side, which drops its unmatched \
                   rows -- the join is acting as an INNER JOIN here.",
    },
    Rule {
        code: "MOD030",
        name: "sum-case-to-count-if",
        category: Category::Mod,
        default_severity: Severity::Info,
        // Safe, unlike MOD014 whose shape this is carved out of: executed
        // against DuckDB, `count_if(c)` matches `sum(CASE WHEN c THEN 1 ELSE
        // 0 END)` with c NULL, with no input rows (both NULL), with rows but
        // no match (both 0), and in type (both HUGEINT). The literals must be
        // exactly 1 and 0: `1.0` would make the sum a DECIMAL.
        fix_safety: FixSafety::Safe,
        message: "sum(CASE WHEN c THEN 1 ELSE 0 END) is count_if(c).",
    },
    Rule {
        code: "MOD031",
        name: "case-to-function",
        category: Category::Mod,
        // Opt-in: the rewrite is proven equivalent, but no SQL written by
        // others has shown how often the shape occurs, so there is no basis
        // yet for turning it on everywhere.
        default_severity: Severity::Off,
        fix_safety: FixSafety::Safe,
        message: "This CASE is coalesce(...) or nullif(...); the function says it directly.",
    },
    Rule {
        code: "MOD032",
        name: "not-in-null",
        category: Category::Mod,
        // Default-on despite no corpus evidence: every finding is a certain
        // bug (the test is never true), so there is no noise to measure.
        default_severity: Severity::Warning,
        fix_safety: FixSafety::None,
        message: "NOT IN list contains NULL: the test is never true, so this keeps no rows.",
    },
    Rule {
        code: "MOD033",
        name: "having-without-aggregate",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::None,
        message: "HAVING uses no aggregate: as a WHERE condition it filters before grouping.",
    },
    Rule {
        code: "MOD034",
        name: "redundant-count-default",
        category: Category::Mod,
        default_severity: Severity::Off,
        fix_safety: FixSafety::Safe,
        message: "count(...) is never NULL; the coalesce/ifnull default does nothing.",
    },
    Rule {
        code: "MOD035",
        name: "wrapped-date-filter",
        category: Category::Mod,
        // Default-on: the cost is measured (7-74x on a 20M-row table, every
        // row group read) and the range spelling is never slower, so a
        // finding is worth acting on even where the table is small today.
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "Filtering on year()/strftime()/a DATE cast of a column reads every row \
                   group; compare the column to a range (ts >= '2023-01-01' AND ts < \
                   '2024-01-01') so DuckDB can skip what it doesn't need.",
    },
    Rule {
        code: "MOD036",
        name: "row-at-a-time-insert",
        category: Category::Mod,
        // Default-on: measured at 50x (one multi-row INSERT) to 1,800x
        // (INSERT ... SELECT), and the threshold keeps it silent on anything
        // shorter than a load script.
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "10+ single-row INSERTs into one table in a row: load them as one multi-row \
                   INSERT, or INSERT ... SELECT FROM read_csv(...), instead.",
    },
    Rule {
        code: "MOD037",
        name: "order-by-random-sample",
        category: Category::Mod,
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "ORDER BY random() LIMIT n orders every row to keep n; USING SAMPLE n ROWS \
                   draws the same uniform sample about 4x faster.",
    },
    Rule {
        code: "MOD038",
        name: "sample-before-where",
        category: Category::Mod,
        // Warning: the query returns far fewer rows than it asks for.
        default_severity: Severity::Warning,
        fix_safety: FixSafety::None,
        message: "USING SAMPLE n [ROWS] samples before WHERE filters, so far fewer than n \
                   rows come back; filter in a subquery and sample its result.",
    },
    Rule {
        code: "MOD039",
        name: "delete-then-insert",
        category: Category::Mod,
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "DELETE then INSERT into the same table is an upsert in two passes; MERGE \
                   INTO does it in one atomic statement, about 2x faster. Outside a \
                   transaction, a failed INSERT leaves the deleted rows gone.",
    },
    Rule {
        code: "MOD040",
        name: "csv-full-sniff",
        category: Category::Mod,
        default_severity: Severity::Info,
        fix_safety: FixSafety::None,
        message: "sample_size = -1 reads the whole CSV to guess its types, 15x slower on 5M \
                   rows; declare them with columns = {...} or types = {...} instead.",
    },
];

/// Looks up a rule by its code, e.g. `lookup("MOD001")`.
///
/// A linear scan: over a table of a few dozen rows it costs nothing
/// measurable, and a precomputed map would buy nothing in return.
#[must_use]
pub fn lookup(code: &str) -> Option<&'static Rule> {
    RULES.iter().find(|rule| rule.code == code)
}

/// The reserved selection value meaning "every `MOD` rule, default-on and
/// opt-in alike" -- the strictness switch for someone who wants every
/// opinion grebe has, not just the ones that ship on.
pub const SELECT_ALL: &str = "ALL";

/// Expands a `--select` / `select` / `grebe.select` list: a sole `ALL`
/// entry (any case) becomes every `MOD` code, in registry order; anything
/// else passes through untouched. `ALL` is never a real code, so this runs
/// before a caller validates codes against [`lookup`].
///
/// `PRS` and `SRC` codes are deliberately left out of the expansion: a
/// selection only ever restricts which `MOD` rules run (`Selection::enabled`
/// in `analysis.rs`), and `ALL` keeps that meaning -- it does not also
/// reach into `[severity]` to turn PRS/SRC back on.
#[must_use]
pub fn expand_select(codes: Vec<String>) -> Vec<String> {
    if codes.len() == 1 && codes[0].eq_ignore_ascii_case(SELECT_ALL) {
        return RULES
            .iter()
            .filter(|r| r.category == Category::Mod)
            .map(|r| r.code.to_string())
            .collect();
    }
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code is unique — a duplicate would silently shadow a rule in
    /// [`lookup`] and no test besides this one would notice.
    #[test]
    fn codes_are_unique() {
        for (i, a) in RULES.iter().enumerate() {
            for b in &RULES[i + 1..] {
                assert_ne!(a.code, b.code, "duplicate rule code {}", a.code);
            }
        }
    }

    /// Every name is unique too — `name` is the human-facing spelling, and a
    /// duplicate would make two rules indistinguishable in output.
    #[test]
    fn names_are_unique() {
        for (i, a) in RULES.iter().enumerate() {
            for b in &RULES[i + 1..] {
                assert_ne!(a.name, b.name, "duplicate rule name {}", a.name);
            }
        }
    }

    /// `^(PRS|SRC|MOD)\d{3}$`, spelled out by hand since this crate has no
    /// regex dependency to spell it with (and none should be added just for
    /// a test).
    #[test]
    fn codes_match_shape() {
        for rule in RULES {
            let bytes = rule.code.as_bytes();
            assert_eq!(bytes.len(), 6, "{} is not 6 bytes", rule.code);
            let prefix = &rule.code[..3];
            assert!(
                prefix == "PRS" || prefix == "SRC" || prefix == "MOD",
                "{} has an unrecognized prefix",
                rule.code
            );
            for &b in &bytes[3..] {
                assert!(b.is_ascii_digit(), "{} has a non-digit suffix", rule.code);
            }
        }
    }

    /// The code's prefix always agrees with its `category` field — nobody
    /// hand-editing a row should be able to make these disagree unnoticed.
    #[test]
    fn code_prefix_matches_category() {
        for rule in RULES {
            let expected = match rule.category {
                Category::Prs => "PRS",
                Category::Src => "SRC",
                Category::Mod => "MOD",
            };
            assert_eq!(&rule.code[..3], expected, "{} category mismatch", rule.code);
        }
    }

    /// Every name is kebab-case: lowercase ASCII letters and digits,
    /// hyphen-separated, no leading/trailing/doubled hyphen.
    #[test]
    fn names_are_kebab_case() {
        for rule in RULES {
            assert!(!rule.name.is_empty(), "{} has an empty name", rule.code);
            assert!(
                !rule.name.starts_with('-') && !rule.name.ends_with('-'),
                "{} name has a leading/trailing hyphen",
                rule.code
            );
            assert!(
                !rule.name.contains("--"),
                "{} name has a doubled hyphen",
                rule.code
            );
            for c in rule.name.chars() {
                assert!(
                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-',
                    "{} name has a non-kebab-case character {:?}",
                    rule.code,
                    c
                );
            }
        }
    }

    /// [`lookup`] round-trips for every row in the table.
    #[test]
    fn lookup_round_trips() {
        for rule in RULES {
            let found = lookup(rule.code).unwrap_or_else(|| panic!("{} not found", rule.code));
            assert_eq!(found.code, rule.code);
            assert_eq!(found.name, rule.name);
        }
    }

    /// An unregistered code looks up to nothing rather than panicking —
    /// including `SEM001`, `SRC001` and `PRS900`, which only a binding tool or
    /// a second parse engine would need.
    #[test]
    fn lookup_missing_is_none() {
        assert!(lookup("MOD999").is_none());
        assert!(lookup("").is_none());
        assert!(lookup("SEM001").is_none(), "SEM001 needs binding");
        assert!(lookup("SRC001").is_none(), "SRC001 needs binding");
        assert!(
            lookup("PRS900").is_none(),
            "PRS900 has no PEG-matcher analog"
        );
    }

    /// Category counts: 1 PRS, 3 SRC, 29 MOD (MOD001-029, no gaps).
    #[test]
    fn category_counts() {
        let prs = RULES.iter().filter(|r| r.category == Category::Prs).count();
        let src = RULES.iter().filter(|r| r.category == Category::Src).count();
        let mods = RULES.iter().filter(|r| r.category == Category::Mod).count();
        assert_eq!(prs, 1);
        assert_eq!(src, 2);
        assert_eq!(mods, 39);
        assert_eq!(RULES.len(), 42);
    }

    /// The opt-in band defaults to `Off`: MOD010, MOD017-020, MOD022-023, MOD025,
    /// MOD031, MOD033-034 — 11 rows.
    #[test]
    fn opt_in_band_defaults_off() {
        let off_codes = [
            "MOD010", "MOD017", "MOD018", "MOD019", "MOD020", "MOD022", "MOD023", "MOD025",
            "MOD031", "MOD033", "MOD034",
        ];
        for code in off_codes {
            let rule = lookup(code).unwrap_or_else(|| panic!("{code} missing"));
            assert_eq!(
                rule.default_severity,
                Severity::Off,
                "{code} should default to Off"
            );
        }
        let off_count = RULES
            .iter()
            .filter(|r| r.default_severity == Severity::Off)
            .count();
        assert_eq!(off_count, off_codes.len());
    }

    /// Every `Mod` rule not in the opt-in band above is default-on
    /// (`Error`/`Warning`/`Info`, never `Off`) — MOD001-009, 011-016, 021,
    /// 026-030, 032, 035-040.
    #[test]
    fn default_on_band_is_not_off() {
        let off_codes = [
            "MOD010", "MOD017", "MOD018", "MOD019", "MOD020", "MOD022", "MOD023", "MOD025",
            "MOD031", "MOD033", "MOD034",
        ];
        for rule in RULES.iter().filter(|r| r.category == Category::Mod) {
            if off_codes.contains(&rule.code) {
                continue;
            }
            assert_ne!(
                rule.default_severity,
                Severity::Off,
                "{} should be default-on",
                rule.code
            );
        }
    }

    /// PRS001 is the one rule severity `Error` — everything else in the
    /// set is advisory (`Info`/`Warning`/`Off`).
    #[test]
    fn only_prs001_is_error() {
        for rule in RULES {
            if rule.code == "PRS001" {
                assert_eq!(rule.default_severity, Severity::Error);
            } else {
                assert_ne!(
                    rule.default_severity,
                    Severity::Error,
                    "{} unexpectedly defaults to Error",
                    rule.code
                );
            }
        }
    }

    /// `ALL` expands to every `MOD` code, PRS/SRC excluded, in registry
    /// order — the order `--select`'s own output and `grebe rules` share.
    #[test]
    fn select_all_expands_to_every_mod_code_in_registry_order() {
        let expanded = expand_select(vec!["ALL".to_string()]);
        let want: Vec<&str> = RULES
            .iter()
            .filter(|r| r.category == Category::Mod)
            .map(|r| r.code)
            .collect();
        assert_eq!(expanded, want);
        assert!(!expanded.is_empty());
        for code in &expanded {
            assert!(lookup(code).is_some(), "{code} not in the registry");
        }
    }

    /// Case-insensitive, and only when `ALL` is the sole entry — mixed with
    /// another code it is not a real rule, so expansion leaves it alone and
    /// the caller's own validation reports it as unknown.
    #[test]
    fn select_all_is_case_insensitive_but_not_combinable() {
        assert_eq!(
            expand_select(vec!["all".to_string()]),
            expand_select(vec!["ALL".to_string()])
        );
        let mixed = expand_select(vec!["ALL".to_string(), "MOD001".to_string()]);
        assert_eq!(mixed, vec!["ALL".to_string(), "MOD001".to_string()]);
    }

    /// An ordinary selection is untouched.
    #[test]
    fn expand_select_passes_through_an_ordinary_list() {
        let codes = vec!["MOD002".to_string(), "MOD010".to_string()];
        assert_eq!(expand_select(codes.clone()), codes);
    }
}
