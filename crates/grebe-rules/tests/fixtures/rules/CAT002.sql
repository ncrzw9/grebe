-- CAT002 unknown-column: `alias.column` where the alias is one catalog table
-- (catalog.json here) and that table has no such column. Unqualified columns
-- are not checked: without binding, a bare name can be many things.

-- Fires.
SELECT o.totl FROM orders o;

SELECT orders.nope FROM orders;

SELECT o.id, c.nme FROM orders AS o JOIN customers AS c ON o.customer_id = c.cust_id;

-- In any clause.
SELECT o.id FROM orders o WHERE o.status = 'open' GROUP BY o.id, o.region HAVING max(o.amount) > 1 ORDER BY o.created;

-- A view has columns too.
SELECT b.customer_id FROM big_orders b;

-- A table named with its schema or database.
SELECT e.tss FROM raw.events e;

SELECT k.uid FROM lake.main.clicks k;

-- The first two parts of a longer chain: the column, then a struct field.
SELECT o.metadata.src FROM orders o;

-- DML targets and their aliases.
UPDATE customers c SET name = 'x' WHERE c.idd = 1;

DELETE FROM orders USING customers WHERE orders.customer_id = customers.cid;

INSERT INTO customers AS t VALUES (1, 'a', NULL) ON CONFLICT DO UPDATE SET name = t.nam;

MERGE INTO orders AS m USING customers c ON m.customer = c.id WHEN MATCHED THEN DELETE;

-- The same alias for the same table in two scopes is still one table.
SELECT o.id FROM orders o WHERE EXISTS (SELECT 1 FROM orders o WHERE o.missing = 1);

-- Does not fire.
-- Columns the table has, in any case, quoted or not.
SELECT o.id, o.TOTAL, o."Placed", c."Sign Up", c."sign up", o.rowid FROM orders o JOIN customers c ON o.customer_id = c.id;

-- A struct field of a real column.
SELECT o.meta.src, o.meta.anything FROM orders o;

-- Unqualified, or a star.
SELECT nope, o.*, COLUMNS('^t') FROM orders o;

-- The alias is something whose columns the catalog cannot give.
WITH c AS (SELECT 1 AS made_up) SELECT c.made_up FROM c;

SELECT s.one, f.column0, t.i, v.a FROM (SELECT 1 AS one) s, read_csv('f.csv') f, range(3) t(i), (VALUES (1)) v(a);

SELECT range.range, generate_series.generate_series FROM range(3), generate_series(1, 3);

SELECT p.anything FROM 'orders.parquet' p;

SELECT w.x FROM warehouse.main.facts w;

-- Columns renamed in the alias.
SELECT a.x, a.y FROM customers AS a(x, y);

-- A table this file creates or alters: its columns are the file's business.
CREATE TABLE staging AS SELECT 1 AS made_up;

SELECT s.made_up FROM staging s;

ALTER TABLE notes ADD COLUMN extra INT;

SELECT n.extra FROM notes n;

-- One alias, two different relations: which one a reference means is scoping.
SELECT o.id FROM orders o WHERE EXISTS (SELECT 1 FROM customers o WHERE o.name = 'a');

-- An alias only: the table's own name is no longer in scope, so `orders`
-- here is not this rule's to judge.
SELECT orders.whatever FROM orders o;

-- `o` is also a struct column of another table in the statement.
SELECT o.id2 FROM orders o, audit;

-- A lambda parameter or a select alias named like a table alias.
SELECT list_transform([{'k': 1}], o -> o.k) FROM orders o;

SELECT list_transform([{'k': 1}], lambda c: c.k) FROM customers c;

SELECT {'k': 1} AS c, c.k FROM customers c;

-- PIVOT and UNPIVOT make columns of their own.
SELECT p.anything FROM orders PIVOT (sum(total) FOR placed IN ('2024-01-01')) p;

-- Not a relation of the statement at all.
INSERT INTO customers VALUES (1, 'a', NULL) ON CONFLICT DO UPDATE SET name = excluded.name;

-- A macro's parameters.
CREATE MACRO pick(o) AS o.anything;

-- Silenced like any other finding.
-- grebe: ignore[CAT002]
SELECT o.not_there_on_purpose FROM orders o;
