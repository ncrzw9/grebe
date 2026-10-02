#!/usr/bin/env node
// Generates syntaxes/duckdb-keywords.injection.json — a TextMate injection
// grammar that highlights DuckDB keywords VS Code's built-in ANSI-ish SQL
// grammar doesn't know (DESCRIBE, SUMMARIZE, PIVOT, UNPIVOT, QUALIFY, ...).
//
// Source of truth:
// crates/grebe-syntax/vendor/grammar/keywords/reserved_keyword.list — the
// DuckDB grammar's own reserved-word list, vendored verbatim elsewhere in
// this repo for the parser. Deliberately RESERVED KEYWORDS ONLY, not the
// other four keyword lists:
//
//   - A reserved keyword can never be a DuckDB identifier (that's what
//     "reserved" means to the grammar), so painting one as a keyword is
//     always correct, everywhere it appears.
//   - unreserved_keyword.list (339 entries) is full of words that are
//     perfectly legal column/table names in DuckDB -- NAME, TYPE, VALUE,
//     KEY, and friends. Injecting those would highlight ordinary
//     identifiers as if they were keywords, which is worse than leaving a
//     keyword uncoloured: an uncoloured keyword still reads as fine, a
//     keyword-coloured identifier reads as broken.
//   - column_name_keyword.list, func_name_keyword.list and
//     type_name_keyword.list are narrower carve-outs of the same shape
//     (words DuckDB lets you use as identifiers in specific positions),
//     so the same argument excludes them too.
//
// This deliberately does NOT cover every DuckDB keyword a user might
// notice missing -- ASOF, SEMI, ANTI, MACRO and POSITIONAL are all
// unreserved and so stay out on purpose (see editors/vscode/README.md).
// DESCRIBE, SUMMARIZE, PIVOT, UNPIVOT and QUALIFY, the DuckDB words most
// often missed by the built-in grammar, are all reserved, so they are covered.
//
// No npm dependency: this is a plain Node script (Node is already required
// to package the .vsix with @vscode/vsce), run by hand whenever the
// keyword list changes. Its only output is the JSON file below; it is never
// loaded by the extension itself at runtime.
//
// Regenerate with: node build-grammar.js

const fs = require("fs");
const path = require("path");

const REPO_ROOT = path.join(__dirname, "..", "..");
const SOURCE_LIST = path.join(
  REPO_ROOT,
  "crates",
  "grebe-syntax",
  "vendor",
  "grammar",
  "keywords",
  "reserved_keyword.list",
);
const OUT_FILE = path.join(
  __dirname,
  "syntaxes",
  "duckdb-keywords.injection.json",
);

function escapeRegex(word) {
  return word.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function main() {
  const raw = fs.readFileSync(SOURCE_LIST, "utf8");
  const keywords = Array.from(
    new Set(
      raw
        .split(/\r?\n/)
        .map((line) => line.trim())
        .filter((line) => line.length > 0),
    ),
  );

  if (keywords.length === 0) {
    throw new Error(`no keywords read from ${SOURCE_LIST}`);
  }

  // Longest first (then alphabetical, for a stable, reviewable diff) so a
  // shorter keyword that is a prefix of a longer one -- e.g. PIVOT next to
  // PIVOT_WIDER -- can never win the alternation before the longer form
  // gets a chance to match.
  keywords.sort((a, b) => b.length - a.length || a.localeCompare(b));

  const alternation = keywords.map(escapeRegex).join("|");
  const match = `(?i)\\b(${alternation})\\b`;

  const grammar = {
    generated: {
      note: "GENERATED FILE -- do not hand-edit.",
      by: "editors/vscode/build-grammar.js",
      from: "crates/grebe-syntax/vendor/grammar/keywords/reserved_keyword.list",
      regenerate: "node editors/vscode/build-grammar.js",
    },
    scopeName: "source.duckdb.keywords",
    injectionSelector: "L:source.sql",
    patterns: [
      {
        name: "keyword.other.duckdb",
        match,
      },
    ],
  };

  fs.mkdirSync(path.dirname(OUT_FILE), { recursive: true });
  fs.writeFileSync(OUT_FILE, `${JSON.stringify(grammar, null, 2)}\n`);
  console.log(
    `wrote ${path.relative(REPO_ROOT, OUT_FILE)} (${keywords.length} reserved keywords)`,
  );
}

main();
