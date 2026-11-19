// The connections the `grebe.duckdb.connections` setting names, as plain
// data: reading the setting from its scopes, choosing the scope to write to,
// what a change to it asks of a running session, and which connections a
// statement refers to. No `vscode` import, so it can be tested with plain
// Node.
//
// An entry is `{ alias, path, type?, readOnly?, detached?, session? }`.
// `session` names a `-- grebe: session NAME` session; without it the entry
// belongs to the shared one. Passwords are never part of an entry.

"use strict";

const path = require("path");
const conninfo = require("./conninfo");

const DEFAULT = "default";

// Types that are servers even when the path does not look like a
// connection string.
const SERVER_TYPES = new Set(["postgres", "postgresql", "mysql"]);

// The scopes a setting can be in, most specific first: VS Code's own order
// of precedence.
const SCOPES = ["workspaceFolder", "workspace", "global"];
const VALUE_OF = { workspaceFolder: "workspaceFolderValue", workspace: "workspaceValue", global: "globalValue" };

/** The type a path is attached as: the one given, else what a URI's scheme
 *  says, else a DuckDB file. */
function typeOf(p, type) {
  if (typeof type === "string" && type) return type.toLowerCase();
  const scheme = /^([a-z][a-z0-9+.-]*):\/\//i.exec(String(p));
  if (scheme && /^postgres(ql)?$/i.test(scheme[1])) return "postgres";
  if (scheme && /^mysql$/i.test(scheme[1])) return "mysql";
  return "duckdb";
}

/** Whether `e` is on a server (attached when first needed) rather than a
 *  local file (attached when the session starts). */
function isServer(e) {
  return conninfo.isConnection(e.path) || SERVER_TYPES.has(e.type);
}

/** `raw` (one scope's value) as entries: well-formed ones only, a password
 *  dropped, a relative file made absolute against `folder`. */
function normalize(raw, folder) {
  if (!Array.isArray(raw)) return [];
  const out = [];
  for (const c of raw) {
    if (!c || typeof c.alias !== "string" || typeof c.path !== "string" || !c.alias || !c.path) continue;
    let target = conninfo.withoutPassword(c.path.trim());
    if (!conninfo.isConnection(target) && !path.isAbsolute(target) && folder) target = path.join(folder, target);
    const e = { alias: c.alias, path: target, type: typeOf(target, c.type), readOnly: c.readOnly === true };
    if (c.detached === true) e.detached = true;
    if (typeof c.session === "string" && c.session && c.session !== DEFAULT) e.session = c.session;
    out.push(e);
  }
  return out;
}

/** The session an entry belongs to. */
const sessionOf = (e) => e.session || DEFAULT;

/** What tells two entries apart: their session and their alias (DuckDB does
 *  not tell upper case from lower in a name). */
const keyOf = (e) => `${sessionOf(e)}\u0000${String(e.alias).toLowerCase()}`;

/** Whether two entries attach the same thing the same way. */
const sameTarget = (a, b) => a.path === b.path && a.type === b.type && Boolean(a.readOnly) === Boolean(b.readOnly);

/**
 * Every entry of every scope, as one list. `info` is what VS Code's
 * `inspect()` gives for the setting. For the same session and alias the
 * most specific scope wins. Each entry says which scope it is from
 * (`scope`).
 */
function merge(info, folder) {
  const out = new Map();
  for (const scope of SCOPES) {
    for (const e of normalize(info && info[VALUE_OF[scope]], folder)) {
      const k = keyOf(e);
      if (!out.has(k)) out.set(k, { ...e, scope });
    }
  }
  return [...out.values()];
}

/** The scope to add an entry to: the one that already has the setting when
 *  exactly one does, else the user's settings. */
function chooseScope(info) {
  const have = SCOPES.filter((s) => info && Array.isArray(info[VALUE_OF[s]]));
  return have.length === 1 ? have[0] : "global";
}

