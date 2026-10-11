# Contributing to grebe

Thanks for helping. Bug reports with a SQL snippet are the most useful thing
you can send; a pull request is welcome too.

## Before you start

For anything bigger than a small fix (a new rule, a formatting change, a new
extension feature) open an issue first, so we agree on the behaviour before
you write it. Formatting output and rule behaviour are user-facing: a change
reformats or re-flags people's projects.

Please keep to [Scope](README.md#scope): DuckDB only, no catalog binding, no
templated SQL.

## Building and testing

You need Rust 1.85 or newer. The same checks CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The VS Code extension lives in `editors/vscode`; see
[its README](editors/vscode/README.md#build-from-source) for building it and
running its tests against a real `grebe` and `duckdb`.

## Where things go

| Crate          | What it holds                                        |
|----------------|------------------------------------------------------|
| `grebe-syntax` | tokenizer, PEG matcher, concrete syntax tree         |
| `grebe-rules`  | lint rules, configuration, fixes, suppressions       |
| `grebe-format` | the formatter                                        |
| `grebe-lsp`    | the language server                                  |
| `grebe-cli`    | the `grebe` binary                                   |

- **Parser.** `crates/grebe-syntax/vendor/` is DuckDB's grammar, vendored
  verbatim: don't hand-edit it. Where grebe and DuckDB disagree, record the
  statement in `crates/grebe-syntax/tests/fixtures/known-divergences.sql` with the reasoning.
- **Formatter.** Add a unit test in `crates/grebe-format/src/lib.rs` for any
  change in output. Formatting must stay idempotent and never change the
  statement's tokens.
- **Rules.** A rule's fixtures are a pair in
  `crates/grebe-rules/tests/fixtures/rules/`: `<CODE>.sql` with statements it
  must fire on and near-misses it must leave alone, and `<CODE>.expected`
  with its findings and fixes. After a deliberate change, regenerate with
  `GREBE_BLESS=1 cargo test -p grebe-rules --test rules` and review the diff.

## Pull requests

- One change per pull request, with tests.
- Commit messages: a short imperative summary line, as in `git log`.
- `unsafe` is forbidden workspace-wide.
- Don't bump versions; that happens at release.

## Accessibility

The extension's results grid, catalog and messages are webviews. Changes
there should work by keyboard alone, follow the VS Code theme (including
high-contrast themes), and give screen readers a label or text for anything
shown only visually. Reports of anything that falls short are bugs: use the
accessibility issue form.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
Security issues go through [SECURITY.md](SECURITY.md), not public issues.

By contributing you agree your work is licensed under the [MIT License](LICENSE).
