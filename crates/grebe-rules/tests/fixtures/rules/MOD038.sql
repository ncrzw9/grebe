-- MOD038 sample-before-where: USING SAMPLE n ROWS is written after WHERE but
-- samples before it. Executed: WHERE v = 1 USING SAMPLE 1000 ROWS returned 9
-- rows, not 1,000.

-- Fires.
SELECT * FROM t WHERE x = 1 USING SAMPLE 100 ROWS;

SELECT g, x FROM t WHERE status = 'shipped' AND g = 2 USING SAMPLE reservoir(50 ROWS);

-- A count with no unit is a row count too.
SELECT * FROM t WHERE x = 1 USING SAMPLE 100;

SELECT * FROM t WHERE x = 1 USING SAMPLE reservoir(100);

SELECT * FROM t WHERE x = 1 USING SAMPLE 100 (reservoir, 42);

-- Does not fire.
-- A percentage keeps the same fraction whether taken before or after WHERE.
SELECT avg(x) FROM t WHERE g = 1 USING SAMPLE 10%;

SELECT avg(x) FROM t WHERE g = 1 USING SAMPLE bernoulli(10 PERCENT);

-- No WHERE: nothing to sample before.
SELECT * FROM t USING SAMPLE 100 ROWS;

-- TABLESAMPLE is attached to the table and reads as what it does.
SELECT * FROM t TABLESAMPLE reservoir(100 ROWS) WHERE x = 1;
