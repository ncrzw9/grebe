// `node --test editors/vscode/test/*.test.js` -- no dependencies, no VS Code.

"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { redact, isConnection, parse, where, aliasFor, withPassword } = require("../conninfo");

test("redact: a password never survives, in any spelling of a connection string", () => {
  const cases = [
    ["dbname=ub host=localhost user=postgres password=postrgres", "dbname=ub host=localhost user=postgres password=***"],
    ["host=h PASSWORD = s3cret port=5432", "host=h PASSWORD = *** port=5432"],
    ["host=h password='a b\\'c' port=1", "host=h password=*** port=1"],
    ["host=h passwd=x", "host=h passwd=***"],
    ["postgresql://postgres:postrgres@localhost:5432/ub", "postgresql://postgres:***@localhost:5432/ub"],
    ["postgres://u:p%40ss@h/db?sslmode=require&password=two", "postgres://u:***@h/db?sslmode=require&password=***"],
    // As DuckDB reports a failed ATTACH, and as it sits in the SQL.
    [
      'IO Error: Unable to connect to Postgres at "dbname=ub password=wrongpw": FATAL',
      'IO Error: Unable to connect to Postgres at "dbname=ub password=***": FATAL',
    ],
    ["ATTACH 'dbname=ub password=pw' AS local (TYPE postgres)", "ATTACH 'dbname=ub password=***' AS local (TYPE postgres)"],
    ["ATTACH 'host=h password=''a b''' AS x", "ATTACH 'host=h password=***' AS x"],
  ];
  for (const [input, want] of cases) assert.equal(redact(input), want, input);
  assert.equal(redact("SELECT 'no secrets here'"), "SELECT 'no secrets here'");
  assert.equal(redact("postgresql://postgres@localhost/ub"), "postgresql://postgres@localhost/ub", "no password, nothing to hide");
  assert.equal(redact(null), null);
});

test("a connection string is told from a file path", () => {
  assert.ok(isConnection("dbname=ub host=localhost"));
  assert.ok(isConnection("postgresql://localhost/ub"));
  assert.ok(!isConnection("/data/warehouse.duckdb"));
  assert.ok(!isConnection("C:\\data\\a=b.duckdb"));
  assert.ok(!isConnection(""));
});

test("parse: libpq key=value and URIs", () => {
  assert.deepEqual(parse("dbname=ub host=localhost port=5432 user=postgres password='p w'"), {
    dbname: "ub",
    host: "localhost",
    port: "5432",
    user: "postgres",
    password: "p w",
  });
  assert.deepEqual(parse("postgresql://me:p%40ss@db.example.com:6543/sales?sslmode=require"), {
    user: "me",
    password: "p@ss",
    host: "db.example.com",
    port: "6543",
    dbname: "sales",
    sslmode: "require",
  });
  assert.deepEqual(parse("postgres://localhost"), { host: "localhost" });
});

test("where: a short label with no secrets in it", () => {
  assert.equal(where("dbname=ub host=localhost port=5432 user=postgres password=postrgres"), "ub@localhost:5432");
  assert.equal(where("postgresql://postgres:pw@localhost/ub"), "ub@localhost");
  assert.equal(where("dbname=ub"), "ub");
  assert.equal(where("host=db.internal password=x"), "db.internal");
  assert.equal(where("password=x"), "connection");
  assert.equal(where("/data/warehouse.duckdb"), "warehouse.duckdb");
  assert.equal(where(null), "");
});

test("an alias from the database name, and a password added in the same style", () => {
  assert.equal(aliasFor("dbname=ub host=localhost"), "ub");
  assert.equal(aliasFor("postgresql://localhost/sales"), "sales");
  assert.equal(aliasFor("host=localhost"), "pg");
  assert.deepEqual(parse(withPassword("dbname=ub user=postgres", "it's a\\pw")).password, "it's a\\pw");
  assert.equal(withPassword("postgresql://postgres@localhost/ub", "p@ss"), "postgresql://postgres@localhost/ub?password=p%40ss");
  assert.equal(withPassword("postgresql://h/ub?sslmode=disable", "x"), "postgresql://h/ub?sslmode=disable&password=x");
});
