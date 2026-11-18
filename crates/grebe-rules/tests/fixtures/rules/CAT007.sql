-- CAT007 insert-mismatch: `INSERT ... VALUES` that does not fit the table
-- (catalog.json: customers has id, name, "Sign Up").

-- Fires.
INSERT INTO customers VALUES (1, 'a', DATE '2024-01-01', 'extra');

INSERT INTO customers (id, name) VALUES (1, 'a', DATE '2024-01-01');

INSERT INTO customers (id, name) VALUES (1, 'a'), (2);

INSERT INTO customers (id, nme) VALUES (1, 'a');

INSERT OR REPLACE INTO teams VALUES (1, 'a', 'b') ;

-- Does not fire.
INSERT INTO customers VALUES (1, 'a', DATE '2024-01-01');

INSERT INTO customers (id, name) VALUES (1, 'a'), (2, DEFAULT);

INSERT INTO customers ("ID", "sign up") VALUES (1, NULL);

-- Fewer values than columns: a generated column may make up the difference,
-- and the snapshot does not say which columns are generated.
INSERT INTO customers VALUES (1, 'a');

-- Not checked.
INSERT INTO customers DEFAULT VALUES;

INSERT INTO customers BY NAME SELECT 1 AS id;

INSERT INTO customers SELECT 1, 2, 3, 4;

INSERT INTO big_orders VALUES (1, 2, 3);
