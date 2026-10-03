-- MOD034 redundant-count-default: count(...) is never NULL

-- Fires.
SELECT coalesce(count(*), 0) FROM t;

SELECT g, COALESCE(count(x), 0) FROM t GROUP BY g;

SELECT ifnull(count(DISTINCT status), 0) FROM t;

SELECT coalesce(count(*), 0) FROM empty_t;

-- Does not fire.
-- sum can be NULL: the default does something.
SELECT coalesce(sum(x), 0) FROM t;

-- Only a 0 default is matched: it is the common spelling, and a default of
-- another type (a string, say) would change the result type on removal.
SELECT coalesce(count(*), -1) FROM t;

-- More than two arguments.
SELECT coalesce(count(*), x, 0) FROM t GROUP BY x;
