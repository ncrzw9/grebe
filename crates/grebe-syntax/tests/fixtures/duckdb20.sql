-- DuckDB 2.0 feature samples, transcribed verbatim from the release
-- highlights post: https://duckdb.org/2026/08/17/duckdb-20-highlights
--
-- These matter because the vendored grammar IS a 2.0 grammar (ce512b8) while
-- the dogfood corpus (TPC-H/TPC-DS queries and examples mined from DuckDB's
-- 1.x-era documentation) exercises no 2.0-only syntax. Every statement here
-- should parse, except the three marked `-- expect: REJECT`.
--
-- server: quack_serve
CALL quack_serve(token = 'my_token');
-- server: attach/connect/disconnect
ATTACH 'quack:server.example.com' AS qk (TOKEN 'my_token');
CONNECT qk;
SELECT count(*) FROM events;
DISCONNECT;
-- server: remote pushdown
CONNECT 'postgres://localhost/mydb';
SELECT count(*) FROM orders;
DISCONNECT;
-- variant
CREATE TABLE events (payload VARIANT);
INSERT INTO events VALUES ('{"user": {"id": 42, "tags": ["a", "b"]}}'::JSON::VARIANT);
SELECT variant_type(payload), variant_keys(payload) FROM events;
SELECT * FROM events WHERE variant_contains(payload, {'user': {'id': 42}}::VARIANT);
-- triggers
CREATE TABLE target (id INTEGER, val INTEGER);
CREATE TABLE audit (id INTEGER, old_val INTEGER, new_val INTEGER);
CREATE TRIGGER trg_audit AFTER UPDATE ON target
REFERENCING OLD TABLE AS o NEW TABLE AS n
FOR EACH STATEMENT
    INSERT INTO audit
    SELECT n.id, o.val, n.val
    FROM o
    JOIN n ON o.id = n.id;
INSERT INTO target VALUES (1, 10), (2, 20);
UPDATE target SET val = val * 10 WHERE id <= 2;
SELECT * FROM audit;
-- nearest join
SELECT q.user_id, t.product_id
FROM users q
    INNER JOIN products t APPROX NEAREST 2
    BY SIMILARITY array_cosine_similarity(q.embedding, t.embedding);
-- dml inside cte
WITH moved AS MATERIALIZED (DELETE FROM staging RETURNING *)
INSERT INTO archive SELECT * FROM moved;
-- nested schemas
CREATE SCHEMA aviary;
CREATE SCHEMA aviary.wetlands;
CREATE TABLE aviary.wetlands.counts (wingspan DECIMAL);
-- variables
SET VARIABLE threshold = 100;
SELECT * FROM orders WHERE amount > $threshold;
-- json mutation
SELECT json_set('{"a":1}', '$.b', '2');
-- recursive cte USING KEY
WITH RECURSIVE tbl(a, b) USING KEY (a, avg(b)) AS (
    SELECT 1, 5
    UNION ALL
    SELECT a, b - 1 FROM tbl WHERE b > 0
)
TABLE tbl;
-- row-group pruning
SELECT * FROM logs WHERE contains(message, 'ERROR');
SELECT * FROM t WHERE substr(code, 1, 3) = 'NL-';
SELECT * FROM 'data/*.parquet' WHERE id IN (1, 5, 9);
-- recursive cte benchmark
CREATE TABLE edges AS
    SELECT (range % 100_000)::INTEGER AS src,
           ((range * 13 + 7) % 100_000)::INTEGER AS dst
    FROM range(1_000_000);
WITH RECURSIVE reachable(node) AS (
    SELECT 0
    UNION
    SELECT dst FROM edges, reachable WHERE src = node
)
SELECT count(*) FROM reachable;
-- timezones and collations
SELECT '2026-08-14 12:00:00'::TIMESTAMPTZ AT TIME ZONE 'Europe/Paris';
SELECT * FROM names ORDER BY name COLLATE de;
-- extension usage
LOAD add_numbers;
SELECT add_numbers(40, 2);
SELECT add_numbers(40);
SELECT add_numbers(b := 2, a := 40);
SELECT add_numbers(40, NULL);
-- custom extension repositories
-- NOT YET SHIPPED: the blog announces these, but engine v1.6.0-dev12831
-- rejects all three with ParserException and the vendored grammar has no
-- REPOSITORY production at all. We agree with the engine by rejecting them.
-- Re-check at 2.0 GA; if the engine starts accepting, this is an
-- engine-only accept, which the differential gate forbids.
SET allow_extension_repositories = 'allowed';
-- expect: REJECT
CREATE EXTENSION REPOSITORY my_repo FROM 'https://extensions.example.org';
INSTALL my_ext FROM my_repo;
-- expect: REJECT
LOAD my_repo/my_ext;
-- expect: REJECT
CREATE EXTENSION REPOSITORY my_repo FROM 's3://my-bucket/extensions'
    USING PUBLIC KEY '-----BEGIN PUBLIC KEY----- ...';
