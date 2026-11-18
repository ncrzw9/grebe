-- CAT003 unbounded-scan: the statement's result reads every row of a table
-- the catalog (catalog.json here) estimates at 1,000,000 rows or more, and
-- nothing filters, limits, samples, joins, groups or aggregates it.

-- Fires.
SELECT * FROM orders;

SELECT id, total FROM orders o;

SELECT u.id, lower(u.email) AS email FROM users u;

FROM logs;

-- Each arm of a set operation is returned.
SELECT id FROM users UNION ALL SELECT id FROM payments;

-- Does not fire.
-- Bounded.
SELECT * FROM orders WHERE customer_id = 7;

SELECT * FROM orders LIMIT 10;

SELECT * FROM logs USING SAMPLE 10%;

SELECT * FROM logs TABLESAMPLE 5 ROWS;

SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id;

SELECT id FROM users QUALIFY row_number() OVER (ORDER BY created DESC) <= 10;

-- Aggregates read everything on purpose, and return little.
SELECT count(*) FROM logs;

SELECT customer_id, sum(total) FROM orders GROUP BY customer_id;

SELECT DISTINCT customer_id FROM orders;

SELECT my_macro(total) FROM orders;

-- A small table, or one with no estimate.
SELECT * FROM customers;

SELECT * FROM notes;

-- Not the statement's result: whatever reads it bounds it, or the copy is
-- the point.
WITH o AS (SELECT * FROM orders) SELECT * FROM o WHERE id = 1;

SELECT * FROM (SELECT * FROM orders) WHERE id = 1;

CREATE TABLE orders_copy AS SELECT * FROM orders;

INSERT INTO orders_copy SELECT * FROM orders;

COPY (SELECT * FROM orders) TO 'orders.parquet';

CREATE VIEW all_orders AS SELECT * FROM orders;

EXPLAIN SELECT * FROM orders;
