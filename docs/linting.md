# Linting

`grebe check` reads SQL and reports what is likely wrong, slow, or more
plainly written another way. It reads the SQL alone; it runs no query and
needs no database. Given a [catalog snapshot](catalog.md) it also checks
names and keys.

## What it prints

One line per finding, `path:line:column: CODE name`, on stdout. A summary
goes to stderr.

<!-- example: grebe check query.sql -->

```sql
select status, count(*) from orders where total = null group by status;
```

```text
query.sql:1:22: MOD001 count-star
query.sql:1:49: MOD002 null-comparison
query.sql:1:56: MOD003 group-by-all

3 finding(s) in 1 file(s)
```

`--json` prints one JSON document instead. Offsets `start` and `end` are
bytes into the file.

<!-- example: grebe check --json query.sql -->

```sql
select status, count(*) from orders where total = null group by status;
```

```json
{"version":"1.0.0-rc.3","files":1,"findings":[{"path":"query.sql","line":1,"column":22,"code":"MOD001","name":"count-star","severity":"info","start":21,"end":22},{"path":"query.sql","line":1,"column":49,"code":"MOD002","name":"null-comparison","severity":"warning","start":48,"end":54},{"path":"query.sql","line":1,"column":56,"code":"MOD003","name":"group-by-all","severity":"info","start":55,"end":70}],"unparsed":0,"fixed":0}

3 finding(s) in 1 file(s)
```

A finding a catalog backs carries its evidence, and a suggestion when there
is one:

<!-- example: grebe check --json --catalog catalog.json query.sql -->

```sql
select id from ordrs;
```

```json
{"version":"1.0.0-rc.3","files":1,"findings":[{"path":"query.sql","line":1,"column":16,"code":"CAT001","name":"unknown-table","severity":"warning","start":15,"end":20,"suggestion":"orders"}],"unparsed":0,"fixed":0}

1 finding(s) in 1 file(s)
```

A statement that does not parse is reported, and the rest of the file is
still checked:

<!-- example: grebe check query.sql -->

```sql
select id from orders where;
select count(*) from orders;
```

```text
query.sql:1:28: PRS001 syntax-error
query.sql:2:14: MOD001 count-star

2 finding(s) in 1 file(s); 1 at error severity; 1 statement(s) did not parse
```

## Exit codes

| code | meaning |
|---|---|
| 0 | no findings, or none at error severity |
| 1 | a finding at error severity |
| 2 | usage or config error, or a file that could not be read or written |

Only PRS001 is an error by default. `[severity]` can make any rule one.

## Fixing

`--fix` rewrites the files in place with the fixes marked safe in
`grebe rules`, then prints what is left. A safe fix does not change what a
query returns.

<!-- example: grebe check --fix query.sql -->

```sql
select status, count(*) from orders where total = null group by status;
```

```text
query.sql:1:48: MOD002 null-comparison

fixed 2 finding(s) in 1 file(s)

1 finding(s) in 1 file(s)
```

The file afterwards:

<!-- example: grebe check --fix query.sql -->

```sql
select status, count(*) from orders where total = null group by status;
```

```sql
select status, count() from orders where total = null GROUP BY ALL;
```

`--unsafe` adds the fixes that can change the result. Read the diff.

<!-- example: grebe check --fix --unsafe query.sql -->

```sql
select status, count(*) from orders where total = null group by status;
```

```sql
select status, count() from orders where total IS NULL GROUP BY ALL;
```

The fixed SQL is not re-laid out; run `grebe format` after.

## Choosing rules

Without `--select`, every rule whose default severity is not `off` runs.
`--select` runs only the MOD rules it names, opt-in ones included:

<!-- example: grebe check --select MOD002,MOD041 query.sql -->

```sql
select status, count(*) from orders where total = null group by status;
```

```text
query.sql:1:1: MOD041 from-before-select
query.sql:1:49: MOD002 null-comparison

2 finding(s) in 1 file(s)
```

`--select ALL` runs every MOD rule:

<!-- example: grebe check --select ALL query.sql -->

```sql
select status, count(*) from orders where total = null group by status;
```

