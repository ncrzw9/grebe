# What grebe catches

Situations a DuckDB user meets, what grebe says about them, and what DuckDB
itself does with the same SQL. grebe's output is checked by `cargo test`.
DuckDB's was run with the DuckDB 2.0.0 CLI on the shop database of
[Catalog](catalog.md) (2,000,000 orders, 1,800,001 payments, 3,000
refunds), and is quoted as it printed.

## A NOT IN that returns nothing

Which orders are unpaid? One payment has no order, so its `order_id` is
NULL, and `NOT IN` against a set holding a NULL is never true.

<!-- example: grebe check query.sql -->

```sql
select count() from orders where id not in (select order_id from payments);
```

```text
query.sql:1:37: MOD028 not-in-subquery

1 finding(s) in 1 file(s)
```

With the catalog, grebe names the column:

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select count() from orders where id not in (select order_id from payments);
```

```text
query.sql:1:37: CAT005 nullable-not-in (payments.order_id may be NULL)

1 finding(s) in 1 file(s)
```

DuckDB answers 0. There are 200,000 unpaid orders, which `NOT EXISTS`
finds:

<!-- duckdb -->

```text
D select count() from orders where id not in (select order_id from payments);
┌──────────────┐
│ count_star() │
│    int64     │
├──────────────┤
│            0 │
└──────────────┘
D select count() from orders o where not exists (select 1 from payments p where p.order_id = o.id);
┌──────────────┐
│ count_star() │
│    int64     │
├──────────────┤
│       200000 │
└──────────────┘
```

## A filter on = NULL

`= NULL` is never true. grebe offers `IS NULL` as an unsafe fix, since it
changes the answer.

<!-- example: grebe check query.sql -->

```sql
select count() from payments where order_id = null;
```

```text
query.sql:1:45: MOD002 null-comparison

1 finding(s) in 1 file(s)
```

<!-- duckdb -->

```text
D select count() from payments where order_id = null;
┌──────────────┐
│ count_star() │
│    int64     │
├──────────────┤
│            0 │
└──────────────┘
D select count() from payments where order_id is null;
┌──────────────┐
│ count_star() │
│    int64     │
├──────────────┤
│            1 │
└──────────────┘
```

## A LEFT JOIN that drops rows

Every order with its payment over 10, if any. The condition is in `WHERE`,
so unmatched orders are dropped, and the outer join is an inner join.

<!-- example: grebe check query.sql -->

```sql
select count() from orders o left join payments p on p.order_id = o.id where p.amount > 10;
```

```text
query.sql:1:78: MOD029 filter-defeats-outer-join

1 finding(s) in 1 file(s)
```

<!-- duckdb -->

```text
D select count() from orders o left join payments p on p.order_id = o.id where p.amount > 10;
┌──────────────┐
│ count_star() │
│    int64     │
├──────────────┤
│      1764000 │
└──────────────┘
D select count() from orders o left join payments p on p.order_id = o.id and p.amount > 10;
┌──────────────┐
│ count_star() │
│    int64     │
├──────────────┤
│      2000000 │
└──────────────┘
```

## A misspelt column, before the query runs

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select o.totl from orders o where o.id = 1;
```

```text
query.sql:1:10: CAT002 unknown-column (did you mean total?)

1 finding(s) in 1 file(s)
```

DuckDB finds it too, once the query is sent:

<!-- duckdb -->

```text
D select o.totl from orders o where o.id = 1;
Binder Error: Table "o" does not have a column named "totl"

Candidate bindings: : "total", "status"

LINE 1: select o.totl from orders o where o.id = 1;
               ^^^^^^
```

grebe reports it in the editor as you type, in a file of many statements,
and in CI, without a database.

## A join that multiplies rows

Payments joined to refunds on the customer: neither side's `customer_id` is
a key, so each payment meets every refund of its customer.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select count() from payments p join refunds r on r.customer_id = p.customer_id;
```

```text
query.sql:1:47: CAT009 fan-out-join (neither payments(customer_id) nor refunds(customer_id) is a key)

