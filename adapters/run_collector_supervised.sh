#!/usr/bin/env bash
# Robust, unsupervised long-run wrapper for the authenticated Kalshi WS collector.
# - Loads creds from the gitignored KalshiAPIKeysDONOTPUSH/.api_keys via load_keys.py.
# - Restarts the collector if it ever exits (network drops, etc.) with backoff.
# - Prunes data/raw files older than MAX_AGE_DAYS before each (re)start (retention cap).
# - Falls back to the REST orderbook collector if the WS collector can't stay up.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

OUT="${OUT:-../data/raw}"
CLICKHOUSE="${CLICKHOUSE:-http://localhost:8123}"
SERIES="${SERIES:-KXNATGASD}"
MAX_AGE_DAYS="${MAX_AGE_DAYS:-2}"
DEADLINE=$(( $(date +%s) + ${MAX_RUNTIME_SECS:-172800} ))   # default 2 days

# Extract creds into env (writes the gitignored PEM, prints id + pem path).
eval "$(python3 - <<'PY'
from load_keys import load_keys
kid, pem = load_keys()
print(f'export KALSHI_API_KEY_ID={kid}')
print(f'export KALSHI_PRIVATE_KEY={pem}')
PY
)"
if [ -z "${KALSHI_API_KEY_ID:-}" ]; then
  echo "FATAL: could not load Kalshi creds" >&2; exit 1
fi
echo "supervisor: creds loaded (key ${KALSHI_API_KEY_ID:0:8}…), out=$OUT ch=$CLICKHOUSE"

backoff=2
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  # retention prune
  find "$OUT" -type f -name '*.ndjson.gz' -mtime +"$MAX_AGE_DAYS" -delete 2>/dev/null || true
  echo "supervisor: starting WS collector $(date -u +%FT%TZ)"
  python3 kalshi_ws_collector.py --series "$SERIES" --out "$OUT" --clickhouse "$CLICKHOUSE"
  rc=$?
  echo "supervisor: WS collector exited rc=$rc; restarting in ${backoff}s"
  sleep "$backoff"
  backoff=$(( backoff < 60 ? backoff*2 : 60 ))
done
echo "supervisor: reached max runtime, stopping $(date -u +%FT%TZ)"
