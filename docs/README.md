# grebe docs

- [Formatting](formatting.md): what `grebe format` does, every `[format]`
  option and style, format off/on, `--check`, stdin.
- [Linting](linting.md): what `grebe check` prints, `--fix`, `--select`,
  severities, suppression, exit codes, and every rule with an example.
- [Friendly SQL](friendly-sql.md): `--select FRIENDLY`, each rewrite, and
  three real queries taken through it.
- [Catalog](catalog.md): making a snapshot of a DuckDB database, its shape,
  and the CAT rules that read it.
- [What grebe catches](value.md): mistakes DuckDB users make, what grebe
  says, and what DuckDB does with the same SQL.

## Quick start

Install:

```sh
cargo install grebe
```

or take a binary from the
[releases](https://github.com/ncrzw9/grebe/releases), or the `ncrzw9.grebe`
extension for VS Code.

Format a file in place:

<!-- example: grebe format query.sql -->

```sql
select id,name from customers where signed_up>=date '2024-01-01' order by name
```

```sql
SELECT id, name FROM customers WHERE signed_up >= date '2024-01-01' ORDER BY name;
```

Check a file:

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

Check a file against a catalog snapshot of your database
(`grebe catalog --with-metadata shop.duckdb > catalog.json`):

<!-- example: grebe check --catalog catalog.json query.sql -->

```sql
select c.name, o.totl from orders o join customers c on c.id = o.customer_id;
```

```text
query.sql:1:18: CAT002 unknown-column (did you mean total?)

1 finding(s) in 1 file(s)
```

## How the examples are kept exact

Every example that runs grebe is marked in the page source with an HTML
comment that holds the command, followed by its input and output blocks
(and a `grebe.toml` block when it has one):

````markdown
<!-- example: grebe check --select MOD001 query.sql -->

```sql
select count(*) from orders;
```

```text
query.sql:1:14: MOD001 count-star

1 finding(s) in 1 file(s)
```
````

`cargo test` (`crates/grebe-cli/tests/docs.rs`) runs each one in an empty
directory holding the files in [`examples/`](examples) and compares the
result with the page: the file after the command when the output block is
`sql`, otherwise everything printed, stdout then stderr. After a deliberate
change, `GREBE_DOCS_BLESS=1 cargo test -p grebe --test docs` rewrites the
outputs; review the diff.

Output from DuckDB itself is marked `<!-- duckdb -->` and is not run by the
test. It was run with the DuckDB 2.0.0 CLI on the database
[`examples/shop.sql`](examples/shop.sql) builds.
