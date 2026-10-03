-- MOD036 row-at-a-time-insert: ten or more consecutive single-row INSERTs
-- into one table. Executed: 10,000 of them took 5.1 s; the same rows as one
-- multi-row INSERT took 0.10 s.

-- Fires: ten in a row into the same table.
INSERT INTO target VALUES (0, 'name-0');
INSERT INTO target VALUES (1, 'name-1');
INSERT INTO target VALUES (2, 'name-2');
INSERT INTO target VALUES (3, 'name-3');
INSERT INTO target VALUES (4, 'name-4');
INSERT INTO target VALUES (5, 'name-5');
INSERT INTO target VALUES (6, 'name-6');
INSERT INTO target VALUES (7, 'name-7');
INSERT INTO target VALUES (8, 'name-8');
INSERT INTO target VALUES (9, 'name-9');

-- Does not fire: nine in a row.
INSERT INTO staging VALUES (0, 'name-0');
INSERT INTO staging VALUES (1, 'name-1');
INSERT INTO staging VALUES (2, 'name-2');
INSERT INTO staging VALUES (3, 'name-3');
INSERT INTO staging VALUES (4, 'name-4');
INSERT INTO staging VALUES (5, 'name-5');
INSERT INTO staging VALUES (6, 'name-6');
INSERT INTO staging VALUES (7, 'name-7');
INSERT INTO staging VALUES (8, 'name-8');

-- Does not fire: an insert into another table breaks the run.
INSERT INTO target VALUES (100, 'name-100');
INSERT INTO target VALUES (101, 'name-101');
INSERT INTO target VALUES (102, 'name-102');
INSERT INTO target VALUES (103, 'name-103');
INSERT INTO target VALUES (104, 'name-104');
INSERT INTO target VALUES (105, 'name-105');
INSERT INTO other VALUES (0, 'x');
INSERT INTO target VALUES (200, 'name-200');
INSERT INTO target VALUES (201, 'name-201');
INSERT INTO target VALUES (202, 'name-202');
INSERT INTO target VALUES (203, 'name-203');
INSERT INTO target VALUES (204, 'name-204');
INSERT INTO target VALUES (205, 'name-205');

-- Does not fire: already one multi-row INSERT.
INSERT INTO target VALUES (1, 'a'), (2, 'b'), (3, 'c');
