-- MOD033 having-without-aggregate: a HAVING that could be a WHERE

-- Fires.
SELECT g, count(*) FROM t GROUP BY g HAVING g > 1;

SELECT g, status, sum(x) FROM t GROUP BY g, status HAVING status = 'shipped' AND g = 2;

SELECT t.g, count(*) FROM t GROUP BY t.g HAVING t.g <> 1;

-- Does not fire.
-- Uses an aggregate: this is what HAVING is for.
SELECT g, count(*) FROM t GROUP BY g HAVING count(*) > 10;

-- n is the aggregate's alias, not a grouped column.
SELECT g, count(*) AS n FROM t GROUP BY g HAVING n > 10;

-- Under ROLLUP, HAVING sees the subtotal rows WHERE cannot.
SELECT g, count(*) FROM t GROUP BY ROLLUP (g) HAVING g IS NULL;

-- A subquery in the condition is skipped.
SELECT g, count(*) FROM t GROUP BY g HAVING g IN (SELECT x FROM b);

-- GROUP BY ALL names no columns to check against.
SELECT g, count(*) FROM t GROUP BY ALL HAVING g > 1;
