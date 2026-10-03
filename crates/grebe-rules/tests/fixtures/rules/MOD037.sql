-- MOD037 order-by-random-sample: ORDER BY random() LIMIT n sorts every row
-- to keep n; USING SAMPLE n ROWS is about 4x faster for the same uniform
-- sample. Only where the rewrite is equivalent: USING SAMPLE samples before
-- WHERE, grouping and the rest, so any of those disqualify.

-- Fires.
SELECT * FROM t ORDER BY random() LIMIT 100;

SELECT x, y, upper(status) FROM t ORDER BY RANDOM() DESC LIMIT 10;

-- Does not fire.
-- A WHERE would run after the sample, returning fewer rows.
SELECT * FROM t WHERE x = 1 ORDER BY random() LIMIT 100;

SELECT g, count(*) FROM t GROUP BY g ORDER BY random() LIMIT 2;

SELECT DISTINCT g FROM t ORDER BY random() LIMIT 1;

SELECT x, row_number() OVER (ORDER BY y) FROM t ORDER BY random() LIMIT 5;

-- OFFSET too.
SELECT * FROM t ORDER BY random() LIMIT 10 OFFSET 5;

-- Ordered by something else as well.
SELECT * FROM t ORDER BY g, random() LIMIT 10;

-- No LIMIT: shuffling every row, not sampling.
SELECT * FROM t ORDER BY random();

-- A set operation: the sample would apply to one branch, not the result.
SELECT * FROM t UNION ALL SELECT * FROM empty_t ORDER BY random() LIMIT 5;
