#!/usr/bin/env bash
# Fetch a DuckDB CLI for the tests:  fetch-duckdb.sh VERSION DIR
# Leaves DIR/duckdb, from DuckDB's own release on GitHub. The archive was
# renamed with 2.0: duckdb-cli-linux-<arch>.tar.gz from 2.0.0 on,
# duckdb_cli-linux-<arch>.zip before. Both names are tried, newest first.
set -euo pipefail
version="$1"
dir="$2"
mkdir -p "$dir"
case "$(uname -m)" in
  x86_64 | amd64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "fetch-duckdb: no DuckDB CLI build for $(uname -m)" >&2; exit 1 ;;
esac
base="https://github.com/duckdb/duckdb/releases/download/v$version"
if curl -fsSL --retry 3 -o "$dir/duckdb.tar.gz" "$base/duckdb-cli-linux-$arch.tar.gz" 2>/dev/null; then
  tar xzf "$dir/duckdb.tar.gz" -C "$dir"
elif curl -fsSL --retry 3 -o "$dir/duckdb.zip" "$base/duckdb_cli-linux-$arch.zip"; then
  unzip -qo "$dir/duckdb.zip" -d "$dir"
else
  echo "fetch-duckdb: v$version has neither duckdb-cli-linux-$arch.tar.gz nor duckdb_cli-linux-$arch.zip at $base" >&2
  exit 1
fi
rm -f "$dir/duckdb.tar.gz" "$dir/duckdb.zip"
chmod +x "$dir/duckdb"
got="$(DUCKDB_AGENT_MODE=0 "$dir/duckdb" -no-init -version)"
case "$got" in
  "v$version "*) echo "fetch-duckdb: $got" >&2 ;;
  *) echo "fetch-duckdb: wanted v$version, got: $got" >&2; exit 1 ;;
esac