/** The scopes whose value has an entry for `session` and `alias`. */
function scopesWith(info, session, alias) {
  const k = keyOf({ session, alias });
  return SCOPES.filter((s) => normalize(info && info[VALUE_OF[s]]).some((e) => keyOf(e) === k));
}

/** An entry as it is written to the setting: what is not the default is
 *  left out. `rel`, when given, is the folder a file inside it is written
 *  relative to. */
function toSetting(e, rel) {
  let p = conninfo.withoutPassword(e.path);
  if (rel && !conninfo.isConnection(p) && path.isAbsolute(p)) {
    const r = path.relative(rel, p);
    if (r && !r.startsWith("..") && !path.isAbsolute(r)) p = r.split(path.sep).join("/");
  }
  const out = { alias: e.alias, path: p };
  if (e.type && e.type !== typeOf(p)) out.type = e.type;
  if (e.readOnly) out.readOnly = true;
  if (e.detached) out.detached = true;
  if (e.session && e.session !== DEFAULT) out.session = e.session;
  return out;
}

/** `raw` (a scope's value) with `fn` applied to the item for `session` and
 *  `alias`; `fn` returns the new item, or null to remove it. Items that are
 *  not well-formed are kept as they are. */
function editItem(raw, session, alias, fn) {
  const k = keyOf({ session, alias });
  const out = [];
  for (const item of Array.isArray(raw) ? raw : []) {
    const [e] = normalize([item]);
    if (e && keyOf(e) === k) {
      const next = fn(item);
      if (next) out.push(next);
    } else {
      out.push(item);
    }
  }
  return out;
}

/**
 * What a running session should do now that the setting went from `prev`
 * to `next` (both its entries only). `attached` maps the lower-cased alias
 * of each database attached now to what it is. The answer is a list of
 * `{ do: "detach", alias }` and `{ do: "attach", entry }`, in order.
 *
 *   - An entry taken out, or marked detached, is detached if it is
 *     attached.
 *   - One that now names something else (path, type, read-only) is
 *     detached, and attached again if it is a local file or was attached.
 *   - A new local one, or one no longer marked detached, is attached. A
 *     server waits until it is needed.
 *   - Something attached already is left alone, so what the Catalog has
 *     just done (attach, then add to the setting) asks for nothing more.
 */
function plan(prev, next, attached) {
  const before = new Map(prev.map((e) => [keyOf(e), e]));
  const after = new Map(next.map((e) => [keyOf(e), e]));
  const out = [];
  const isOn = (e) => attached.has(String(e.alias).toLowerCase());
  for (const [k, p] of before) {
    if (!after.has(k) && isOn(p)) out.push({ do: "detach", alias: p.alias });
  }
  for (const [k, n] of after) {
    const p = before.get(k);
    const on = isOn(n);
    if (n.detached) {
      if (on) out.push({ do: "detach", alias: n.alias });
      continue;
    }
    if (p && !sameTarget(p, n)) {
      if (on) out.push({ do: "detach", alias: n.alias });
      if (!isServer(n) || on) out.push({ do: "attach", entry: n });
      continue;
    }
    if (on || isServer(n)) continue;
    if (!p || p.detached) out.push({ do: "attach", entry: n });
  }
  return out;
}

/**
 * Which of `aliases` the statement `sql` refers to: as `alias.` (the start
 * of a qualified name, quoted or not) or `USE alias`, in any case, outside
 * string literals and comments. Lower-cased.
 */
