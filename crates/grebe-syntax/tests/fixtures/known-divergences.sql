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
