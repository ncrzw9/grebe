-- MOD032 not-in-null: x NOT IN (..., NULL, ...) is never true

-- Fires.
SELECT * FROM t WHERE x NOT IN (1, NULL);

SELECT * FROM t WHERE status NOT IN ('lost', NULL, 'shipped');

SELECT count(*) FILTER (WHERE x NOT IN (NULL)) FROM t;

SELECT * FROM t WHERE g = 1 AND y not in (2, null);

-- Does not fire.
-- Plain IN still matches the non-NULL items.
SELECT * FROM t WHERE x IN (1, NULL);

-- No NULL in the list.
SELECT * FROM t WHERE x NOT IN (1, 2);

-- A subquery is MOD028's concern, not this rule's.
SELECT * FROM t WHERE x NOT IN (SELECT x FROM b);

-- A NULL that is not a bare literal is not matched.
SELECT * FROM t WHERE x NOT IN (1, CAST(NULL AS INTEGER));
