-- MOD035 wrapped-date-filter: a WHERE that wraps the column defeats
-- row-group pruning; compare the column itself to a range.

-- Fires: extracting a part, any comparison.
SELECT count(*) FROM events WHERE year(ts) = 2023;

SELECT count(*) FROM events WHERE year(ts) = 2023 AND month(ts) = 3;

SELECT count(*) FROM events WHERE date_part('year', ts) >= 2023;

SELECT count(*) FROM events WHERE extract(year FROM ts) = 2023;

SELECT count(*) FROM events WHERE year(e.ts) BETWEEN 2022 AND 2023;

SELECT count(*) FROM events WHERE month(ts) IN (1, 2, 3);

-- Fires: formatting or converting the column.
SELECT count(*) FROM events WHERE strftime(ts, '%Y-%m') = '2023-03';

SELECT count(*) FROM events WHERE epoch(ts) >= 1677628800;

-- Fires: a cast to DATE with anything but equality.
SELECT count(*) FROM events WHERE ts::DATE >= DATE '2023-03-01' AND ts::DATE < DATE '2023-04-01';

SELECT count(*) FROM events WHERE CAST(ts AS DATE) BETWEEN DATE '2023-01-01' AND DATE '2023-12-31';

SELECT count(*) FROM events WHERE ts::DATE IN (DATE '2023-03-15', DATE '2023-03-16');

-- Does not fire: DuckDB prunes these as well as it prunes a range.
SELECT count(*) FROM events WHERE ts >= TIMESTAMP '2023-01-01' AND ts < TIMESTAMP '2024-01-01';

SELECT count(*) FROM events WHERE date_trunc('month', ts) = TIMESTAMP '2023-03-01';

SELECT count(*) FROM events WHERE date_trunc('year', ts) BETWEEN TIMESTAMP '2022-01-01' AND TIMESTAMP '2023-01-01';

SELECT count(*) FROM events WHERE ts::DATE = DATE '2023-03-15';

SELECT count(*) FROM events WHERE CAST(ts AS DATE) = DATE '2023-03-15';

-- Does not fire: not a filter against constants.
-- Both sides read columns: a join-like comparison, nothing to prune by.
SELECT count(*) FROM events WHERE year(ts) = year(created);

-- In the select list, not the WHERE.
SELECT year(ts) = 2023 FROM events;

-- A function this rule does not know is left alone.
SELECT count(*) FROM events WHERE my_udf(ts) = 2023;

-- The DATE cast is on a constant; the column is not cast.
SELECT count(*) FROM events WHERE ts - '2023-01-01'::DATE > INTERVAL 1 DAY;

-- Cast to another type.
SELECT count(*) FROM events WHERE ts::VARCHAR >= '2023';

-- The WHERE of a subquery is checked, the subquery's select list is not.
SELECT count(*) FROM events WHERE id IN (SELECT id FROM other WHERE x > 1);
