-- CAT009 fan-out-join: a join of two tables that both have a key in the
-- catalog, on columns that are a key of neither.

-- Fires.
SELECT * FROM payments p JOIN refunds r ON r.customer_id = p.customer_id;

SELECT * FROM payments JOIN refunds USING (customer_id);

SELECT * FROM payments p LEFT JOIN refunds r ON p.note = r.note AND p.customer_id = r.customer_id;

-- Does not fire.
-- A key on one side.
SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id;

SELECT * FROM payments p JOIN refunds r ON r.id = p.id;

SELECT * FROM users u JOIN teams t ON t.name = u.name;

-- A table without a key in the catalog.
SELECT * FROM logs l JOIN payments p ON p.note = l.msg;

-- Not only equalities, more than two tables, a semi join.
SELECT * FROM payments p JOIN refunds r ON r.customer_id = p.customer_id AND r.amount > p.amount;

SELECT * FROM payments p JOIN refunds r ON r.customer_id = p.customer_id JOIN orders o ON o.id = p.order_id;

SELECT * FROM payments p SEMI JOIN refunds r ON r.customer_id = p.customer_id;