```text
query.sql:1:1: MOD041 from-before-select
query.sql:1:22: MOD001 count-star
query.sql:1:49: MOD002 null-comparison
query.sql:1:56: MOD003 group-by-all

4 finding(s) in 1 file(s)
```

`--select FRIENDLY` runs the rules that rewrite SQL into DuckDB's friendly
SQL; see [Friendly SQL](friendly-sql.md). It can stand among other codes:
`--select FRIENDLY,MOD006`.

PRS and SRC findings are reported whatever `--select` says, and CAT rules
run whenever a catalog is given; `[severity]` can turn any of them off. `select = [...]` in `grebe.toml` does the same as
`--select`; the command line wins.

## Severity

`[severity]` sets a rule to `error`, `warning`, `info` or `off`. A level
turns an opt-in rule on.

<!-- example: grebe check query.sql -->

```toml
[severity]
MOD002 = "error"
MOD001 = "off"
MOD041 = "info"
```

```sql
select status, count(*) from orders where total = null group by status;
```

```text
query.sql:1:1: MOD041 from-before-select
query.sql:1:49: MOD002 null-comparison
query.sql:1:56: MOD003 group-by-all

3 finding(s) in 1 file(s); 1 at error severity
```

That run exits 1.

## Silencing one statement

`-- grebe: ignore[CODE, ...]` before a statement, or on its first line,
silences those codes for it. A bare `-- grebe: ignore` silences every code.
A silenced finding is never fixed.

<!-- example: grebe check query.sql -->

```sql
-- grebe: ignore[MOD001]
select count(*) from orders;
select count(*) from payments; -- grebe: ignore
select count(*) from refunds;
```

```text
query.sql:4:14: MOD001 count-star

1 finding(s) in 1 file(s)
```

A comment on the line where the previous statement ends belongs to that
statement.

## Rules

`grebe rules` lists them all:

<!-- example: grebe rules -->

```text
CODE    NAME                       CATEGORY  SEVERITY      DETECTOR  FIX
CAT001  unknown-table              CAT       warning       yes       no fix
CAT002  unknown-column             CAT       warning       yes       no fix
CAT003  unbounded-scan             CAT       info          yes       no fix
CAT004  index-defeating-filter     CAT       info          yes       no fix
CAT005  nullable-not-in            CAT       warning       yes       no fix
CAT006  ambiguous-column           CAT       warning       yes       no fix
CAT007  insert-mismatch            CAT       warning       yes       no fix
CAT008  type-mismatch              CAT       info          yes       no fix
CAT009  fan-out-join               CAT       info          yes       no fix
CAT010  redundant-distinct         CAT       info          yes       no fix
MOD001  count-star                 MOD       info          yes       fix (safe)
MOD002  null-comparison            MOD       warning       yes       fix (unsafe)
MOD003  group-by-all               MOD       info          yes       fix (safe)
MOD004  subquery-order-by          MOD       info          yes       no fix
MOD005  distinct-group-by          MOD       warning       yes       no fix
MOD006  ifnull-coalesce            MOD       info          yes       fix (safe)
MOD007  natural-join               MOD       warning       yes       no fix
MOD008  bare-union                 MOD       info          yes       no fix
MOD009  self-alias                 MOD       info          yes       fix (safe)
MOD010  implicit-cross-join        MOD       off (opt-in)  yes       no fix
MOD011  ordinal-reference          MOD       info          yes       no fix
MOD012  view-order-by              MOD       info          yes       no fix
MOD013  order-by-all               MOD       info          yes       fix (safe)
MOD014  case-to-filter             MOD       info          yes       fix (unsafe)
MOD015  case-to-switch             MOD       info          yes       fix (safe)
MOD016  unused-cte                 MOD       warning       yes       fix (safe)
MOD017  select-star-ctas           MOD       off (opt-in)  yes       no fix
MOD018  constraint-load-cost       MOD       off (opt-in)  yes       no fix
MOD019  large-in-list              MOD       off (opt-in)  yes       no fix
MOD020  plain-big-number           MOD       off (opt-in)  yes       no fix
MOD021  qualify-rewrite            MOD       info          yes       no fix
MOD022  from-first                 MOD       off (opt-in)  yes       fix (safe)
MOD023  prefix-alias               MOD       off (opt-in)  yes       fix (safe)
MOD025  select-star                MOD       off (opt-in)  yes       no fix
MOD026  limit-no-orderby           MOD       info          yes       no fix
MOD027  window-no-orderby          MOD       warning       yes       no fix
MOD028  not-in-subquery            MOD       warning       yes       no fix
MOD029  filter-defeats-outer-join  MOD       warning       yes       no fix
MOD030  sum-case-to-count-if       MOD       info          yes       fix (safe)
MOD031  case-to-function           MOD       off (opt-in)  yes       fix (safe)
MOD032  not-in-null                MOD       warning       yes       no fix
MOD033  having-without-aggregate   MOD       off (opt-in)  yes       no fix
MOD034  redundant-count-default    MOD       off (opt-in)  yes       fix (safe)
MOD035  wrapped-date-filter        MOD       info          yes       no fix
MOD036  row-at-a-time-insert       MOD       info          yes       no fix
MOD037  order-by-random-sample     MOD       info          yes       no fix
MOD038  sample-before-where        MOD       warning       yes       no fix
MOD039  delete-then-insert         MOD       info          yes       no fix
MOD040  csv-full-sniff             MOD       info          yes       no fix
MOD041  from-before-select         MOD       off (opt-in)  yes       fix (safe)
PRS001  syntax-error               PRS       error         yes       no fix
SRC002  unrenderable-template      SRC       info          yes       no fix
SRC003  foreign-dialect            SRC       info          yes       no fix

Rows marked "off (opt-in)" default to Off; enable one with 'grebe check --select CODE,...'.
'--select FRIENDLY' is MOD001, MOD003, MOD013, MOD022, MOD023, MOD030, MOD041: the safe rewrites into DuckDB's friendly SQL.
DETECTOR "not yet" means the rule is registered but has no detector, so it cannot fire (53 of 53 can).
```

