#!/usr/bin/env bash
# Start a local ClickHouse server using the self-contained native binary (no Docker needed).
# HTTP on :8123, native TCP on :9000. Data + logs under clickhouse/_data.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CH="$HERE/bin/clickhouse"
DATA="$HERE/clickhouse/_data"
mkdir -p "$DATA"

if [ ! -x "$CH" ]; then
  echo "ClickHouse binary not found at $CH — run: curl -fsSL https://clickhouse.com/ | sh (then mv clickhouse bin/)" >&2
  exit 1
fi

case "${1:-server}" in
  server)
    echo "Starting ClickHouse server (http :8123, tcp :9000), data at $DATA ..."
    exec "$CH" server -- --path="$DATA/" --http_port=8123 --tcp_port=9000 \
        --logger.log="$DATA/clickhouse.log" --logger.errorlog="$DATA/clickhouse.err.log"
    ;;
  init)
    # Load schema via clickhouse-local-style client against a running server, or use `local` for a quick check.
    echo "Loading schema into running server on :9000 ..."
    "$CH" client --port 9000 --multiquery < "$HERE/clickhouse/schema/01_tables.sql"
    echo "Schema loaded."
    ;;
  client)
    shift || true
    exec "$CH" client --port 9000 "$@"
    ;;
  *)
    echo "usage: run_clickhouse.sh [server|init|client]" >&2; exit 2;;
esac
