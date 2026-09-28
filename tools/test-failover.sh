#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} == --start-candidate ]]; then
  shift
  binary=$1
  config=$2
  log=$3
  pid_file=$4
  "$binary" --config "$config" >"$log" 2>&1 &
  printf '%d\n' "$!" >"$pid_file"
  exit 0
fi

BINARY=$1
MONITOR=$2
BASE_CONFIG=$3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/pos3ql-failover-test.XXXXXX")
PRIMARY_PID=""
CANDIDATE_PID=""
MONITOR_PID=""
STORE_PID=""
OLD_CLIENT_PID=""

stop_process() {
  local pid=$1
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill -CONT "$pid" 2>/dev/null || true
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 100); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.05
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
  fi
}
cleanup() {
  stop_process "$MONITOR_PID"
  stop_process "$OLD_CLIENT_PID"
  if [[ -z "$CANDIDATE_PID" && -s "$WORK/candidate.pid" ]]; then
    CANDIDATE_PID=$(cat "$WORK/candidate.pid")
  fi
  stop_process "$CANDIDATE_PID"
  stop_process "$PRIMARY_PID"
  stop_process "$STORE_PID"
  rm -rf "$WORK"
}
trap cleanup EXIT

umask 077
cat >"$WORK/credentials" <<'EOF'
access_key = failover-access
secret_key = failover-secret
EOF

make_config() {
  local destination=$1
  local data_dir=$2
  local postgres_port=$3
  local operations_port=$4
  sed \
    -e "s|^listen_addr = .*|listen_addr = 127.0.0.1:$postgres_port|" \
    -e "s|^operations_listen_addr = .*|operations_listen_addr = 127.0.0.1:$operations_port|" \
    -e "s|^data_dir = .*|data_dir = $data_dir|" \
    -e 's|^object_store = .*|object_store = on|' \
    "$BASE_CONFIG" >"$destination"
  cat >>"$destination" <<EOF
object_store_endpoint = 127.0.0.1:59220
object_store_bucket = pos3ql-failover
object_store_prefix = failover
object_store_region = test-region
object_store_credentials_file = $WORK/credentials
object_store_tls = off
EOF
}

make_config "$WORK/primary.conf" "$WORK/primary-data" 55440 59190
make_config "$WORK/candidate.conf" "$WORK/candidate-data" 55441 59191

python3 "$ROOT/tests/external/s3_test_server.py" \
  --root "$WORK/store" --port 59220 --bucket pos3ql-failover \
  --region test-region --access-key failover-access \
  --secret-key failover-secret >"$WORK/store.log" 2>&1 &
STORE_PID=$!
for attempt in $(seq 1 100); do
  if curl --silent --output /dev/null http://127.0.0.1:59220/; then
    break
  fi
  kill -0 "$STORE_PID" 2>/dev/null || { cat "$WORK/store.log" >&2; exit 1; }
  [[ "$attempt" != 100 ]] || { cat "$WORK/store.log" >&2; exit 1; }
  sleep 0.05
done

"$BINARY" --config "$WORK/primary.conf" >"$WORK/primary.log" 2>&1 &
PRIMARY_PID=$!
for attempt in $(seq 1 200); do
  if curl --fail --silent http://127.0.0.1:59190/readyz \
      | grep -q '"status":"ready"'; then
    break
  fi
  kill -0 "$PRIMARY_PID" 2>/dev/null || { cat "$WORK/primary.log" >&2; exit 1; }
  [[ "$attempt" != 200 ]] || { cat "$WORK/primary.log" >&2; exit 1; }
  sleep 0.05
done

python3 "$ROOT/tools/failover_old_session.py" --port 55440 \
  --ready-file "$WORK/old-client.ready" \
  --continue-file "$WORK/old-client.continue" \
  --result-file "$WORK/old-client.result" >"$WORK/old-client.log" 2>&1 &
OLD_CLIENT_PID=$!
for attempt in $(seq 1 200); do
  [[ -s "$WORK/old-client.ready" ]] && break
  kill -0 "$OLD_CLIENT_PID" 2>/dev/null || { cat "$WORK/old-client.log" >&2; exit 1; }
  [[ "$attempt" != 200 ]] || { cat "$WORK/old-client.log" >&2; exit 1; }
  sleep 0.05
done

"$MONITOR" \
  --primary-ready-url http://127.0.0.1:59190/readyz \
  --candidate-ready-url http://127.0.0.1:59191/readyz \
  --failure-threshold 2 --probe-interval-seconds 1 \
  --probe-timeout-seconds 1 --promotion-timeout-seconds 30 \
  --lock-directory "$WORK/monitor.lock" -- \
  "$0" --start-candidate "$BINARY" "$WORK/candidate.conf" \
  "$WORK/candidate.log" "$WORK/candidate.pid" \
  >"$WORK/monitor.out" 2>"$WORK/monitor.log" &
MONITOR_PID=$!
kill -STOP "$PRIMARY_PID"

for attempt in $(seq 1 500); do
  if ! kill -0 "$MONITOR_PID" 2>/dev/null; then
    break
  fi
  [[ "$attempt" != 500 ]] || { cat "$WORK/monitor.log" >&2; exit 1; }
  sleep 0.1
done
if ! wait "$MONITOR_PID"; then
  cat "$WORK/monitor.log" >&2
  cat "$WORK/candidate.log" >&2 || true
  exit 1
fi
MONITOR_PID=""
CANDIDATE_PID=$(cat "$WORK/candidate.pid")

grep -q 'event=promotion_started' "$WORK/monitor.log"
grep -q 'event=promotion_complete' "$WORK/monitor.log"
python3 "$ROOT/tools/pg-query.py" --port 55441 --expect 41 \
  "SELECT value FROM failover_value"

kill -CONT "$PRIMARY_PID"
for attempt in $(seq 1 100); do
  status=$(curl --silent --output "$WORK/old-ready.json" --write-out '%{http_code}' \
    --max-time 5 http://127.0.0.1:59190/readyz || true)
  [[ "$status" == 503 ]] && break
  [[ "$attempt" != 100 ]] || { cat "$WORK/old-ready.json" >&2; exit 1; }
  sleep 0.05
done
grep -q 'durable_progress_unavailable' "$WORK/old-ready.json"

touch "$WORK/old-client.continue"
if ! wait "$OLD_CLIENT_PID"; then
  cat "$WORK/old-client.log" >&2
  exit 1
fi
OLD_CLIENT_PID=""
grep -q '^fenced$' "$WORK/old-client.result"
python3 "$ROOT/tools/pg-query.py" --port 55441 --expect 1 \
  "SELECT count(*) FROM failover_value"

stop_process "$CANDIDATE_PID"
CANDIDATE_PID=""
stop_process "$PRIMARY_PID"
PRIMARY_PID=""
grep -q 'shutdown complete' "$WORK/candidate.log"
