-- Recorded divergences between our matcher and a real DuckDB engine.
--
-- A divergence is recorded here rather than papered over by bending the
-- tokenizer or hand-editing vendor/. Each entry says which engine, which
-- direction, and either why we do not "fix" it or how the build-time override
-- layer resolves it; either way the entry is a regression guard.

-- === 1. Ellipsis placeholders in mined documentation ===
-- Direction: engine (2.0 preview) ACCEPTS, we reject.
-- Count: 5 statements in the dogfood corpus, all the same snippet mined
-- from DuckDB's documentation.
-- Ruling: record as a fixture.
--
-- `...` is a prose placeholder, not SQL. A number scanner that starts a
-- numeric token on a bare `.` lexes `...` as a number, and the grammar
-- happens to accept it; ours lexes three punctuation tokens. Teaching
-- the scanner to swallow bare dot-runs would make the tokenizer wrong about
-- real SQL to chase 0.03% of the corpus.
-- expect: REJECT
CREATE OR REPLACE TEMPORARY TABLE t1 AS ...;

-- === 2. Bare SELECT with no target list -- RESOLVED ===
-- Direction: the grammar text ACCEPTS, the engine rejects; we follow the
-- engine and reject.
--
-- The grammar text makes the target list optional (`SelectClause <- 'SELECT'
-- DistinctClause? TargetList?`); the shipping parser does not, and raises
-- "SELECT clause without selection list".
--
-- This is the main member of a class: clauses that are optional in the
-- grammar text but required by the engine. In the negative differential
-- (mutants DuckDB rejects), 474 of the 990 the raw grammar text accepts come
-- from that class. A linter that accepts what DuckDB refuses to run reports
-- nothing exactly where a diagnostic matters most, so we follow the engine:
-- `apply_structural_overrides` in crates/grebe-syntax/build.rs drops the `?`.
--
-- The entry stays as a regression guard -- this file exists to make a silent
-- change in either direction fail loudly.
-- expect: REJECT
SELECT FROM read_json('birds.json');

-- === 3. Chained comparisons -- RESOLVED ===
-- Direction: the grammar text ACCEPTS, both engines reject; we follow the
-- engines and reject.
--
-- `ComparisonExpression <- BetweenInLikeExpression ComparisonExpressionTail*`
-- takes any number of tails. The 2.0 transformer refuses more than one
-- ("Chained comparisons are not supported, use AND to combine comparisons")
-- and 1.5.5 calls it a syntax error. `apply_structural_overrides` narrows
-- the `*` to `?`, here and for `IS DISTINCT FROM`.
-- expect: REJECT
SELECT 1 < 2 < 3;

-- === 4. Checks the engine makes after the grammar has matched ===
-- Direction: engine (1.5.5 and 2.0) REJECTS with a ParserException, we
-- accept.
-- Count: 291 of the 62,194 statements of DuckDB's own test suite at v2.0.0.
-- Ruling: record as a fixture.
--
-- These are well-formed by the grammar and refused by the transformer that
-- runs on the parse tree: contradictory sequence bounds, an option given
-- twice, `COUNT(DISTINCT)` with no argument, a duplicate CTE name, DDL
-- inside a CTE, a hex literal with an odd number of digits. Each is a
-- semantic rule of one statement kind, not syntax, and reimplementing the
-- transformer is out of scope for a parser generated from the grammar.
-- The statement still fails when it is run, with the engine's own message.
-- expect: PARSE
CREATE SEQUENCE seq MINVALUE 7 MAXVALUE 5;
-- expect: PARSE
SELECT COUNT(DISTINCT);

-- === 5. A stray backtick ===
-- Direction: engine 1.5.5 ACCEPTS, engine 2.0 rejects; we follow 2.0.
-- Count: 1 statement, from a test of exactly this.
--
-- 2.0 reads a backtick as a quoted-identifier delimiter, so an unpaired one
-- is "unterminated quoted identifier". 1.5.5 lexes it as an operator
-- character and so parses `SELECT `x` as a prefix operator applied to `x`.
-- One tokenizer cannot do both; backtick identifiers are the documented
-- feature and the prefix operator is an accident.
-- expect: REJECT
SELECT `x FROM (SELECT 1 x);

-- === 6. Nesting depth ===
-- Direction: engine 1.5.5 ACCEPTS, engine 2.0 rejects; we reject.
-- Count: 1 statement, 1,100 nested parentheses (too long to keep here).
--
-- 2.0 has a limit of 1,000 nested expressions (`max_expression_depth`);
-- 1.5.5 has none that this statement reaches. The matcher's own recursion
-- limit turns it away as well.
