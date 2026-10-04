-- MOD027 window-no-orderby: a window function whose answer depends on row
-- order, with nothing ordering the window. The result is whatever order the
-- scan produced.

-- Fires.
SELECT row_number() OVER (PARTITION BY g) FROM t;

SELECT x, lag(x) OVER () FROM t;

SELECT first_value(x) OVER (PARTITION BY g) FROM t;

SELECT last_value(x) OVER (PARTITION BY g) FROM t;

SELECT nth_value(x, 2) OVER (PARTITION BY g) FROM t;

-- Does not fire.
-- Ordered in the window.
SELECT first_value(x) OVER (PARTITION BY g ORDER BY y) FROM t;

-- Ordered inside the call, which DuckDB also accepts.
SELECT row_number(ORDER BY x) OVER () FROM t;

SELECT lag(x ORDER BY y) OVER (PARTITION BY g) FROM t;

-- An aggregate over the whole partition does not depend on order.
SELECT sum(x) OVER (PARTITION BY g) FROM t;

-- A named window is not resolved, so it is left alone.
SELECT rank() OVER w FROM t WINDOW w AS (PARTITION BY g);
