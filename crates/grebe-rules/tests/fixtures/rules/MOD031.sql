-- MOD031 case-to-function: a CASE that is exactly coalesce(...) or nullif(...)

-- Fires.
SELECT CASE WHEN x IS NULL THEN y ELSE x END FROM t;

SELECT CASE WHEN x IS NOT NULL THEN x ELSE 0 END FROM t;

SELECT case when t.x is null then t.y + 1 else t.x end FROM t;

SELECT CASE WHEN x = y THEN NULL ELSE x END FROM t;

-- The kept operand may be on either side of the comparison.
SELECT CASE WHEN y = x THEN NULL ELSE x END FROM t;

SELECT CASE WHEN status = 'lost' THEN NULL ELSE status END FROM t;

-- Does not fire.
-- The repeated operand is not a plain column: the CASE evaluates it twice.
SELECT CASE WHEN abs(x) IS NULL THEN y ELSE abs(x) END FROM t;

-- ELSE is not the tested operand.
SELECT CASE WHEN x IS NULL THEN y ELSE g END FROM t;

-- Two branches.
SELECT CASE WHEN x IS NULL THEN y WHEN y IS NULL THEN x ELSE 0 END FROM t;

-- No ELSE.
SELECT CASE WHEN x IS NULL THEN y END FROM t;

-- Not equality.
SELECT CASE WHEN x <> y THEN NULL ELSE x END FROM t;

-- THEN is not NULL.
SELECT CASE WHEN x = y THEN 0 ELSE x END FROM t;

-- Switch form.
SELECT CASE x WHEN NULL THEN y ELSE x END FROM t;
