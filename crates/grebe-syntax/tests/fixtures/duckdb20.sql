-- DuckDB 2.0 feature samples, transcribed verbatim from the release
-- highlights post: https://duckdb.org/2026/08/17/duckdb-20-highlights
--
-- These matter because the vendored grammar is the v2.0.0 grammar (d54cbbe)
-- while the dogfood corpus (TPC-H/TPC-DS queries and examples mined from
-- DuckDB's 1.x-era documentation) exercises no 2.0-only syntax. Every
-- statement here should parse, except those marked `-- expect: REJECT`.
-- Every verdict in this file, either way, was checked against the v2.0.0
-- engine's own parser.
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
-- The spelling the highlights post announced is not the one that shipped:
-- v2.0.0 rejects these three with ParserException, and so do we. The
-- syntax that did ship (`WITH PREFIX ... USING PUBLIC KEY`, `LOAD x FROM
-- repo`) is in the next section.
SET allow_extension_repositories = 'allowed';
-- expect: REJECT
CREATE EXTENSION REPOSITORY my_repo FROM 'https://extensions.example.org';
INSTALL my_ext FROM my_repo;
-- expect: REJECT
LOAD my_repo/my_ext;
-- expect: REJECT
CREATE EXTENSION REPOSITORY my_repo FROM 's3://my-bucket/extensions'
    USING PUBLIC KEY '-----BEGIN PUBLIC KEY----- ...';


-- ---------------------------------------------------------------------------
-- DuckDB 2.0 GA. Statements from DuckDB's own test suite at tag v2.0.0 that
-- the pre-release snapshot of the grammar could not parse. Each was run
-- through the v2.0.0 engine's parser.
-- ---------------------------------------------------------------------------
-- extension repositories, as shipped
CREATE EXTENSION REPOSITORY acme WITH PREFIX 'http://foo.com' USING PUBLIC KEY 'not a key';
LOAD some_extension FROM acme;
DROP EXTENSION REPOSITORY acme;
-- external resources
ATTACH TO NEW TEMPORARY EXTERNAL RESOURCE 'quack@local' AS qk;
ATTACH TO NEW TEMPORARY EXTERNAL RESOURCE 'quack@local' (INSTANCE 'r7i.16xlarge', REGION 'eu-west-1', SPOT, ANYTHING 'else') AS qk2;
ATTACH TO EXTERNAL RESOURCE shared_res AS qk3;
CONNECT TO NEW TEMPORARY EXTERNAL RESOURCE 'quack@local';
-- nested schemas: more than catalog.schema.table
CREATE SCHEMA s1.s2.s3.s4;
CREATE TABLE s1.s2.s3.tbl(i INTEGER, s STRUCT(a INTEGER));
SELECT memory.s1.s2.s3.tbl.s.a FROM s1.s2.s3.tbl;
UPDATE memory.s1.s2.s3.tbl SET i = i + 1;
CREATE MACRO s1.s2.s3.dbl(a) AS a * 2;
SELECT s1.s2.s3.dbl(21), memory.s1.s2.s3.dbl(10);
CREATE TYPE s1.s2.s3.mood AS ENUM ('happy');
select cast(1 as memory.main.bla.text);
-- schema options
ALTER SCHEMA s1 SET (foo='baz', buzz='wizz');
ALTER SCHEMA s1 RESET (foo, buzz);
CREATE OR REPLACE SCHEMA s1 WITH (location='s3://bucket/folder');
-- backtick-quoted identifiers
SELECT `t`.`a col`, `t`.`b col` FROM `my table` AS `t` ORDER BY ALL;
SELECT "a`b" FROM (SELECT 1 AS `a``b`);
SELECT `from`.`select` FROM (SELECT 5 AS `select`) `from`;
UPDATE `my table` SET `b col` = 'ONE' WHERE `a col` = 1;
-- operators are any run of operator characters
SELECT [1.0, 2.0] <-> [2.0, 4.0];
SELECT ([0.0, 0.0] <-> [3.0, 4.0]) + 1;
SELECT @ -5 + -6;
SELECT 8//**/2;
SELECT 2 = SOME(SELECT 1 UNION ALL SELECT 2);
-- IS, IN and NOT in the comparison chain
SELECT 10 IS NOT NULL::VARCHAR;
SELECT 1 NOT IN (1, 2)::INT, 1 IN (1)::INT::VARCHAR::INT;
SELECT 3 IN (1, 2) IN (FALSE);
SELECT false < NOT true IS NULL = true;
SELECT NULL ISNULL = TRUE;
SELECT 5 NOT BETWEEN ASYMMETRIC 1 AND 10;
-- FETCH FIRST, with and without a count, and before OFFSET
SELECT * FROM range(10) FETCH FIRST 3 ROWS ONLY;
SELECT * FROM range(10) FETCH FIRST ROW ONLY;
SELECT * FROM range(10) FETCH NEXT 2 ROWS ONLY OFFSET 1;
-- UNNEST in GROUP BY
SELECT unnest(l) AS x, count(*) FROM (SELECT [1, 2, 2] AS l) GROUP BY unnest(l);
-- aliases and samples on more table expressions
SELECT v.col0 FROM v: VALUES (42) ORDER BY v.col0;
SELECT t.a FROM t: (SELECT 42 AS a) ORDER BY t.a;
SELECT count(*) FROM (SELECT * FROM range(100)) TABLESAMPLE 10 ROWS;
-- the rest
ALTER TABLE t ALTER COLUMN id TYPE VARCHAR COLLATE nocase USING 'x' || id::VARCHAR;
UPDATE t1 SET (a, b) = (1, 2), a = 3;
INSERT INTO strings OVERRIDING SYSTEM VALUE VALUES (1);
CREATE INDEX i1 ON t1(foo) WITH (deferrable);
CREATE TYPE enum_list AS ENUM ('sad', 'ok', 'happy')[];
TRUNCATE a AS ignored_alias RESTRICT;
DROP MACRO TABLE db2.s2.tm;
PIVOT Cities ON Year USING SUM(Population) GROUP BY (Country);
COPY copy_file_name_compressed FROM copy_file_name_auto.csv.gz;
SELECT 1 a$c;
SELECT DATE '2020-'
'01-01'::VARCHAR;
-- A select-list alias without AS must be a bare-label word: these keywords
-- are unreserved, and still not aliases.
SELECT a AS month, b AS filter, a "over" FROM t ORDER BY a;
-- expect: REJECT
SELECT count(*) filter FROM t;
-- expect: REJECT
SELECT DATE '2020-01-01' + 1 month;
-- expect: REJECT
SELECT * exclude FROM t;
-- What the lexer and the literal matchers refuse.
-- expect: REJECT
SELECT 1.2.3;
-- expect: REJECT
SELECT 1..2;
-- expect: REJECT
SELECT "";
-- expect: REJECT
CREATE TABLE s1.""(i INTEGER);
-- expect: REJECT
SELECT 'one' /* comment
*/ 'two';
-- expect: REJECT
SELECT $tag$a$tag$
'b';