Each entry shows the input, the finding, and for a rule with a fix, the SQL
after `grebe check --fix`. Opt-in rules are run with `--select`.

### PRS001 syntax-error

Error. The statement does not parse as DuckDB SQL, so DuckDB would refuse it
too.

<!-- example: grebe check query.sql -->

```sql
select id from orders where;
```

```text
query.sql:1:28: PRS001 syntax-error

1 finding(s) in 1 file(s); 1 at error severity; 1 statement(s) did not parse
```

### SRC002 unrenderable-template

Info. The file holds template markup (dbt, Jinja), which is not SQL until
rendered; those statements are skipped rather than reported as syntax errors.

<!-- example: grebe check query.sql -->

```sql
select * from {{ ref('orders') }};
```

```text
query.sql:1:1: SRC002 unrenderable-template

1 finding(s) in 1 file(s); 1 statement(s) did not parse
```

### SRC003 foreign-dialect

Info. The statement is from another dialect (here a `GRANT`, which DuckDB
does not have), so it is skipped. `foreign-heads = ["..."]` in `grebe.toml` adds statement heads.

<!-- example: grebe check query.sql -->

```sql
grant select on orders to analyst;
```

```text
query.sql:1:1: SRC003 foreign-dialect

1 finding(s) in 1 file(s); 1 statement(s) did not parse
```

### CAT001 to CAT010

These need a catalog. Each is described, with its input and finding, in
[Catalog](catalog.md).

### MOD001 count-star

Info, safe fix. `count()` is DuckDB's spelling of `count(*)`.

<!-- example: grebe check --select MOD001 query.sql -->

```sql
select count(*) from orders;
```

```text
query.sql:1:14: MOD001 count-star

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD001 query.sql -->

```sql
select count(*) from orders;
```

```sql
select count() from orders;
```

### MOD002 null-comparison

Warning, unsafe fix. `= NULL` is never true, so the filter keeps no rows.

<!-- example: grebe check --select MOD002 query.sql -->

```sql
select id from orders where status = null;
```

```text
query.sql:1:36: MOD002 null-comparison

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --unsafe --select MOD002 query.sql -->

