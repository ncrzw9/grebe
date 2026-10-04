-- MOD022 from-first: `SELECT * FROM t ...` can be written `FROM t ...`.

-- Fires.
SELECT * FROM t;

SELECT * FROM t WHERE x = 1 ORDER BY y;

SELECT * FROM t JOIN b USING (x);

-- Does not fire.
-- DISTINCT has no FROM-first spelling; dropping the select clause would
-- drop the deduplication with it.
SELECT DISTINCT * FROM t;

SELECT DISTINCT ON (x) * FROM t ORDER BY x, y;

-- The star is narrowed or reshaped.
SELECT * EXCLUDE (y) FROM t;

SELECT x, y FROM t;

-- No FROM clause.
SELECT 1;