1 finding(s) in 1 file(s)
```

The join returns three times as many rows as the larger table:

<!-- duckdb -->

```text
D select count() from payments p join refunds r on r.customer_id = p.customer_id;
┌──────────────┐
│ count_star() │
│    int64     │
├──────────────┤
│      5400003 │
└──────────────┘
D select (select count() from payments) as payments, (select count() from refunds) as refunds;
┌──────────┬─────────┐
│ payments │ refunds │
│  int64   │  int64  │
├──────────┼─────────┤
│  1800001 │    3000 │
└──────────┴─────────┘
```

## An unbounded scan of a big table

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select * from orders;
```

```text
query.sql:1:15: CAT003 unbounded-scan (orders holds about 2,000,000 rows)

1 finding(s) in 1 file(s)
```

The estimate comes from DuckDB, through the snapshot. A `WHERE`, a `LIMIT`,
a sample or an aggregate silences it.

## An INSERT the table cannot take

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
insert into refunds (id, customer_id, reason) values (3001, 7, 'damaged');
```

```text
query.sql:1:39: CAT007 insert-mismatch (refunds has no column reason)

1 finding(s) in 1 file(s)
```

<!-- duckdb -->

```text
D insert into refunds (id, customer_id, reason) values (3001, 7, 'damaged');
Binder Error: Table "refunds" does not have a column with name "reason"

Did you mean: "amount"
```

## An ambiguous column

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select id, name from orders o join customers c on c.id = o.customer_id where o.total > 100;
```

```text
query.sql:1:8: CAT006 ambiguous-column (id is in o and c)

1 finding(s) in 1 file(s)
```

<!-- duckdb -->

```text
D select id, name from orders o join customers c on c.id = o.customer_id where o.total > 100;
Binder Error: Ambiguous reference to column name "id" (use: 'o.id' or 'c.id')

LINE 1: select id, name from orders o join customers c on c.id = o.custome...
               ^^
```

## Text compared with a number

A join on the wrong column: a name against an id. grebe reports both what
it costs and why it is wrong.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select count() from orders o join customers c on c.name = o.customer_id;
```

```text
query.sql:1:47: CAT009 fan-out-join (neither orders(customer_id) nor customers(name) is a key)
query.sql:1:50: CAT008 type-mismatch (customers.name is VARCHAR and orders.customer_id is BIGINT)

2 finding(s) in 1 file(s)
```

DuckDB casts the name to a number and stops on the first one that is not:

<!-- duckdb -->

```text
D select count() from orders o join customers c on c.name = o.customer_id;
Conversion Error: Could not convert string 'customer 1' to INT64 when casting from source column name

LINE 1: select count() from orders o join customers c on c.name = o.customer_id;
                                                         ^^^^^^
```

## A query from a tool, made readable

<!-- example: grebe check --fix --select FRIENDLY query.sql && grebe format query.sql -->

```sql
SELECT customers.name AS name, COUNT(*) AS orders, SUM(CASE WHEN orders.status = 'cancelled' THEN 1 ELSE 0 END) AS cancelled FROM orders INNER JOIN customers ON customers.id = orders.customer_id GROUP BY customers.name ORDER BY orders DESC, name LIMIT 10;
```

```sql
FROM orders
INNER JOIN customers ON customers.id = orders.customer_id
SELECT name: customers.name, orders: count(), cancelled: count_if(orders.status = 'cancelled')
GROUP BY ALL
ORDER BY orders DESC, name
LIMIT 10;
```

Same rows; see [Friendly SQL](friendly-sql.md) for how that was checked.

## A 12 MB file, formatted in seconds

A generated file of 12,170,132 bytes and 41,600 statements (joins, CTEs,
window functions, CASE sums) formats in 3.9 s with a peak of 209 MB:

```sh
grebe format --no-config - < 12mb.sql > /dev/null
```

Measured on a 4-core Linux machine with the release build of 1.0.0-rc.3,
median of 3 runs, wall time and peak resident memory read from the process
on exit. Before the 1.0 parser work the same file took 37 s and 6 GB
(measured then, not re-run here).

## A whole test suite, checked in four seconds

`grebe check` on DuckDB's own SQL tests, 5,516 files and 24 MB, takes 4.1 s
and 192 MB, against 13.1 s and 2.8 GB before the 1.0 work. Measured on a
4-core Linux machine, median of 3 runs; see the table in the
[README](../README.md#performance).
