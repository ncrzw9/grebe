-- MOD041 from-before-select: `SELECT a FROM t ...` can be written
-- `FROM t SELECT a ...`.

-- Fires.
SELECT x, y FROM t;

SELECT x, y FROM t WHERE x = 1 ORDER BY y;

SELECT t.x, b.y FROM t JOIN b USING (x) WHERE b.y > 0;

-- DISTINCT moves with the select clause.
SELECT DISTINCT x FROM t;

SELECT DISTINCT ON (x) x, y FROM t ORDER BY x, y;

-- A narrowed star is a select list like any other.
SELECT * EXCLUDE (y) FROM t;

-- Each query block on its own: the outer query and the CTE.
WITH recent AS (SELECT x FROM t WHERE y > 1)
SELECT x FROM recent;

-- Both members of a set operation.
SELECT x FROM t UNION ALL SELECT x FROM b;

-- Does not fire.
-- Already FROM-first.
FROM t SELECT x, y;

-- The bare star is MOD022's: it deletes the clause.
SELECT * FROM t;

-- No FROM clause.
SELECT 1;

-- A comment inside the text that would move.
SELECT x, -- the key
    y
FROM t;
