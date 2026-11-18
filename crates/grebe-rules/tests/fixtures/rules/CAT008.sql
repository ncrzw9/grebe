-- CAT008 type-mismatch: a comparison of two catalog columns whose types are
-- of different kinds (text, number, date or time, boolean).

-- Fires.
SELECT 1 FROM users u JOIN teams t ON t.name = u.team_id;

SELECT 1 FROM users WHERE created > score;

SELECT 1 FROM users u, payments p WHERE u.active = p.amount;

SELECT 1 FROM payments p JOIN orders o ON p.note <> o.id;

-- Does not fire.
-- The same kind.
SELECT 1 FROM users u JOIN teams t ON t.id = u.team_id;

SELECT 1 FROM orders o JOIN payments p ON p.amount = o.id;

SELECT 1 FROM orders o JOIN users u ON u.created >= o.placed;

-- A literal, an expression, an unresolved column.
SELECT 1 FROM users WHERE email = 42;

SELECT 1 FROM users WHERE created > score::TIMESTAMP;

SELECT 1 FROM users u, (SELECT 'x' AS name) t WHERE t.name = u.id;

-- A type outside the four kinds.
SELECT 1 FROM orders o, raw.events e WHERE e.payload = o.id;