function references(sql, aliases) {
  const wanted = new Set(aliases.map((a) => String(a).toLowerCase()));
  const found = new Set();
  if (!wanted.size) return [];
  const s = String(sql);
  const n = s.length;
  let i = 0;
  let prev = null; // the last token: { kind: "word" | "quoted" | "dot" | "other", text }
  let before = null; // the token before it
  const push = (tok) => {
    before = prev;
    prev = tok;
  };
  while (i < n) {
    const c = s[i];
    if (/\s/.test(c)) {
      i++;
      continue;
    }
    if (c === "-" && s[i + 1] === "-") {
      const end = s.indexOf("\n", i);
      i = end < 0 ? n : end + 1;
      continue;
    }
    if (c === "/" && s[i + 1] === "*") {
      const end = s.indexOf("*/", i + 2);
      i = end < 0 ? n : end + 2;
      continue;
    }
    if (c === "'") {
      i++;
      while (i < n) {
        if (s[i] === "'" && s[i + 1] === "'") i += 2;
        else if (s[i] === "'") break;
        else i++;
      }
      i++;
      push({ kind: "other" });
      continue;
    }
    if (c === "$") {
      const tag = /^\$[A-Za-z_]*\$/.exec(s.slice(i));
      if (tag) {
        const end = s.indexOf(tag[0], i + tag[0].length);
        i = end < 0 ? n : end + tag[0].length;
        push({ kind: "other" });
        continue;
      }
    }
    if (c === '"') {
      let text = "";
      i++;
      while (i < n) {
        if (s[i] === '"' && s[i + 1] === '"') {
          text += '"';
          i += 2;
        } else if (s[i] === '"') break;
        else text += s[i++];
      }
      i++;
      push({ kind: "quoted", text });
      continue;
    }
    if (/[A-Za-z_\u0080-￿]/.test(c)) {
      const m = /^[A-Za-z_\u0080-￿][A-Za-z0-9_$\u0080-￿]*/.exec(s.slice(i, i + 256));
      const text = m[0];
      i += text.length;
      // USE alias
      if (before === null && prev && prev.kind === "word" && /^use$/i.test(prev.text) && wanted.has(text.toLowerCase())) found.add(text.toLowerCase());
      push({ kind: "word", text });
      continue;
    }
    if (c === ".") {
      // alias. -- not the second part of a.b.c
      const named = prev && (prev.kind === "word" || prev.kind === "quoted");
      const first = !before || before.kind !== "dot";
      if (named && first && wanted.has(prev.text.toLowerCase())) found.add(prev.text.toLowerCase());
      i++;
      push({ kind: "dot" });
      continue;
    }
    i++;
    push({ kind: "other" });
  }
  // USE "alias"
  const use = /^\s*use\s+"((?:[^"]|"")+)"/i.exec(stripLeadingComments(s));
  if (use && wanted.has(use[1].replace(/""/g, '"').toLowerCase())) found.add(use[1].replace(/""/g, '"').toLowerCase());
  return [...found];
}

function stripLeadingComments(sql) {
  let s = sql;
  for (;;) {
    s = s.replace(/^\s+/, "");
    if (s.startsWith("--")) s = s.slice(s.indexOf("\n") < 0 ? s.length : s.indexOf("\n") + 1);
    else if (s.startsWith("/*")) s = s.slice(s.indexOf("*/") < 0 ? s.length : s.indexOf("*/") + 2);
    else return s;
  }
}

/** The records of the store the extension used before the setting, as
 *  entries to add to it: `old` maps a session name to its records. What is
 *  in memory, or has no path, is not moved, nor an alias `have` already
 *  has. */
function migrate(old, have) {
  const out = [];
  const seen = new Set(have.map(keyOf));
  for (const [name, recs] of Object.entries(old || {})) {
    for (const r of Array.isArray(recs) ? recs : []) {
      if (!r || typeof r.alias !== "string" || !r.alias || typeof r.path !== "string" || !r.path || r.path === ":memory:") continue;
      const e = { alias: r.alias, path: conninfo.withoutPassword(r.path), type: typeOf(r.path, r.type), readOnly: r.readOnly === true };
      if (r.detached) e.detached = true;
      if (name !== DEFAULT) e.session = name;
      if (seen.has(keyOf(e))) continue;
      seen.add(keyOf(e));
      out.push(e);
    }
  }
  return out;
}

module.exports = { DEFAULT, SCOPES, typeOf, isServer, normalize, sessionOf, keyOf, sameTarget, merge, chooseScope, scopesWith, toSetting, editItem, plan, references, migrate };
