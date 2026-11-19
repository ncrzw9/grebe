# Catalog

grebe needs no database. Give it a catalog snapshot, a JSON file of what a
database holds, and it also checks table and column names, and checks
queries against what DuckDB knows of each table: row counts, keys, indexes,
NULLs and types. These are the CAT rules. Without a snapshot they do not
run.

## Making a snapshot

`grebe catalog` runs your own `duckdb` CLI, read-only, on a database file
and prints the snapshot. It is the only grebe command that runs DuckDB.

```sh
grebe catalog --with-metadata shop.duckdb > catalog.json
```

`--duckdb PATH` names the CLI when `duckdb` is not on `PATH`. The snapshot
goes to stdout and a count to stderr: `4 table(s) and view(s), 16
column(s)`.

The examples on this page use a small shop database. To build it, run
[`examples/shop.sql`](examples/shop.sql) with DuckDB 2.0.0
(`duckdb shop.duckdb < shop.sql`). Its tables:

```sql
CREATE TABLE customers (
    id BIGINT PRIMARY KEY,
    name VARCHAR NOT NULL,
    email VARCHAR UNIQUE,
    signed_up DATE
);

CREATE TABLE orders (
    id BIGINT PRIMARY KEY,
    customer_id BIGINT NOT NULL,
    total DECIMAL(10, 2),
    placed DATE,
    status VARCHAR
);

CREATE TABLE payments (
    id BIGINT PRIMARY KEY,
    order_id BIGINT,
    customer_id BIGINT NOT NULL,
    amount DECIMAL(10, 2)
);

CREATE TABLE refunds (
    id BIGINT PRIMARY KEY,
    customer_id BIGINT NOT NULL,
    amount DECIMAL(10, 2)
);

CREATE INDEX orders_placed ON orders (placed);
```

The script then loads 1,000 customers, 2,000,000 orders, 1,800,001
payments (nine orders in ten are paid, and one payment has no order) and
3,000 refunds. [`examples/catalog.json`](examples/catalog.json) is its
snapshot, written by the command above.

## What a snapshot holds

Without `--with-metadata`, a JSON array with one object per column. For a
database made with `CREATE TABLE notes (id BIGINT PRIMARY KEY, body VARCHAR
NOT NULL, tag VARCHAR)` and two rows:

<!-- duckdb -->

```console
$ grebe catalog notes.duckdb
[{"database":"notes","schema":"main","table":"notes","column":"id","type":"BIGINT"},
{"database":"notes","schema":"main","table":"notes","column":"body","type":"VARCHAR"},
{"database":"notes","schema":"main","table":"notes","column":"tag","type":"VARCHAR"}]
1 table(s) and view(s), 3 column(s)
```

This is exactly what DuckDB prints in `-json` mode for the query below, so
anything that can run it can write a snapshot:

```sql
SELECT database_name AS database, schema_name AS schema, table_name AS "table",
       column_name AS "column", data_type AS type
FROM duckdb_columns() WHERE NOT internal;
```

With `--with-metadata`, an object: the same rows under `"columns"`, each
with `"nullable"`, and under `"tables"` the kind of each table, DuckDB's
estimate of its rows, its primary key, unique constraints and indexes:

<!-- duckdb -->

```console
$ grebe catalog --with-metadata notes.duckdb
{"columns": [{"database":"notes","schema":"main","table":"notes","column":"id","type":"BIGINT","nullable":false},
{"database":"notes","schema":"main","table":"notes","column":"body","type":"VARCHAR","nullable":false},
{"database":"notes","schema":"main","table":"notes","column":"tag","type":"VARCHAR","nullable":true}],
"tables": [{"database":"notes","schema":"main","table":"notes","kind":"table","rows":2,"primary_key":["id"],"unique":[],"indexes":null}]}
1 table(s) and view(s), 3 column(s)
```

Both forms are read wherever a snapshot is. CAT001 and CAT002 need only the
columns; CAT003 to CAT010 need the metadata, and say nothing about a table
the snapshot gives no such fact for. The row count is an estimate: it does
not go down when rows are deleted.

## Using it

On the command line:

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select o.id, c.name from orders o join customer c on c.id = o.customer_id where o.id = 7;
```

```text
query.sql:1:40: CAT001 unknown-table (did you mean customers?)

1 finding(s) in 1 file(s)
```

Or in `grebe.toml`, relative to that file, where the language server reads
it too:

<!-- example: grebe check query.sql -->

```toml
catalog = "catalog.json"
```

```sql
select o.id, c.name from orders o join customer c on c.id = o.customer_id where o.id = 7;
```

```text
query.sql:1:40: CAT001 unknown-table (did you mean customers?)

1 finding(s) in 1 file(s)
```

`--catalog` wins over the file. `[severity]` turns a CAT rule off
(`CAT003 = "off"`); `--select` does not affect them.

The rules match names; they do not bind. They report only what cannot be
right and stay silent when in doubt: a CTE, a table the file itself
creates, a file read (`'data/*.parquet'`), a table function or a database
the snapshot does not cover is never reported.

## Rules

The evidence a finding cites is in parentheses in text output and in
`"detail"` in `--json`. A suggested name is in `"suggestion"`.

### CAT001 unknown-table

Warning. A table or view the statement reads or writes is not in the
catalog. DuckDB would fail with a Catalog Error.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select * from ordrs where id = 1;
insert into refund values (3001, 7, 1.00);
```