```sql
select id from orders where status = null;
```

```sql
select id from orders where status IS NULL;
```

### MOD003 group-by-all

Info, safe fix. `GROUP BY ALL` stays right when the select list changes.

<!-- example: grebe check --select MOD003 query.sql -->

```sql
select customer_id, status, sum(total) from orders group by customer_id, status;
```

```text
query.sql:1:52: MOD003 group-by-all

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD003 query.sql -->

```sql
select customer_id, status, sum(total) from orders group by customer_id, status;
```

```sql
select customer_id, status, sum(total) from orders GROUP BY ALL;
```

### MOD004 subquery-order-by

Info. An `ORDER BY` in a subquery without `LIMIT` need not survive the outer
query.

<!-- example: grebe check --select MOD004 query.sql -->

```sql
select * from (select id from orders order by placed) where id > 10;
```

```text
query.sql:1:38: MOD004 subquery-order-by

1 finding(s) in 1 file(s)
```

### MOD005 distinct-group-by

Warning. `DISTINCT` on a grouped query is redundant, or hides a wrong
`GROUP BY`.

<!-- example: grebe check --select MOD005 query.sql -->

```sql
select distinct customer_id from orders group by customer_id;
```

```text
query.sql:1:8: MOD005 distinct-group-by

1 finding(s) in 1 file(s)
```

### MOD006 ifnull-coalesce

Info, safe fix. `coalesce` is the standard name, and takes more than two
arguments.

<!-- example: grebe check --select MOD006 query.sql -->

```sql
select ifnull(status, 'none') from orders;
```

```text
query.sql:1:8: MOD006 ifnull-coalesce

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD006 query.sql -->

```sql
select ifnull(status, 'none') from orders;
```

```sql
select coalesce(status, 'none') from orders;
```

### MOD007 natural-join

Warning. A `NATURAL JOIN` joins on every shared column name, so adding a
column changes the join.

<!-- example: grebe check --select MOD007 query.sql -->

```sql
select * from orders natural join payments;
```

```text
query.sql:1:22: MOD007 natural-join

1 finding(s) in 1 file(s)
```

### MOD008 bare-union

Info. `UNION` removes duplicates; say `UNION ALL` or `UNION DISTINCT` so the
reader knows it is meant.

<!-- example: grebe check --select MOD008 query.sql -->

```sql
select id from orders union select order_id from payments;
```

```text
query.sql:1:23: MOD008 bare-union

1 finding(s) in 1 file(s)
```

### MOD009 self-alias

Info, safe fix. `id AS id` says nothing.

<!-- example: grebe check --select MOD009 query.sql -->

```sql
select id as id, total from orders;
```

```text
query.sql:1:8: MOD009 self-alias

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD009 query.sql -->

```sql
select id as id, total from orders;
```

```sql
select id, total from orders;
```

### MOD010 implicit-cross-join

Off (opt-in). A comma join hides its condition in `WHERE`, where it is easy
to lose.

<!-- example: grebe check --select MOD010 query.sql -->

```sql
select * from orders o, customers c where c.id = o.customer_id;
```

```text
query.sql:1:23: MOD010 implicit-cross-join

1 finding(s) in 1 file(s)
```

### MOD011 ordinal-reference

Info. `GROUP BY 1` points at a position, and silently points at another
column when the select list changes.

<!-- example: grebe check --select MOD011 query.sql -->

```sql
select status, count() from orders group by 1 order by 2 desc;
```

```text
query.sql:1:45: MOD011 ordinal-reference
query.sql:1:56: MOD011 ordinal-reference

2 finding(s) in 1 file(s)
```

### MOD012 view-order-by

Info. A view's `ORDER BY` is not part of what the view promises.

<!-- example: grebe check --select MOD012 query.sql -->

```sql
create view recent as select * from orders order by placed desc;
```

```text
query.sql:1:44: MOD012 view-order-by

