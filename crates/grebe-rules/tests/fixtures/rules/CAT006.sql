-- CAT006 ambiguous-column: an unqualified column that two or more tables of
-- its query's FROM have. DuckDB rejects every one of these with "Ambiguous
-- reference to column name".

-- Fires.
SELECT id FROM orders, customers;

SELECT o.total FROM orders o JOIN payments p ON p.order_id = o.id WHERE customer_id = 3;

SELECT customer_id, count(*) FROM orders o JOIN payments p ON p.order_id = o.id GROUP BY customer_id;

SELECT o.id AS id FROM orders o JOIN customers c ON c.id = o.customer_id WHERE id > 5;

SELECT note FROM payments a1 JOIN payments a2 ON a1.id = a2.id;

SELECT p.id FROM payments p JOIN refunds r ON r.customer_id = p.customer_id QUALIFY row_number() OVER (ORDER BY amount) = 1;

-- In an ON, among the tables joined so far.
SELECT 1 FROM payments p JOIN refunds r ON customer_id = 1 JOIN orders o ON o.id = p.order_id;

-- Does not fire.
-- Qualified, or in one table only.
SELECT o.id, c.id, total, name FROM orders o JOIN customers c ON c.id = o.customer_id;

-- USING and NATURAL merge the column.
SELECT id FROM orders JOIN customers USING (id);

SELECT note FROM payments NATURAL JOIN refunds;

-- SEMI and ANTI joins do not bring the right side's columns in.
SELECT id FROM orders SEMI JOIN customers ON orders.customer_id = customers.id;

-- An ON sees only the tables joined before it.
SELECT 1 FROM orders o JOIN customers c ON customer_id = c.id JOIN payments p ON p.order_id = o.id;

-- The inner query's own table wins over the outer ones.
SELECT 1 FROM orders o, customers c WHERE EXISTS (SELECT 1 FROM notes WHERE notes.body = 'x' AND id = 1);

-- ORDER BY prefers the select list.
SELECT o.id FROM orders o, customers c ORDER BY id;

-- Anything but catalog tables in the FROM.
SELECT id FROM orders, (SELECT 1 AS id) s;

WITH c AS (SELECT * FROM customers) SELECT id FROM orders, c;

SELECT id FROM orders, range(3);

-- A lambda parameter.
SELECT list_transform([1, 2], lambda id: id + 1) FROM orders, customers;

-- A comma binds looser than JOIN: this ON sees customers and payments only.
SELECT 1 FROM orders o, customers c JOIN payments p ON customer_id = c.id;
