-- CAT001 unknown-table: a table or view the statement reads or writes that
-- the catalog (catalog.json here) does not have. Runs only when a catalog is
-- given. The replacement is the nearest name, offered as a suggestion.

-- Fires.
SELECT * FROM ordrs;

SELECT * FROM orders o JOIN custmers c ON o.customer_id = c.id;

-- A schema or a database the catalog knows, without that table in it.
SELECT * FROM raw.evnts;

SELECT * FROM lake.main.orders;

SELECT * FROM shop.clicks;

-- Writes are checked like reads.
INSERT INTO custmers VALUES (1, 'a', NULL);

UPDATE ordres SET total = 0;

DELETE FROM big_order WHERE id = 1;

MERGE INTO order_ USING customers c ON order_.id = c.id WHEN MATCHED THEN DELETE;

TRUNCATE TABLE customer;

COPY oders FROM 'orders.csv';

TABLE nothing_like_it;

-- In a subquery, a CTE body, a view body.
SELECT * FROM orders WHERE customer_id IN (SELECT id FROM customrs);

WITH recent AS (SELECT * FROM orderz) SELECT * FROM recent;

CREATE VIEW v_missing AS SELECT * FROM evnts;

-- Does not fire.
-- In the catalog, by any qualification the catalog knows, in any case.
SELECT * FROM orders, ORDERS, "Orders", main.orders, shop.orders, shop.main.orders, raw.events, lake.clicks;

-- An unqualified name is looked for in every schema and database: which of
-- them the session searches (USE, search_path) is not modelled, so a table
-- that exists somewhere is never reported.
SELECT * FROM events, clicks;

-- A CTE, a recursive one included.
WITH RECURSIVE walk(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM walk WHERE n < 3) SELECT * FROM walk;

WITH a AS (SELECT 1 AS x), b AS (SELECT * FROM a) SELECT * FROM b JOIN a USING (x);

-- A table or view this file creates, alters or drops, before or after its use.
CREATE TABLE staging AS SELECT * FROM orders;

SELECT * FROM staging JOIN later_on USING (id);

CREATE OR REPLACE TEMP VIEW later_on AS SELECT id FROM customers;

DROP TABLE IF EXISTS scratch;

INSERT INTO scratch SELECT 1;

ALTER TABLE imported RENAME TO renamed;

SELECT * FROM imported, renamed;

-- Files: a replacement scan, not a table.
SELECT * FROM 'data/orders.parquet', "orders.csv", 'https://example.com/a.json';

-- Table functions and what DuckDB builds in.
SELECT * FROM read_csv('f.csv') r, range(3) t(i), unnest([1, 2]) u, duckdb_tables() d;

SELECT * FROM duckdb_tables, pg_tables, sqlite_master, information_schema.tables, pg_catalog.pg_class;

-- A database the catalog does not cover: it cannot say.
SELECT * FROM warehouse.main.facts, other_schema.things;

-- Not a read or a write of the table's rows.
DESCRIBE no_such_table;

SHOW TABLES;

SUMMARIZE no_such_table;

DROP VIEW IF EXISTS no_such_view;

CREATE TABLE fresh (id INT REFERENCES no_such_parent (id));

-- A subquery, VALUES and PIVOT are relations of their own.
SELECT * FROM (SELECT 1 AS one) sub, (VALUES (1, 2)) pairs(a, b);

PIVOT orders ON placed USING sum(total);

-- A macro's body: its parameters are names this rule does not model.
CREATE MACRO from_any(t) AS TABLE SELECT * FROM query_table(t);

-- Silenced like any other finding.
-- grebe: ignore[CAT001]
SELECT * FROM not_there_on_purpose;