1 finding(s) in 1 file(s)
```

### MOD013 order-by-all

Info, safe fix. `ORDER BY ALL` says "every column, in order".

<!-- example: grebe check --select MOD013 query.sql -->

```sql
select status, customer_id from orders order by status, customer_id;
```

```text
query.sql:1:40: MOD013 order-by-all

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD013 query.sql -->

```sql
select status, customer_id from orders order by status, customer_id;
```

```sql
select status, customer_id from orders ORDER BY ALL;
```

### MOD014 case-to-filter

Info, unsafe fix. `FILTER (WHERE ...)` puts the condition where it reads as
one. The fix drops the `CASE`'s `ELSE`, so with an `ELSE 0` the result can
change: `sum` of no matching rows is NULL, not 0.

<!-- example: grebe check --select MOD014 query.sql -->

```sql
select sum(case when status = 'shipped' then total end) from orders;
```

```text
query.sql:1:8: MOD014 case-to-filter

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --unsafe --select MOD014 query.sql -->

```sql
select sum(case when status = 'shipped' then total end) from orders;
```

```sql
select sum(total) FILTER (WHERE status = 'shipped') from orders;
```

### MOD015 case-to-switch

Info, safe fix. When every arm tests the same expression, `CASE expr WHEN`
says it once.

<!-- example: grebe check --select MOD015 query.sql -->

```sql
select case when status = 'shipped' then 1 when status = 'cancelled' then 2 else 0 end from orders;
```

```text
query.sql:1:8: MOD015 case-to-switch

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD015 query.sql -->

```sql
select case when status = 'shipped' then 1 when status = 'cancelled' then 2 else 0 end from orders;
```

```sql
select CASE status WHEN 'shipped' THEN 1 WHEN 'cancelled' THEN 2 ELSE 0 END from orders;
```

### MOD016 unused-cte

Warning, safe fix. A CTE nothing reads is dead code, or a sign the wrong
name is read.

<!-- example: grebe check --select MOD016 query.sql -->

```sql
with unused as (select 1), used as (select 2 as x) select x from used;
```

```text
query.sql:1:6: MOD016 unused-cte

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD016 query.sql -->

```sql
with unused as (select 1), used as (select 2 as x) select x from used;
```

```sql
with  used as (select 2 as x) select x from used;
```

### MOD017 select-star-ctas

Off (opt-in). `SELECT *` into a new table copies every column, including the
ones added upstream later.

<!-- example: grebe check --select MOD017 query.sql -->

```sql
create table orders_copy as select * from orders;
```

```text
query.sql:1:29: MOD017 select-star-ctas

1 finding(s) in 1 file(s)
```

### MOD018 constraint-load-cost

Off (opt-in). Keys and constraints slow bulk loads and do not speed up
queries in DuckDB.

<!-- example: grebe check --select MOD018 query.sql -->

```sql
create table events (id bigint primary key, payload json);
```

```text
query.sql:1:32: MOD018 constraint-load-cost

1 finding(s) in 1 file(s)
```

### MOD019 large-in-list

Off (opt-in). A literal `IN` list of 20 or more values reads better as a
join against `VALUES` or a table.

<!-- example: grebe check --select MOD019 query.sql -->

```sql
select * from orders where id in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20);
```

```text
query.sql:1:34: MOD019 large-in-list

1 finding(s) in 1 file(s)
```

### MOD020 plain-big-number

Off (opt-in). `1_000_000` is easier to read than `1000000`.

<!-- example: grebe check --select MOD020 query.sql -->

```sql
select * from orders where total > 1000000;
```

```text
query.sql:1:36: MOD020 plain-big-number

1 finding(s) in 1 file(s)
```

### MOD021 qualify-rewrite

Info. `QUALIFY` filters on a window function in place, without a wrapping
subquery.

<!-- example: grebe check --select MOD021 query.sql -->

```sql
select * from (select id, row_number() over (partition by customer_id order by placed desc) as rn from orders) where rn = 1;
```

```text
query.sql:1:112: MOD021 qualify-rewrite

