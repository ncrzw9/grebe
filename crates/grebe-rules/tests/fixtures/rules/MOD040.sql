-- MOD040 csv-full-sniff: sample_size = -1 makes the CSV sniffer read the
-- whole file to guess types before the query starts. Executed: 15x slower
-- on 5M rows. Declaring the types skips the guessing.

-- Fires.
SELECT * FROM read_csv('events.csv', sample_size = -1);

SELECT count(*) FROM read_csv_auto('events.csv', header = true, SAMPLE_SIZE = -1);

CREATE TABLE ev AS SELECT * FROM read_csv('events/*.csv', sample_size := -1);

-- Does not fire.
-- A bounded sample.
SELECT * FROM read_csv('events.csv', sample_size = 100000);

-- Types declared: nothing to sniff.
SELECT * FROM read_csv('events.csv', columns = {'id': 'BIGINT', 'v': 'BIGINT'}, auto_detect = false);

-- Not a CSV reader.
SELECT * FROM read_parquet('events.parquet');
