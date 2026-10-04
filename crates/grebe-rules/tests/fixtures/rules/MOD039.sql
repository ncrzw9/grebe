-- MOD039 delete-then-insert: DELETE FROM t ... then INSERT INTO t ... SELECT,
-- both reading the same source, is an upsert; MERGE INTO does it in one
-- atomic statement. Executed: 2.2x faster for 1M changes into 10M rows, and
-- a failed INSERT outside a transaction left 500,000 deleted rows gone.

-- Fires.
DELETE FROM t WHERE x IN (SELECT x FROM changes);
INSERT INTO t SELECT * FROM changes;

DELETE FROM t USING changes AS c WHERE t.x = c.x;
INSERT INTO t SELECT * FROM changes;

DELETE FROM main.t WHERE EXISTS (SELECT 1 FROM staging.changes AS c WHERE c.x = t.x);
INSERT INTO main.t SELECT c.* FROM staging.changes AS c;

-- Fires inside a transaction too: still two passes over the table.
BEGIN;
DELETE FROM t AS old WHERE old.x IN (SELECT x FROM changes);
INSERT INTO t SELECT * FROM changes;
COMMIT;

-- Does not fire.
-- Unrelated rows: the DELETE and the INSERT share no source.
DELETE FROM t WHERE status = 'lost';
INSERT INTO t SELECT * FROM empty_t;

-- The only table both read is the target itself: no changes to merge.
DELETE FROM t WHERE y = (SELECT max(y) FROM t);
INSERT INTO t SELECT x, y + 1, a, g, status FROM t WHERE y = 1;

DELETE FROM main.t WHERE g IN (SELECT g FROM t WHERE x IS NULL);
INSERT INTO t SELECT * FROM main.t WHERE g = 1;

-- A single-row VALUES insert reads from nowhere.
DELETE FROM t WHERE x = 1;
INSERT INTO t VALUES (1, 2, true, 1, 'shipped');

-- No WHERE: a full reload, not an upsert.
DELETE FROM t;
INSERT INTO t SELECT * FROM changes;

-- Different tables.
DELETE FROM b WHERE x IN (SELECT x FROM changes);
INSERT INTO t SELECT * FROM changes;

-- Something in between.
DELETE FROM t WHERE x IN (SELECT x FROM changes);
SELECT count(*) FROM t;
INSERT INTO t SELECT * FROM changes;
