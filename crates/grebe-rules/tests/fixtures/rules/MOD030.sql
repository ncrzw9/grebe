-- MOD030 sum-case-to-count-if: sum(CASE WHEN c THEN 1 ELSE 0 END) -> count_if(c)

-- Fires.
SELECT sum(CASE WHEN a THEN 1 ELSE 0 END) FROM t;

SELECT g, SUM(CASE WHEN status = 'shipped' THEN 1 ELSE 0 END) FROM t GROUP BY g;

SELECT sum(CASE WHEN x > y THEN 1 ELSE 0 END) FROM empty_t;

-- Does not fire: each of these differs from count_if in value or type.
-- No ELSE: NULL, not 0, when nothing matches.
SELECT sum(CASE WHEN a THEN 1 END) FROM t;

-- A DECIMAL sum, not an integer count.
SELECT sum(CASE WHEN a THEN 1.0 ELSE 0 END) FROM t;

-- Counts twos, not rows.
SELECT sum(CASE WHEN a THEN 2 ELSE 0 END) FROM t;

-- count() of a never-NULL CASE counts every row.
SELECT count(CASE WHEN a THEN 1 ELSE 0 END) FROM t;
