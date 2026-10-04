-- MOD029 filter-defeats-outer-join: WHERE filters the LEFT JOIN's right side

-- Fires.
SELECT t.x, b.label FROM t LEFT JOIN b ON t.x = b.x WHERE b.label = 'one';

SELECT t.x FROM t LEFT OUTER JOIN b AS r ON t.x = r.x WHERE r.x > 0;

SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE b.label IN ('one', 'two');

-- IS NOT NULL keeps exactly the INNER JOIN's rows.
SELECT t.x, b.label FROM t LEFT JOIN b ON t.x = b.x WHERE b.x IS NOT NULL;

SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE t.g = 1 AND b.label IS NOT NULL;

-- Operators and casts pass a NULL through, so these still reject it.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE 0 < b.x::INTEGER + 1;

SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE t.y BETWEEN b.x AND 2;

SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE b.label || '!' = 'one!';

-- Does not fire.
-- IS NULL finds the unmatched rows: the anti-join idiom.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE b.x IS NULL;

-- OR keeps unmatched rows on purpose.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE b.label = 'one' OR b.x IS NULL;

-- coalesce defaults the unmatched side first.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE coalesce(b.label, 'none') <> 'one';

-- The filter is on the preserved side.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE t.x > 1;

-- INNER JOIN: nothing to defeat.
SELECT t.x FROM t JOIN b ON t.x = b.x WHERE b.label = 'one';

-- IS DISTINCT FROM is NULL-aware by design.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE b.label IS DISTINCT FROM 'one';

-- Any function call is a guard: ifnull and concat both turn NULL into a value.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE ifnull(b.label, 'none') <> 'one';

SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE concat(b.label, 'x') = 'x';

-- CASE can map the unmatched row's NULL to a value that passes.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE CASE WHEN b.x IS NULL THEN 1 ELSE b.x END > 0;

-- An IN-list item that is NULL does not stop the test succeeding.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE 1 IN (b.x, 1);

-- Inside a subquery, b names the subquery's own table.
SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE t.x NOT IN (SELECT b.x FROM b WHERE b.label IS NOT NULL);

SELECT t.x FROM t LEFT JOIN b ON t.x = b.x WHERE t.y = (SELECT max(b.x) FROM b);