1 finding(s) in 1 file(s)
```

### MOD022 from-first

Off (opt-in), safe fix. `FROM t` alone means `SELECT * FROM t`.

<!-- example: grebe check --select MOD022 query.sql -->

```sql
select * from orders;
```

```text
query.sql:1:1: MOD022 from-first

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD022 query.sql -->

```sql
select * from orders;
```

```sql
 from orders;
```

### MOD023 prefix-alias

Off (opt-in), safe fix. `name: expr` puts the name first, where the eye
looks for it.

<!-- example: grebe check --select MOD023 query.sql -->

```sql
select total * 2 as doubled from orders;
```

```text
query.sql:1:8: MOD023 prefix-alias

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD023 query.sql -->

```sql
select total * 2 as doubled from orders;
```

```sql
select doubled: total * 2 from orders;
```

### MOD025 select-star

Off (opt-in). A `*` in the final result changes shape when the table does.

<!-- example: grebe check --select MOD025 query.sql -->

```sql
select * from orders where id = 1;
```

```text
query.sql:1:8: MOD025 select-star

1 finding(s) in 1 file(s)
```

### MOD026 limit-no-orderby

Info. Without `ORDER BY`, which rows `LIMIT` keeps can change from run to
run.

<!-- example: grebe check --select MOD026 query.sql -->

```sql
select id from orders limit 10;
```

```text
query.sql:1:23: MOD026 limit-no-orderby

1 finding(s) in 1 file(s)
```

### MOD027 window-no-orderby

Warning. `row_number()` with no `ORDER BY` numbers rows in no set order.

<!-- example: grebe check --select MOD027 query.sql -->

```sql
select id, row_number() over (partition by customer_id) from orders;
```

```text
query.sql:1:12: MOD027 window-no-orderby

1 finding(s) in 1 file(s)
```

### MOD028 not-in-subquery

Warning. One NULL in the subquery's result makes `NOT IN` match nothing.
With a catalog, [CAT005](catalog.md#cat005-nullable-not-in) reports it with
the column named, or not at all when the column is NOT NULL.

<!-- example: grebe check --select MOD028 query.sql -->

```sql
select id from orders where id not in (select order_id from payments);
```

```text
query.sql:1:32: MOD028 not-in-subquery

1 finding(s) in 1 file(s)
```

### MOD029 filter-defeats-outer-join

Warning. A `WHERE` on the right side of a `LEFT JOIN` drops the unmatched
rows, so the join acts as an inner join.

<!-- example: grebe check --select MOD029 query.sql -->

```sql
select o.id from orders o left join payments p on p.order_id = o.id where p.amount > 10;
```

```text
query.sql:1:75: MOD029 filter-defeats-outer-join

1 finding(s) in 1 file(s)
```

### MOD030 sum-case-to-count-if

Info, safe fix. `count_if(c)` says what `sum(CASE WHEN c THEN 1 ELSE 0 END)`
means.

<!-- example: grebe check --select MOD030 query.sql -->

```sql
select sum(case when status = 'cancelled' then 1 else 0 end) from orders;
```

```text
query.sql:1:8: MOD030 sum-case-to-count-if

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD030 query.sql -->

```sql
select sum(case when status = 'cancelled' then 1 else 0 end) from orders;
```

```sql
select count_if(status = 'cancelled') from orders;
```

### MOD031 case-to-function

Off (opt-in), safe fix. This `CASE` is `coalesce` or `nullif` written out.

<!-- example: grebe check --select MOD031 query.sql -->

```sql
select case when status is null then 'none' else status end from orders;
```

```text
query.sql:1:8: MOD031 case-to-function

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD031 query.sql -->

```sql
select case when status is null then 'none' else status end from orders;
```

```sql
select coalesce(status, 'none') from orders;
```

### MOD032 not-in-null

Warning. A NULL in a `NOT IN` list makes the test never true: no rows come
back.

<!-- example: grebe check --select MOD032 query.sql -->

```sql
select id from orders where status not in ('cancelled', null);
```

```text
query.sql:1:29: MOD032 not-in-null

