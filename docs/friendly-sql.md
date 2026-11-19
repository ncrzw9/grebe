# Friendly SQL

DuckDB accepts a shorter, plainer SQL than the standard: `FROM` first,
`GROUP BY ALL`, `count()`, `count_if`, `name: expr`. `--select FRIENDLY`
runs the seven rules that rewrite conventional SQL into it, all with a safe
fix, so with `--fix` it rewrites a file without changing what its queries
return. `grebe format` then lays it out.

```sh
grebe check --fix --select FRIENDLY query.sql && grebe format query.sql
```

The seven: MOD001, MOD003, MOD013, MOD022, MOD023, MOD030, MOD041. In VS Code
the same rewrite is the command **grebe: Rewrite as Friendly SQL**.

## Each rewrite alone

`count(*)` is `count()` (MOD001):

<!-- example: grebe check --fix --select MOD001 query.sql -->

```sql
select count(*) from orders;
```

```sql
select count() from orders;
```

A `GROUP BY` that repeats the select list is `GROUP BY ALL` (MOD003):

<!-- example: grebe check --fix --select MOD003 query.sql -->

```sql
select customer_id, status, sum(total) from orders group by customer_id, status;
```

```sql
select customer_id, status, sum(total) from orders GROUP BY ALL;
```

An `ORDER BY` that repeats the whole select list is `ORDER BY ALL` (MOD013):

<!-- example: grebe check --fix --select MOD013 query.sql -->

```sql
select status, customer_id from orders order by status, customer_id;
```

```sql
select status, customer_id from orders ORDER BY ALL;
```

`SELECT * FROM t` is `FROM t` (MOD022):

<!-- example: grebe check --fix --select MOD022 query.sql -->

```sql
select * from orders where total > 100;
```

```sql
 from orders where total > 100;
```

`expr AS name` is `name: expr` (MOD023):

<!-- example: grebe check --fix --select MOD023 query.sql -->

```sql
select total * 2 as doubled from orders;
```

```sql
select doubled: total * 2 from orders;
```

`sum(CASE WHEN c THEN 1 ELSE 0 END)` is `count_if(c)` (MOD030):

<!-- example: grebe check --fix --select MOD030 query.sql -->

```sql
select sum(case when status = 'cancelled' then 1 else 0 end) from orders;
```

```sql
select count_if(status = 'cancelled') from orders;
```

`SELECT ... FROM t` is `FROM t SELECT ...` (MOD041):

<!-- example: grebe check --fix --select MOD041 query.sql -->

```sql
select id, total from orders where total > 100;
```

```sql
from orders select id, total where total > 100;
```

## Three queries

Each is taken through
`grebe check --fix --select FRIENDLY query.sql && grebe format query.sql`.

### A report from a BI tool

Revenue and cancellations per customer this year, as a dashboard tool
writes it:

<!-- example: grebe check --fix --select FRIENDLY query.sql && grebe format query.sql -->

```sql
SELECT c.name AS customer_name, COUNT(*) AS order_count, SUM(o.total) AS revenue, SUM(CASE WHEN o.status = 'cancelled' THEN 1 ELSE 0 END) AS cancelled_orders FROM orders o INNER JOIN customers c ON c.id = o.customer_id WHERE o.placed >= DATE '2024-01-01' GROUP BY c.name ORDER BY revenue DESC, customer_name LIMIT 20;
```

```sql
FROM orders o
INNER JOIN customers c ON c.id = o.customer_id
SELECT
    customer_name: c.name,
    order_count: count(),
    revenue: sum(o.total),
    cancelled_orders: count_if(o.status = 'cancelled')
WHERE o.placed >= date '2024-01-01'
GROUP BY ALL
ORDER BY revenue DESC, customer_name
LIMIT 20;
```

The tables come first, so the reader knows what `o` and `c` are before
meeting them. Each column's name is at the start of its line. `count_if`
says what is counted, and `GROUP BY ALL` cannot drift from the select list.

### Net payments per customer

Two CTEs and an outer join, as a colleague might hand it over:

<!-- example: grebe check --fix --select FRIENDLY query.sql && grebe format query.sql -->

```sql
with paid as (select customer_id, sum(amount) as paid from payments group by customer_id),
refunded as (select customer_id, sum(amount) as refunded from refunds group by customer_id)
select paid.customer_id, paid.paid, coalesce(refunded.refunded, 0) as refunded, paid.paid - coalesce(refunded.refunded, 0) as net
from paid left join refunded on refunded.customer_id = paid.customer_id
order by paid.customer_id;
```

```sql
WITH paid AS (
    FROM payments
    SELECT customer_id, paid: sum(amount)
    GROUP BY ALL
),
refunded AS (
    FROM refunds
    SELECT customer_id, refunded: sum(amount)
    GROUP BY ALL
)

FROM paid
LEFT JOIN refunded ON refunded.customer_id = paid.customer_id
SELECT
    paid.customer_id,
    paid.paid,
    refunded: COALESCE(refunded.refunded, 0),
    net: paid.paid - COALESCE(refunded.refunded, 0)
ORDER BY paid.customer_id;
```

Each CTE now starts with the table it reads and says `GROUP BY ALL` instead
of naming `customer_id` twice. In the outer query the two computed columns
are named where the line starts.

### Payment status by month

A join, a CASE sum and a filter on a CTE that reads a whole table:

<!-- example: grebe check --fix --select FRIENDLY query.sql && grebe format query.sql -->

```sql
WITH recent AS (SELECT * FROM orders WHERE placed >= DATE '2024-07-01')
SELECT date_trunc('month', r.placed) AS month, r.status, COUNT(*) AS orders, SUM(CASE WHEN p.id IS NULL THEN 1 ELSE 0 END) AS unpaid
FROM recent r LEFT JOIN payments p ON p.order_id = r.id
GROUP BY date_trunc('month', r.placed), r.status
ORDER BY month, status;
```

```sql
WITH recent AS (
    FROM orders
    WHERE placed >= date '2024-07-01'
)

FROM recent r
LEFT JOIN payments p ON p.order_id = r.id
SELECT
    month: date_trunc('month', r.placed),
    r.status,
    orders: count(),
    unpaid: count_if(p.id IS NULL)
GROUP BY date_trunc('month', r.placed), r.status
ORDER BY month, status;
```

`FROM orders WHERE ...` is the whole CTE: no `SELECT *` to read past. The
`GROUP BY` groups by an expression, which MOD003 leaves as written.

## Results are unchanged

Every FRIENDLY fix is marked safe: it is another spelling of the same
query. That was checked on the shop database of [Catalog](catalog.md) with
DuckDB 2.0.0. Each query on this page was run before and after its rewrite
(`duckdb -readonly -csv`) and the outputs compared with `diff`. The three
queries above gave the same rows in the same order, with the same column
names. The single rewrites gave the same rows; the `GROUP BY` one returns
them in no set order either way, so it was compared sorted.

One thing can change: the name DuckDB gives a column that has no alias,
which is the text of its expression. Before the MOD030 rewrite the column
is called `sum(CASE  WHEN ((status = 'cancelled')) THEN (1) ELSE 0 END)`,
after it `count_if((status = 'cancelled'))`. If something reads a result
by column name, give the column an alias.

To check a rewrite of your own the same way, the rule fixtures carry
`crates/grebe-rules/tests/fixtures/rules/setup.sql`: a table holding every
combination of values, NULL included, to run a query and its rewrite
against.