```text
query.sql:1:15: CAT001 unknown-table (did you mean orders?)
query.sql:2:13: CAT001 unknown-table (did you mean refunds?)

2 finding(s) in 1 file(s)
```

### CAT002 unknown-column

Warning. `alias.column`, where the alias is a catalog table that has no such
column. Unqualified columns are not checked.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select o.totl from orders o where o.id = 1;
select c.nmae from customers c;
```

```text
query.sql:1:10: CAT002 unknown-column (did you mean total?)
query.sql:2:10: CAT002 unknown-column

2 finding(s) in 1 file(s)
```

A suggestion is given only when one name is close enough.

### CAT003 unbounded-scan

Info. The result reads every row of a table the catalog estimates at
1,000,000 rows or more: nothing filters, limits, samples, joins, groups or
aggregates it. `large-table-rows = N` in `grebe.toml` moves the line.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select * from orders;
select * from orders where placed >= date '2024-06-01';
select count() from orders;
```

```text
query.sql:1:15: CAT003 unbounded-scan (orders holds about 2,000,000 rows)

1 finding(s) in 1 file(s)
```

Only the first statement is reported: the second filters, and the third
reads everything because the answer needs it, and returns one row.

### CAT004 index-defeating-filter

Info. A filter wraps the first column of a key or an index in a function, a
cast to text, or a leading-wildcard `LIKE`, so DuckDB filters row by row
instead of on the column.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select id from customers where lower(email) = 'c1@example.com';
select id from orders where strftime(placed, '%Y-%m') = '2024-03';
select id from orders where placed >= date '2024-03-01' and placed < date '2024-04-01';
```

```text
query.sql:1:32: CAT004 index-defeating-filter (lower() is applied to customers.email, which leads a unique key)
query.sql:2:29: CAT004 index-defeating-filter (strftime() is applied to orders.placed, which leads an index)

2 finding(s) in 1 file(s)
```

The range on the third line is what to write instead.

### CAT005 nullable-not-in

Warning. `x NOT IN (SELECT c FROM t)` where `c` may be NULL. One NULL makes
the test never true, so no rows come back. It takes MOD028's place with the
column named, and when the catalog says the column is NOT NULL, neither is
reported.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select count() from orders where id not in (select order_id from payments);
select count() from customers where id not in (select customer_id from payments);
```

```text
query.sql:1:37: CAT005 nullable-not-in (payments.order_id may be NULL)

1 finding(s) in 1 file(s)
```

[What grebe catches](value.md#a-not-in-that-returns-nothing) shows DuckDB's
answer to the first query.

### CAT006 ambiguous-column

Warning. An unqualified column that two tables of the same `FROM` have.
DuckDB rejects the query.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select id, name from orders o join customers c on c.id = o.customer_id where o.total > 100;
```

```text
query.sql:1:8: CAT006 ambiguous-column (id is in o and c)

1 finding(s) in 1 file(s)
```

`name` is only in `customers`, so it is not reported.

### CAT007 insert-mismatch

Warning. An `INSERT ... VALUES` DuckDB rejects: a column the table lacks, a
row whose width is not the column list's, or a row wider than the table.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
insert into refunds (id, customer_id, reason) values (3001, 7, 'damaged');
insert into refunds values (3002, 7, 1.00, 'damaged');
insert into refunds (id, customer_id) values (3003, 7, 1.00);
```

```text
query.sql:1:39: CAT007 insert-mismatch (refunds has no column reason)
query.sql:2:28: CAT007 insert-mismatch (refunds has 3 columns, the row has 4 values)
query.sql:3:46: CAT007 insert-mismatch (2 columns named, 3 values given)

3 finding(s) in 1 file(s)
```

### CAT008 type-mismatch

Info. Two columns of different kinds of type (text, number, date or time,
boolean) compared. DuckDB rejects it or casts one side, which can fail on a
value or match what it should not. On the shop database DuckDB 2.0.0 stops
with `Conversion Error: Could not convert string 'shipped' to INT64`.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select id from orders where status = customer_id;
```

```text
query.sql:1:29: CAT008 type-mismatch (orders.status is VARCHAR and orders.customer_id is BIGINT)

1 finding(s) in 1 file(s)
```

### CAT009 fan-out-join

Info. A join of two keyed tables on columns that are a key of neither: a
row can match many rows on both sides, and the result can be larger than
either table.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select p.id, r.amount from payments p join refunds r on r.customer_id = p.customer_id;
select o.id, c.name from orders o join customers c on c.id = o.customer_id;
```

```text
query.sql:1:54: CAT009 fan-out-join (neither payments(customer_id) nor refunds(customer_id) is a key)

1 finding(s) in 1 file(s)
```

The second join is on `customers.id`, a key, so each order matches at most
one customer.

### CAT010 redundant-distinct

Info. `SELECT DISTINCT` over one table whose select list holds its primary
key, or a unique key with no nullable column: the rows already differ, and
the `DISTINCT` is work for nothing.

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select distinct id, name from customers;
select distinct email from customers;
```

```text
query.sql:1:8: CAT010 redundant-distinct (id is the primary key of customers)

1 finding(s) in 1 file(s)
```

`email` is unique but may be NULL. A unique column can hold many NULLs,
which `DISTINCT` folds into one, so the second is left alone.
