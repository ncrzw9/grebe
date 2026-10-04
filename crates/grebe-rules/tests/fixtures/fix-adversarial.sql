-- Adversarial input for `--fix`, one statement per entry.
--
-- A corpus of TPC-H/TPC-DS queries and DuckDB documentation examples
-- exercises ordinary SQL, not the SQL that breaks a rewriter. It passes even
-- when MOD016's fix does not converge, because no corpus file happens to
-- declare two unused CTEs in one WITH.
--
-- So this file holds the shapes that *should* be hard. Every statement here
-- must, after `--fix` converges: still parse, and be stable under a second
-- run. Enforced by tests/fix_adversarial.rs.
--
-- Add a case here whenever a fix bug is found, before fixing it.

-- Two unused CTEs in one WITH: the deletions share the comma between them, so
-- a single pass drops one and leaves the file still changing.
WITH a AS (SELECT 1), b AS (SELECT 2) SELECT 9;

-- Three, for good measure -- the middle one is the interesting position.
WITH a AS (SELECT 1), b AS (SELECT 2), c AS (SELECT 3) SELECT 9;

-- Only the first is unused; the comma to delete is the following one.
WITH a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM b;

-- Only the last is unused; the comma to delete is the preceding one.
WITH a AS (SELECT 1), b AS (SELECT 2) SELECT * FROM a;

-- A comment sits where the comma is being deleted.
WITH a AS (SELECT 1), -- note
     b AS (SELECT 2) SELECT * FROM b;

-- The star belongs to COLUMNS(*), not to count(). Deleting it would produce
-- the invalid `count(COLUMNS())`.
SELECT min(COLUMNS(*)), count(COLUMNS(*)) FROM numbers;

-- count() nested inside another call, so the fix span sits deep in the tree.
SELECT greatest(count(*), 1) FROM t;

-- Nested fixable calls: the outer and inner rewrites are both eligible and
-- their spans are nested rather than merely adjacent.
SELECT ifnull(ifnull(a, 0), 1) FROM t;

-- A self-alias inside a subquery, so the fix applies below the top level.
SELECT (SELECT x AS x) AS y FROM t;

-- Several fixable findings in one statement, adjacent in the select list.
SELECT count(*), ifnull(a, 0), b AS b FROM t;

-- A fixable finding on a statement that follows an unparseable one, which
-- forces the per-statement fallback path and its span offsetting.
THIS IS NOT SQL AT ALL;
SELECT count(*) FROM t;

-- A fixable finding whose statement carries a suppression: must not be fixed.
-- grebe: ignore[MOD001]
SELECT count(*) FROM suppressed_t;

-- An unused CTE after a comment containing a comma: the separator is the
-- comma token, never the one inside the comment.
WITH a AS (SELECT 1 AS x) /* keep, note */ , unused AS (SELECT 2) SELECT * FROM a;

-- The same with a line comment carrying a comma, and the unused CTE first.
WITH unused AS (SELECT 2) -- first, then
, b AS (SELECT 1 AS x) SELECT * FROM b;
