-- MOD014 case-to-filter: agg(CASE WHEN c THEN x END) -> agg(x) FILTER (WHERE c)

-- Fires, and the fix is equivalent: no ELSE, so the CASE is NULL where the
-- filter drops the row, and aggregates skip NULLs either way.
SELECT sum(CASE WHEN a THEN x END) FROM t;

SELECT g, max(CASE WHEN status = 'shipped' THEN y END) FROM t GROUP BY g;

-- Fires, and the fix changes results, which is why it is unsafe: the ELSE 0
-- makes a group with no match sum to 0, the FILTER version to NULL.
SELECT g, sum(CASE WHEN x > 5 THEN y ELSE 0 END) FROM t GROUP BY g;

-- Does not fire.
-- Two branches.
SELECT sum(CASE WHEN a THEN x WHEN NOT a THEN y END) FROM t;

-- ELSE is not a constant.
SELECT sum(CASE WHEN a THEN x ELSE y END) FROM t;

-- Not an aggregate.
SELECT upper(CASE WHEN a THEN status END) FROM t;

-- The exact count_if shape is MOD030's.
SELECT sum(CASE WHEN a THEN 1 ELSE 0 END) FROM t;
