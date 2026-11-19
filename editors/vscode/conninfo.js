// Database connection strings: what to show of one, and the password never.
// No `vscode` import, so it can be tested with plain Node.
//
// DuckDB keeps an attached PostgreSQL (or MySQL) database's connection string
// as its `path` in duckdb_databases(), password and all, and repeats it in
// its errors ("Unable to connect to Postgres at "... password=...""). Anything
// the extension shows or logs goes through redact() first.

"use strict";

const path = require("path");

// A libpq/MySQL `password=` (or `passwd=`) value: quoted as libpq quotes it,
// quoted again inside a SQL string literal (''...''), or bare up to a space
// or quote.
const KEYWORD_PASSWORD = /\b(pass(?:word|wd)\s*=\s*)(''(?:[^']|'{4})*''|'(?:[^'\\]|\\.)*'|[^\s'"]+)/gi;
// The password in a URI's user info: scheme://user:password@host.
const URI_PASSWORD = /(\b[a-z][a-z0-9+.-]*:\/\/[^/\s:@'"]*:)[^@\s'"/]*@/gi;
// Same in a ?password=... query parameter.
const QUERY_PASSWORD = /([?&]password=)[^&\s'"]*/gi;

/** `text` with every password in a connection string replaced by ***. */
function redact(text) {
  if (text === null || text === undefined) return text;
  return String(text)
    .replace(URI_PASSWORD, "$1***@")
    .replace(QUERY_PASSWORD, "$1***")
    .replace(KEYWORD_PASSWORD, "$1***");
}

/** Whether `s` is a connection string rather than a file path. */
function isConnection(s) {
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(s) || /(^|\s)\w+\s*=/.test(s);
}

/** libpq keyword=value pairs (values may be '...'-quoted), or the parts of a
 *  postgres:// URI, as { dbname, host, port, user, password }. */
function parse(s) {
  const out = {};
  const uri = /^[a-z][a-z0-9+.-]*:\/\/(?:([^:@/]*)(?::([^@/]*))?@)?([^/?:]*)(?::(\d+))?(?:\/([^?]*))?(?:\?(.*))?$/i.exec(s.trim());
  if (uri) {
    const dec = (v) => (v === undefined || v === "" ? undefined : decodeURIComponent(v));
    Object.assign(out, { user: dec(uri[1]), password: dec(uri[2]), host: dec(uri[3]), port: uri[4], dbname: dec(uri[5]) });
    for (const kv of (uri[6] || "").split("&")) {
      const [k, v] = kv.split("=");
      if (k && v !== undefined) out[k] = dec(v);
    }
  } else {
    const re = /(\w+)\s*=\s*('(?:[^'\\]|\\.)*'|\S*)/g;
    let m;
    while ((m = re.exec(s))) {
      out[m[1]] = m[2].startsWith("'") ? m[2].slice(1, -1).replace(/\\(.)/g, "$1") : m[2];
    }
  }
  for (const k of Object.keys(out)) if (out[k] === undefined) delete out[k];
  return out;
}

/** Where an attached database lives, in a few words and without secrets:
 *  `ub@localhost:5432` for a connection string, the file name for a file. */
function where(p) {
  if (!p) return "";
  if (!isConnection(p)) return path.basename(p);
  const c = parse(p);
  const host = c.host || c.hostaddr || "";
  const at = host ? `${host}${c.port ? `:${c.port}` : ""}` : "";
  const db = c.dbname || c.database || c.db || "";
  return db && at ? `${db}@${at}` : db || at || "connection";
}

/** A SQL-friendly alias for a connection: its database name, else "pg". */
function aliasFor(s) {
  const c = parse(s);
  return c.dbname || c.database || "pg";
}

/** `s` with a password added, in the style it is written in. */
function withPassword(s, password) {
  const t = s.trim();
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(t)) return `${t}${t.includes("?") ? "&" : "?"}password=${encodeURIComponent(password)}`;
  return `${t} password='${password.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'`;
}

/** `s` with no password in it, to keep: the keyword, the URI's user info
 *  and the query parameter are each removed whole. A file path is returned
 *  as it is. */
function withoutPassword(s) {
  if (s === null || s === undefined || !isConnection(String(s))) return s;
  return String(s)
    .replace(URI_PASSWORD, (m, head) => `${head.slice(0, -1)}@`)
    .replace(/([?&])password=[^&\s'"]*&?/gi, "$1")
    .replace(/[?&]$/, "")
    .replace(KEYWORD_PASSWORD, "")
    .replace(/\s{2,}/g, " ")
    .trim();
}

/** `s` with libpq's connect_timeout set to `seconds`, unless it sets one:
 *  a server that does not answer is then given up on by the client, and
 *  the DuckDB session stays usable. */
function withConnectTimeout(s, seconds) {
  const t = String(s).trim();
  if (/\bconnect_timeout\s*=/i.test(t)) return t;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(t)) return `${t}${t.includes("?") ? "&" : "?"}connect_timeout=${seconds}`;
  return `${t} connect_timeout=${seconds}`;
}

/** `s` as it is kept and compared: no password, and no connect_timeout,
 *  which says nothing about where it is. A file path is returned as it is. */
function plain(s) {
  const t = withoutPassword(s);
  if (t === null || t === undefined || !isConnection(String(t))) return t;
  return String(t)
    .replace(/([?&])connect_timeout=[^&\s'"]*&?/gi, "$1")
    .replace(/[?&]$/, "")
    .replace(/(^|\s)connect_timeout\s*=\s*\S*/gi, "")
    .replace(/\s{2,}/g, " ")
    .trim();
}

module.exports = { redact, isConnection, parse, where, aliasFor, withPassword, withoutPassword, withConnectTimeout, plain };
