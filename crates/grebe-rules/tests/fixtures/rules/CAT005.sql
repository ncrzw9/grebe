-- CAT005 nullable-not-in: `x NOT IN (SELECT c FROM t ...)`, `t` the
-- subquery's only table, and the catalog says `c` may be NULL. It reports
-- in MOD028's place (see the MOD028 tests for the NOT NULL case).

-- Fires.
SELECT id FROM orders WHERE id NOT IN (SELECT order_id FROM payments);

SELECT id FROM orders WHERE id NOT IN (SELECT p.order_id FROM payments p WHERE p.amount > 0);

SELECT id FROM customers WHERE name NOT IN (SELECT DISTINCT code FROM tags);

-- Does not fire.
-- The column cannot be NULL.
SELECT id FROM customers WHERE id NOT IN (SELECT customer_id FROM payments);

-- The subquery removes the NULLs itself.
SELECT id FROM orders WHERE id NOT IN (SELECT order_id FROM payments WHERE order_id IS NOT NULL);

-- Plain IN is not affected by a NULL.
SELECT id FROM orders WHERE id IN (SELECT order_id FROM payments);

-- Nullability not in the catalog, or not one column of one table.
SELECT id FROM orders WHERE id NOT IN (SELECT id FROM customers);

SELECT id FROM orders WHERE id NOT IN (SELECT p.order_id FROM payments p JOIN refunds r ON r.id = p.id);

SELECT id FROM orders WHERE id NOT IN (SELECT order_id FROM payments UNION SELECT id FROM refunds);

SELECT id FROM orders WHERE id NOT IN (SELECT coalesce(order_id, 0) FROM payments);
