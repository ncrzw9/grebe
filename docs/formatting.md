# Formatting

`grebe format` rewrites SQL files in place to one layout. It changes
whitespace, line breaks and the case of keywords, functions and types, and
adds a missing final semicolon. Names, literals, comments and the order of
things stay as written.

Every example below is run by `cargo test` (see
[README](README.md#how-the-examples-are-kept-exact)): the first block is the
file, the second is the file after the command.

## The default

<!-- example: grebe format query.sql -->

```sql
select o.id, c.name, sum(o.total) as spent from orders o join customers c on c.id = o.customer_id where o.status = 'shipped' and o.placed >= date '2024-01-01' group by o.id, c.name;
```

```sql
SELECT o.id, c.name, sum(o.total) AS spent
FROM orders o
JOIN customers c ON c.id = o.customer_id
WHERE o.status = 'shipped' AND o.placed >= date '2024-01-01'
GROUP BY o.id, c.name;
```

Keywords go upper case, functions and types lower case. Each clause starts a
line once the statement does not fit in 100 columns. Inside a clause, a list
that does not fit goes one item per line, indented four spaces:

<!-- example: grebe format query.sql -->

```sql
select customer_id, count(*) as orders, sum(total) as revenue, min(placed) as first_order, max(placed) as last_order from orders group by customer_id;
```

```sql
SELECT
    customer_id,
    count(*) AS orders,
    sum(total) AS revenue,
    min(placed) AS first_order,
    max(placed) AS last_order
FROM orders
GROUP BY customer_id;
```

Comments stay with the line they were on:

<!-- example: grebe format query.sql -->

```sql
-- Top customers this year.
select id, name -- the display name
from customers
where signed_up >= date '2024-01-01';
```

```sql
-- Top customers this year.
SELECT
    id,
    name -- the display name
FROM customers
WHERE signed_up >= date '2024-01-01';
```

A statement that does not parse is left exactly as written, and the
statements around it are formatted:

<!-- example: grebe format query.sql -->

```sql
select id from orders where;
select   id from   orders;
```

```sql
select id from orders where;

SELECT id FROM orders;
```

## Options

Set options in a `[format]` table of `grebe.toml` (or `[tool.grebe.format]`
in `pyproject.toml`). grebe finds the file by walking up from the first path
it is given. Each section below shows the table, the input and the result.

### indent_size

Spaces per level, 1 to 16. Default 4.

<!-- example: grebe format query.sql -->

```toml
[format]
indent_size = 2
```

```sql
select customer_id, count(*) as orders, sum(total) as revenue, min(placed) as first_order, max(placed) as last_order from orders group by customer_id;
```

```sql
SELECT
  customer_id,
  count(*) AS orders,
  sum(total) AS revenue,
  min(placed) AS first_order,
  max(placed) AS last_order
FROM orders
GROUP BY customer_id;
```

### inline_threshold

A construct that fits in this many columns stays on one line. 20 to 1000.
Default 100.

<!-- example: grebe format query.sql -->

```toml
[format]
inline_threshold = 40
```

```sql
select id, name, email from customers where id = 1;
```

```sql
SELECT id, name, email
FROM customers
WHERE id = 1;
```

### keyword_case

`"upper"` (default) or `"lower"`.

<!-- example: grebe format query.sql -->

```toml
[format]
keyword_case = "lower"
```

```sql
SELECT id FROM customers WHERE name IS NOT NULL ORDER BY id;
```

```sql
select id from customers where name is not null order by id;
```

### function_case

`"lower"` (default), `"upper"` or `"preserve"`. It covers functions,
aggregates, window and table functions. `CAST`, `COALESCE` and the other
keyword forms that read as calls follow `keyword_case`.

<!-- example: grebe format query.sql -->

```toml
[format]
function_case = "upper"
```

```sql
select count(*), Max(total), coalesce(status, 'none') from orders;
```

```sql
SELECT COUNT(*), MAX(total), COALESCE(status, 'none') FROM orders;
```

<!-- example: grebe format query.sql -->

```toml
[format]
function_case = "preserve"
```

```sql
select count(*), Max(total), coalesce(status, 'none') from orders;
```

```sql
SELECT count(*), Max(total), COALESCE(status, 'none') FROM orders;
```

### type_case

`"lower"` (default), `"upper"` or `"preserve"`. It covers a type wherever it
stands: a cast, `::`, a column definition, a typed literal.

<!-- example: grebe format query.sql -->

```toml
[format]
type_case = "upper"
```

```sql
create table events (id bigint, happened timestamp with time zone, amount decimal(18, 2));
select id::varchar, cast(amount as double) from events where happened > timestamp '2024-01-01';
```

```sql
CREATE TABLE events (id BIGINT, happened TIMESTAMP WITH TIME ZONE, amount DECIMAL(18, 2));

SELECT id::VARCHAR, CAST(amount AS DOUBLE) FROM events WHERE happened > TIMESTAMP '2024-01-01';
```

### function_paren_space

`false` (default) or `true`: a space between a call's name and its
parenthesis. A type's modifiers always hug: `decimal(18, 2)`.

<!-- example: grebe format query.sql -->

```toml
[format]
function_paren_space = true
```

```sql
select count(*), coalesce(a, b), cast(c as decimal(18, 2)) from t;
```

```sql
SELECT count (*), COALESCE (a, b), CAST (c AS decimal(18, 2)) FROM t;
```

### keyword_paren_space

`true` (default) or `false`: the space between a keyword and the parenthesis
after it (`IN`, `VALUES`, `OVER`, `AS`, `EXISTS`, `USING`, `FILTER`, ...).
A parenthesis after a name, as in `INSERT INTO t (a, b)`, is not affected.

<!-- example: grebe format query.sql -->

```toml
[format]
keyword_paren_space = false
```

```sql
with recent as (select * from orders where id in (1, 2, 3))
select row_number() over (order by id) from recent;
insert into t (a, b) values (1, 2);
```

```sql
WITH recent AS(
    SELECT *
    FROM orders
    WHERE id IN(1, 2, 3)
)

SELECT row_number() OVER(ORDER BY id)
FROM recent;

INSERT INTO t (a, b) VALUES(1, 2);
```

### semicolon

`"attached"` (default), `"spaced"` or `"newline"`.

<!-- example: grebe format query.sql -->

```toml
[format]
semicolon = "spaced"
```

```sql
select 1;
select 2;
```

```sql
SELECT 1 ;

SELECT 2 ;
```

<!-- example: grebe format query.sql -->

```toml
[format]
semicolon = "newline"
```

```sql
select 1;
select 2;
```

```sql
SELECT 1
;

SELECT 2
;
```

### comma

`"trailing"` (default) or `"leading"`: where the comma goes in a list that
breaks over lines. A list that fits on one line is `a, b` either way.

<!-- example: grebe format query.sql -->

```toml
[format]
comma = "leading"
```

```sql
select customer_id, count(*) as orders, sum(total) as revenue, min(placed) as first_order, max(placed) as last_order from orders group by customer_id;
```

```sql
SELECT
    customer_id
    , count(*) AS orders
    , sum(total) AS revenue
    , min(placed) AS first_order
    , max(placed) AS last_order
FROM orders
GROUP BY customer_id;
```

A `--` comment after an item stays at the end of that item's line:

<!-- example: grebe format query.sql -->

```toml
[format]
comma = "leading"
list_layout = "expanded"
```

```sql
select id, -- the key
    name
from customers;
```

```sql
SELECT
    id -- the key
    , name
FROM customers;
```

### logical_operator

`"leading"` (default) or `"trailing"`: whether `AND` and `OR` start a line
of a condition that breaks, or end the line before.

<!-- example: grebe format query.sql -->

```sql
select id from orders where status = 'shipped' and placed >= date '2024-01-01' and placed < date '2025-01-01' and (total > 100 or customer_id = 42);
```

```sql
SELECT id
FROM orders
WHERE status = 'shipped'
    AND placed >= date '2024-01-01'
    AND placed < date '2025-01-01'
    AND (total > 100 OR customer_id = 42);
```

<!-- example: grebe format query.sql -->

```toml
[format]
logical_operator = "trailing"
```

```sql
select id from orders where status = 'shipped' and placed >= date '2024-01-01' and placed < date '2025-01-01' and (total > 100 or customer_id = 42);
```

```sql
SELECT id
FROM orders
WHERE status = 'shipped' AND
    placed >= date '2024-01-01' AND
    placed < date '2025-01-01' AND
    (total > 100 OR customer_id = 42);
```

### indent_style

`"space"` (default) or `"tab"`. A tab counts as `indent_size` columns
towards `inline_threshold`, so lines break where they would with spaces.

<!-- example: grebe format query.sql -->

```toml
[format]
indent_style = "tab"
```

```sql
select customer_id, count(*) as orders, sum(total) as revenue, min(placed) as first_order, max(placed) as last_order from orders group by customer_id;
```

```sql
SELECT
	customer_id,
	count(*) AS orders,
	sum(total) AS revenue,
	min(placed) AS first_order,
	max(placed) AS last_order
FROM orders
GROUP BY customer_id;
```

### blank_lines_between_statements

0 to 10. Default 1.

<!-- example: grebe format query.sql -->

```toml
[format]
blank_lines_between_statements = 0
```

```sql
select 1;


select 2;
```

```sql
SELECT 1;
SELECT 2;
```

### max_blank_lines_in_statement

0 to 10. Default 0. Keeps up to this many blank lines in a row where the
author left them inside a statement, at a place the line breaks anyway.

<!-- example: grebe format query.sql -->

```toml
[format]
max_blank_lines_in_statement = 1
```

```sql
with paid as (select order_id from payments),

open as (select id from orders where id not in (select order_id from paid))

select count(*) from open;
```

```sql
WITH paid AS (
    SELECT order_id
    FROM payments
),

open AS (
    SELECT id
    FROM orders
    WHERE id NOT IN (SELECT order_id FROM paid)
)

SELECT count(*)
FROM open;
```

The same input with the default drops them:

<!-- example: grebe format query.sql -->

```sql
with paid as (select order_id from payments),

open as (select id from orders where id not in (select order_id from paid))

select count(*) from open;
```

```sql
WITH paid AS (
    SELECT order_id
    FROM payments
),
open AS (
    SELECT id
    FROM orders
    WHERE id NOT IN (SELECT order_id FROM paid)
)

SELECT count(*)
FROM open;
```

### list_layout

`"auto"` (default) or `"expanded"`: a select list, `GROUP BY` or `ORDER BY`
list of more than one item goes one item per line, even when it fits.

<!-- example: grebe format query.sql -->

```toml
[format]
list_layout = "expanded"
```

```sql
select id, name from customers order by name, id;
```

```sql
SELECT
    id,
    name
FROM customers
ORDER BY
    name,
    id;
```

### case_layout

`"auto"` (default) or `"expanded"`: a `CASE` with more than one `WHEN` puts
each arm on a line of its own, even when it fits.

<!-- example: grebe format query.sql -->

```toml
[format]
case_layout = "expanded"
```

```sql
select case when total > 100 then 'large' when total > 10 then 'medium' else 'small' end as size from orders;
```

```sql
SELECT
    CASE
        WHEN total > 100 THEN 'large'
        WHEN total > 10 THEN 'medium'
        ELSE 'small'
    END AS size
FROM orders;
```

## Styles

`style` names a bundle of options. An option set next to it wins.

### style = "dbt"

Lower-case keywords, functions and types, trailing commas, four spaces, one
select item per line.

<!-- example: grebe format query.sql -->

```toml
[format]
style = "dbt"
```

```sql
SELECT o.id, c.name, SUM(o.total) AS spent FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY o.id, c.name;
```

```sql
select
    o.id,
    c.name,
    sum(o.total) as spent
from orders o
join customers c on c.id = o.customer_id
group by
    o.id,
    c.name;
```

### style = "mazur"

The same seven options with the same values today.

<!-- example: grebe format query.sql -->

```toml
[format]
style = "mazur"
```

```sql
SELECT o.id, c.name, SUM(o.total) AS spent FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY o.id, c.name;
```

```sql
select
    o.id,
    c.name,
    sum(o.total) as spent
from orders o
join customers c on c.id = o.customer_id
group by
    o.id,
    c.name;
```

### An option next to a style

<!-- example: grebe format query.sql -->

```toml
[format]
style = "dbt"
keyword_case = "upper"
comma = "leading"
```

```sql
SELECT o.id, c.name, SUM(o.total) AS spent FROM orders o JOIN customers c ON c.id = o.customer_id GROUP BY o.id, c.name;
```

```sql
SELECT
    o.id
    , c.name
    , sum(o.total) AS spent
FROM orders o
JOIN customers c ON c.id = o.customer_id
GROUP BY
    o.id
    , c.name;
```

## Leaving SQL as written

Everything between `-- grebe: format off` and `-- grebe: format on`, each on
a line of its own, is kept byte for byte. `-- fmt: off` and `-- fmt: on` work
too. The statements around the region are formatted.

<!-- example: grebe format query.sql -->

```sql
select   1;
-- grebe: format off
SELECT  id,    name
FROM    customers ;
-- grebe: format on
select   2;
```

```sql
SELECT 1;
-- grebe: format off
SELECT  id,    name
FROM    customers ;
-- grebe: format on
SELECT 2;
```

A marker inside a statement keeps that whole statement as written.

## Checking without writing

`--check` writes nothing. It names each file that would change and exits 1
if any would, 0 if none would.

<!-- example: grebe format --check query.sql -->

```sql
select 1
```

```text
would reformat query.sql

1 file(s) would be reformatted, 0 unchanged
```

<!-- example: grebe format --check query.sql -->

```sql
SELECT 1;
```

```text

0 file(s) would be reformatted, 1 unchanged
```

## stdin

A single `-` reads stdin and writes the result to stdout, and nothing else.

<!-- example: grebe format - -->

```sql
select id,name from customers
```

```sql
SELECT id, name FROM customers;
```

Config is found from the current directory. `--config PATH` reads a given
file and `--no-config` reads none.
