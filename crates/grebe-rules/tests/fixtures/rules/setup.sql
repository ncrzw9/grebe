-- Data the rule fixtures run against when a rewrite's results are compared
-- with the original's. Every combination of values, NULL included, so a
-- rewrite that differs only when some operand is NULL cannot hide.
CREATE TABLE t AS
SELECT *
FROM (VALUES (1), (2), (NULL)) AS xs (x)
CROSS JOIN (VALUES (1), (2), (NULL)) AS ys (y)
CROSS JOIN (VALUES (true), (false), (NULL)) AS bs (a)
CROSS JOIN (VALUES (1), (2)) AS gs (g)
CROSS JOIN (VALUES ('shipped'), ('lost'), (NULL)) AS ss (status);

-- Same columns, no rows: aggregates over empty input behave differently.
CREATE TABLE empty_t AS SELECT * FROM t WHERE false;