1 finding(s) in 1 file(s)
```

### MOD033 having-without-aggregate

Off (opt-in). A `HAVING` with no aggregate is a `WHERE`, which filters
before grouping and so does less work.

<!-- example: grebe check --select MOD033 query.sql -->

```sql
select status, count() from orders group by status having status <> 'cancelled';
```

```text
query.sql:1:52: MOD033 having-without-aggregate

1 finding(s) in 1 file(s)
```

### MOD034 redundant-count-default

Off (opt-in), safe fix. `count` is never NULL, so the default does nothing.

<!-- example: grebe check --select MOD034 query.sql -->

```sql
select coalesce(count(id), 0) from orders;
```

```text
query.sql:1:8: MOD034 redundant-count-default

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD034 query.sql -->

```sql
select coalesce(count(id), 0) from orders;
```

```sql
select count(id) from orders;
```

### MOD035 wrapped-date-filter

Info. `year(placed) = 2024` reads every row group; a range on the column
lets DuckDB skip the ones outside it.

<!-- example: grebe check --select MOD035 query.sql -->

```sql
select id from orders where year(placed) = 2024;
```

```text
query.sql:1:29: MOD035 wrapped-date-filter

1 finding(s) in 1 file(s)
```

### MOD036 row-at-a-time-insert

Info. Ten or more single-row `INSERT`s into one table in a row are much
slower than one multi-row `INSERT`.

<!-- example: grebe check --select MOD036 query.sql -->

```sql
insert into refunds values (1, 10, 1.00);
insert into refunds values (2, 11, 1.00);
insert into refunds values (3, 12, 1.00);
insert into refunds values (4, 13, 1.00);
insert into refunds values (5, 14, 1.00);
insert into refunds values (6, 15, 1.00);
insert into refunds values (7, 16, 1.00);
insert into refunds values (8, 17, 1.00);
insert into refunds values (9, 18, 1.00);
insert into refunds values (10, 19, 1.00);
```

```text
query.sql:1:1: MOD036 row-at-a-time-insert

1 finding(s) in 1 file(s)
```

### MOD037 order-by-random-sample

Info. `ORDER BY random() LIMIT n` sorts every row to keep n; `USING SAMPLE n
ROWS` does not.

<!-- example: grebe check --select MOD037 query.sql -->

```sql
select * from orders order by random() limit 100;
```

```text
query.sql:1:22: MOD037 order-by-random-sample

1 finding(s) in 1 file(s)
```

### MOD038 sample-before-where

Warning. `USING SAMPLE` samples before `WHERE` filters, so far fewer rows
than asked for come back.

<!-- example: grebe check --select MOD038 query.sql -->

```sql
select * from orders where status = 'cancelled' using sample 100 rows;
```

```text
query.sql:1:49: MOD038 sample-before-where

1 finding(s) in 1 file(s)
```

### MOD039 delete-then-insert

Info. A `DELETE` then `INSERT` from the same source is an upsert in two
passes; `MERGE INTO` does it in one atomic statement.

<!-- example: grebe check --select MOD039 query.sql -->

```sql
delete from orders where id in (select id from staged_orders);
insert into orders select * from staged_orders;
```

```text
query.sql:1:1: MOD039 delete-then-insert

1 finding(s) in 1 file(s)
```

### MOD040 csv-full-sniff

Info. `sample_size = -1` reads the whole CSV to guess its types; declaring
the types is faster.

<!-- example: grebe check --select MOD040 query.sql -->

```sql
select * from read_csv('orders.csv', sample_size = -1);
```

```text
query.sql:1:15: MOD040 csv-full-sniff

1 finding(s) in 1 file(s)
```

### MOD041 from-before-select

Off (opt-in), safe fix. `FROM t SELECT ...` reads in the order the query
runs.

<!-- example: grebe check --select MOD041 query.sql -->

```sql
select id, total from orders where total > 100;
```

```text
query.sql:1:1: MOD041 from-before-select

1 finding(s) in 1 file(s)
```

<!-- example: grebe check --fix --select MOD041 query.sql -->

```sql
select id, total from orders where total > 100;
```

```sql
from orders select id, total where total > 100;
```
