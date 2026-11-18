-- CAT004 index-defeating-filter: a WHERE or ON conjunct wraps the leading
-- column of a key or index (catalog.json: users.id, users.email, users.created,
-- orders.id, orders.customer_id) in something DuckDB cannot see through, and
-- compares it with a constant.

-- Fires.
SELECT id FROM users WHERE lower(email) = 'a@example.com';

SELECT id FROM users u WHERE upper(u.email) IN ('A', 'B');

SELECT id FROM users WHERE trim(email) = 'a' AND name = 'x';

SELECT id FROM users WHERE strftime(created, '%Y') = '2024';

SELECT id FROM users WHERE id::VARCHAR = '42';

SELECT id FROM users WHERE CAST(id AS TEXT) = '42';

SELECT id FROM users WHERE email LIKE '%@example.com';

SELECT id FROM users WHERE email ILIKE '_dmin%';

SELECT o.id FROM orders o JOIN users u ON u.id = o.customer_id AND lower(u.email) = 'a';

SELECT id FROM users WHERE 'a' = lower(email);

-- An unqualified column one table of the FROM has.
SELECT o.id FROM orders o, users u WHERE lower(email) = 'x' AND o.id = u.id;

-- Does not fire.
-- DuckDB turns these back into a filter on the column.
SELECT id FROM users WHERE id + 0 = 5;

SELECT id FROM users WHERE id::BIGINT = 5;

SELECT id FROM users WHERE created::DATE = DATE '2024-01-01';

SELECT id FROM users WHERE date_trunc('day', created) = TIMESTAMP '2024-01-01';

SELECT id FROM users WHERE email LIKE 'adm%';

-- Not an indexed column, or not certain which table it is.
SELECT id FROM users WHERE lower(name) = 'x';

SELECT id FROM (SELECT * FROM users) s WHERE lower(email) = 'x';

-- Not a constant on the other side, not a top-level conjunct, negated.
SELECT u.id FROM users u JOIN teams t ON lower(u.email) = lower(t.name);

SELECT id FROM users WHERE lower(email) = 'x' OR name = 'y';

SELECT id FROM users WHERE NOT lower(email) = 'x';

SELECT id FROM users WHERE email NOT LIKE '%x';

-- In the select list, not a filter.
SELECT lower(email) = 'x' FROM users;
