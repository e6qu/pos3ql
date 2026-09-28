#!/usr/bin/env bash
set -euo pipefail

ARCHIVE=$1
WORK=$(mktemp -d "${TMPDIR:-/tmp}/pos3ql-package-test.XXXXXX")
SERVER_PID=""
cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

tar -xzf "$ARCHIVE" -C "$WORK"
PACKAGE=$(find "$WORK" -mindepth 1 -maxdepth 1 -type d -name 'pos3ql-*')
[[ -x "$PACKAGE/bin/pos3ql" ]]
[[ -f "$PACKAGE/etc/pos3ql/pos3ql.conf" ]]
[[ -f "$PACKAGE/lib/systemd/system/pos3ql.service" ]]
[[ -f "$PACKAGE/share/doc/pos3ql/operations.md" ]]
"$PACKAGE/bin/pos3ql" --help | grep -q '^usage: pos3ql'

CONFIG="$WORK/smoke.conf"
sed \
  -e "s|^listen_addr = .*|listen_addr = 127.0.0.1:55433|" \
  -e "s|^operations_listen_addr = .*|operations_listen_addr = 127.0.0.1:59187|" \
  -e "s|^data_dir = .*|data_dir = $WORK/data|" \
  "$PACKAGE/etc/pos3ql/pos3ql.conf" > "$CONFIG"
"$PACKAGE/bin/pos3ql" --config "$CONFIG" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for attempt in $(seq 1 100); do
  if curl --fail --silent http://127.0.0.1:59187/livez \
      | grep -q '"status":"live"'; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    cat "$WORK/server.log" >&2
    exit 1
  fi
  [[ "$attempt" != 100 ]] || { cat "$WORK/server.log" >&2; exit 1; }
  sleep 0.05
done
kill "$SERVER_PID"
wait "$SERVER_PID"
SERVER_PID=""
grep -q 'shutdown complete' "$WORK/server.log"
