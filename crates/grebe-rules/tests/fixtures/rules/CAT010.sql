-- CAT010 redundant-distinct: SELECT DISTINCT over one table whose select
-- list holds a key that cannot be NULL.

-- Fires.
SELECT DISTINCT id, total FROM orders;

SELECT DISTINCT * FROM customers;

SELECT DISTINCT t.name AS n, t.label FROM tags t;

SELECT DISTINCT name FROM teams;

-- Does not fire.
-- No key in the select list.
SELECT DISTINCT customer_id FROM orders;

-- A unique key that may be NULL: several rows can hold NULL.
SELECT DISTINCT code FROM tags;

-- Expressions, joins, DISTINCT ON, GROUP BY.
SELECT DISTINCT id + 1 FROM orders;

SELECT DISTINCT o.id FROM orders o JOIN customers c ON c.id = o.customer_id;

SELECT DISTINCT ON (id) id, total FROM orders;

SELECT DISTINCT id FROM orders GROUP BY id;

SELECT DISTINCT * EXCLUDE (id) FROM customers;
