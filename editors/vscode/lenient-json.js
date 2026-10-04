// A JSON reader for the duckdb CLI's `-json` output, which JSON.parse cannot
// be trusted with:
//
//   - `NaN`, `Infinity` and `-Infinity` are emitted bare, and JSON.parse
//     rejects the whole document over one of them;
//   - BIGINT is a bare number, so 9007199254740993 reads back as ...992;
//   - two columns with the same name become duplicate keys, and JSON.parse
//     keeps only the last;
//   - an object's key order is the column order, and JSON.parse reorders
//     integer-like keys ("1", "2") ahead of the rest.
//
// So objects come back as arrays of [key, value] pairs, in order, duplicates
// kept; a number that would lose precision as a double comes back as
// `{ int: "<digits>" }`, so it stays exact and still reads as a number rather
// than as a VARCHAR that happens to hold digits. Nothing here is
// DuckDB-specific beyond those choices.

"use strict";

class LenientJsonError extends Error {}

/** Parse `text`; objects become `{ pairs: [[key, value], ...] }`, unsafe
 *  integers `{ int: "<digits>" }`. */
function parse(text) {
  let i = 0;

  const fail = (what) => {
    throw new LenientJsonError(`${what} at offset ${i}`);
  };
  const ws = () => {
    while (i < text.length && " \t\r\n".includes(text[i])) i++;
  };
  const lit = (word, value) => {
    if (text.startsWith(word, i)) {
      i += word.length;
      return value;
    }
    return undefined;
  };

  function value() {
    ws();
    const c = text[i];
    if (c === "{") return object();
    if (c === "[") return array();
    if (c === '"') return string();
    for (const [word, v] of [
      ["null", null],
      ["true", true],
      ["false", false],
      ["NaN", NaN],
      ["Infinity", Infinity],
      ["-Infinity", -Infinity],
    ]) {
      const got = lit(word, v);
      if (got !== undefined) return got;
    }
    return number();
  }

  function object() {
    i++; // {
    const pairs = [];
    ws();
    if (text[i] === "}") {
      i++;
      return { pairs };
    }
    for (;;) {
      ws();
      if (text[i] !== '"') fail("expected a key");
      const key = string();
      ws();
      if (text[i] !== ":") fail("expected ':'");
      i++;
      pairs.push([key, value()]);
      ws();
      if (text[i] === ",") {
        i++;
        continue;
      }
      if (text[i] === "}") {
        i++;
        return { pairs };
      }
      fail("expected ',' or '}'");
    }
  }

  function array() {
    i++; // [
    const items = [];
    ws();
    if (text[i] === "]") {
      i++;
      return items;
    }
    for (;;) {
      items.push(value());
      ws();
      if (text[i] === ",") {
        i++;
        continue;
      }
      if (text[i] === "]") {
        i++;
        return items;
      }
      fail("expected ',' or ']'");
    }
  }

  function string() {
    i++; // opening quote
    let out = "";
    for (;;) {
      if (i >= text.length) fail("unterminated string");
      const c = text[i++];
      if (c === '"') return out;
      if (c !== "\\") {
        out += c;
        continue;
      }
      const e = text[i++];
      if (e === "u") {
        const hex = text.slice(i, i + 4);
        if (!/^[0-9a-fA-F]{4}$/.test(hex)) fail("bad \\u escape");
        out += String.fromCharCode(parseInt(hex, 16));
        i += 4;
      } else {
        const map = { '"': '"', "\\": "\\", "/": "/", b: "\b", f: "\f", n: "\n", r: "\r", t: "\t" };
        if (!(e in map)) fail("bad escape");
        out += map[e];
      }
    }
  }

  function number() {
    const m = /^-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?/.exec(text.slice(i));
    if (!m) fail("unexpected character");
    i += m[0].length;
    const n = Number(m[0]);
    // An integer the double cannot hold exactly keeps its digits.
    if (!m[2] && !m[3] && !Number.isSafeInteger(n)) return { int: m[0] };
    return n;
  }

  const v = value();
  ws();
  if (i !== text.length) fail("trailing text");
  return v;
}

/**
 * Turn a `-json` result array into `{ columns, rows }`, where each row is an
 * array of values in column order. Columns come from the first row: every
 * row of one result has the same keys in the same order.
 */
function toTable(parsed) {
  if (!Array.isArray(parsed)) throw new LenientJsonError("expected an array of rows");
  if (parsed.length === 0) return { columns: [], rows: [] };
  const columns = parsed[0].pairs.map(([k]) => k);
  const rows = parsed.map((r) => r.pairs.map(([, v]) => v));
  return { columns, rows };
}

/**
 * `-json` output to `{ columns, rows }`, fast. JSON.parse is 8-20x quicker
 * than the reader above, and correct unless the text has one of the four
 * things it mishandles (see the top of this file). Those are cheap to rule
 * out: bare NaN/Infinity and 16+ digit integers by pattern, duplicate
 * column names from the first row alone -- the CLI puts one row per line
 * and JSON escapes newlines, so the first line is exactly the first row.
 * Anything that fails a check, or matches by accident (the patterns can
 * match inside a string), takes the careful path.
 */
function readTable(text) {
  if (!/NaN|Infinity/.test(text) && !/[[:,]\s*-?\d{16,}/.test(text)) {
    const nl = text.indexOf("\n");
    const firstLine = (nl < 0 ? text : text.slice(0, nl)).replace(/^\[/, "").replace(/[,\]]\s*$/, "");
    const columns = firstLine === "" ? [] : parse(firstLine).pairs.map(([k]) => k);
    if (new Set(columns).size === columns.length) {
      const objects = JSON.parse(text);
      return { columns, rows: objects.map((o) => columns.map((c) => o[c])) };
    }
  }
  return toTable(parse(text));
}

/** One value as display text; `null` stays null (the grid shows NULL). */
function text(v) {
  if (v === null || v === undefined) return null;
  if (typeof v === "number") {
    if (Number.isNaN(v)) return "NaN";
    if (!Number.isFinite(v)) return v > 0 ? "inf" : "-inf";
    return String(v);
  }
  if (typeof v !== "object") return String(v);
  if ("int" in v && Object.keys(v).length === 1) return v.int;
  const inner = (x) => (x === null || x === undefined ? "NULL" : text(x));
  if (Array.isArray(v)) return `[${v.map(inner).join(", ")}]`;
  const pairs = Array.isArray(v.pairs) ? v.pairs : Object.entries(v);
  return `{${pairs.map(([k, x]) => `${k}: ${inner(x)}`).join(", ")}}`;
}

const isNumeric = (v) => typeof v === "number" || (v !== null && typeof v === "object" && "int" in v && Object.keys(v).length === 1);

/**
 * What the results panel receives: column-major display text, `null` for
 * NULL, capped at `max` rows. Column-major because it crosses postMessage,
 * which serializes: a few long arrays of strings clone far faster than one
 * small object per cell. `num` right-aligns a column whose values are
 * numbers.
 */
function columnar({ columns, rows }, max) {
  const shown = Math.min(rows.length, max);
  const data = columns.map(() => new Array(shown));
  const num = columns.map(() => false);
  for (let r = 0; r < shown; r++) {
    const row = rows[r];
    for (let c = 0; c < columns.length; c++) {
      const v = row[c];
      if (!num[c] && isNumeric(v)) num[c] = true;
      data[c][r] = text(v);
    }
  }
  return { columns, num, data, total: rows.length };
}

module.exports = { parse, toTable, readTable, columnar, text, LenientJsonError };
