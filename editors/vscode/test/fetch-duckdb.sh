#!/usr/bin/env bash
# Fetch a DuckDB CLI for the tests:  fetch-duckdb.sh VERSION DIR
# Leaves DIR/duckdb. The CLI is DuckDB's own build either way: the release
# archive from GitHub when the release has one under the usual name, else
# the duckdb-cli wheel from PyPI, which carries the same binary.
set -euo pipefail
version="$1"
dir="$2"
mkdir -p "$dir"
case "$(uname -m)" in
  x86_64 | amd64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "fetch-duckdb: no DuckDB CLI build for $(uname -m)" >&2; exit 1 ;;
esac
url="https://github.com/duckdb/duckdb/releases/download/v$version/duckdb_cli-linux-$arch.zip"
if curl -fsSL -o "$dir/duckdb.zip" "$url"; then
  unzip -qo "$dir/duckdb.zip" -d "$dir"
  rm -f "$dir/duckdb.zip"
else
  echo "fetch-duckdb: no $url; taking duckdb-cli==$version from PyPI" >&2
  rm -f "$dir/duckdb.zip"
  python3 -m pip download --quiet --no-deps --only-binary :all: --dest "$dir/wheel" "duckdb-cli==$version"
  unzip -qo -j "$dir"/wheel/*.whl 'duckdb_cli/duckdb' -d "$dir"
  rm -rf "$dir/wheel"
fi
chmod +x "$dir/duckdb"
got="$(DUCKDB_AGENT_MODE=0 "$dir/duckdb" -no-init -version)"
case "$got" in
  "v$version "*) echo "fetch-duckdb: $got" >&2 ;;
  *) echo "fetch-duckdb: wanted v$version, got: $got" >&2; exit 1 ;;
esac
