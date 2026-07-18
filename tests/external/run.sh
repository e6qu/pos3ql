#!/bin/zsh
# External conformance suite for pos3ql.
#
# Everything here tests from the OUTSIDE: the newest psql client (18.x)
# over the real wire, raw-socket protocol probes, the psycopg driver, and
# the newest MinIO as the object store. Nothing links against pos3ql.
#
# Requirements: docker, psql 18+ (brew install libpq), python3, cargo.
# Usage: tests/external/run.sh [--keep]

set -u
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
EXT=tests/external
WORK=$(mktemp -d /tmp/pos3ql-external.XXXXXX)
KEEP=${1:-}

PSQL=${POS3QL_PSQL:-/opt/homebrew/opt/libpq/bin/psql}
MINIO_PORT=${POS3QL_MINIO_PORT:-19311}
PG_PORT=${POS3QL_PG_PORT:-15433}
MINIO_CONTAINER=pos3ql-external-minio

PASS=0
FAIL=0
step() { print -- "\n=== $1 ==="; }
ok()   { PASS=$((PASS+1)); print -- "PASS: $1"; }
bad()  { FAIL=$((FAIL+1)); print -- "FAIL: $1"; }

cleanup() {
  [[ -n "${SERVER_PID:-}" ]] && kill "$SERVER_PID" 2>/dev/null
  docker rm -f $MINIO_CONTAINER >/dev/null 2>&1
  if [[ "$KEEP" == "--keep" ]]; then
    print -- "work dir kept: $WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

step "toolchain versions (targets: newest psql / MinIO)"
"$PSQL" --version || { bad "psql missing"; exit 1; }
docker --version >/dev/null || { bad "docker missing"; exit 1; }

step "build pos3ql (release)"
cargo build --release -q || { bad "build"; exit 1; }
ok "build"

step "start MinIO (latest) and create bucket"
docker rm -f $MINIO_CONTAINER >/dev/null 2>&1
docker run -d --name $MINIO_CONTAINER -p ${MINIO_PORT}:9000 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio:latest server /data >/dev/null || { bad "minio start"; exit 1; }
for i in {1..50}; do
  docker exec $MINIO_CONTAINER mc alias set local http://localhost:9000 minioadmin minioadmin >/dev/null 2>&1 && break
  sleep 0.2
done
docker exec $MINIO_CONTAINER mc mb --ignore-existing local/pos3ql-external >/dev/null || { bad "bucket"; exit 1; }
docker exec $MINIO_CONTAINER mc --version | head -1
ok "minio $(docker run --rm minio/minio:latest --version 2>/dev/null | head -1 | awk '{print $3}')"

step "write config and start pos3ql"
cat > "$WORK/server.conf" <<EOF
listen_addr = 127.0.0.1:${PG_PORT}
data_dir = ${WORK}/data
max_connections = 16
memtable_bytes = 16MiB
wal_bytes = 16MiB
s3 = on
s3_endpoint = 127.0.0.1:${MINIO_PORT}
s3_bucket = pos3ql-external
s3_prefix = run-$$/
s3_access_key = minioadmin
s3_secret_key = minioadmin
wal_upload = on
EOF
./target/release/pos3ql --config "$WORK/server.conf" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in {1..50}; do
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q -c "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q -c "SELECT 1" >/dev/null || { bad "server did not come up"; cat "$WORK/server.log"; exit 1; }
ok "server up (pid $SERVER_PID)"

psql_run() { # <name>
  local name=$1
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -a -q -P pager=off \
    -f "$EXT/sql/$name.sql" > "$WORK/$name.out" 2>&1
  if diff -u "$EXT/expected/$name.out" "$WORK/$name.out" > "$WORK/$name.diff"; then
    ok "psql golden: $name"
  else
    bad "psql golden: $name (see $WORK/$name.diff)"
    head -40 "$WORK/$name.diff"
  fi
}

step "psql golden tests (SQL dialect over the wire)"
psql_run basic
psql_run errors
psql_run extended

step "protocol 3.0 and 3.2 with the newest psql"
for v in 3.0 3.2; do
  out=$(PGMAXPROTOCOLVERSION=$v "$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -t -A -c "SELECT 'proto $v ok'" 2>&1)
  [[ "$out" == "proto $v ok" ]] && ok "psql protocol $v" || bad "psql protocol $v: $out"
done

step "raw wire-protocol probes (SSLRequest, negotiation, framing)"
if POS3QL_PORT=$PG_PORT python3 "$EXT/wire_probe.py" > "$WORK/wire.out" 2>&1; then
  ok "wire probes"
else
  bad "wire probes"; cat "$WORK/wire.out"
fi

step "driver test (psycopg 3, extended protocol with binary parameters)"
VENV="$ROOT/target/external-venv"
if [[ ! -x "$VENV/bin/python" ]]; then
  python3 -m venv "$VENV" && "$VENV/bin/pip" install --quiet 'psycopg[binary]'
fi
if POS3QL_PORT=$PG_PORT "$VENV/bin/python" - <<EOF > "$WORK/driver.out" 2>&1
import os, runpy, sys
sys.argv = ["driver_test.py"]
src = open("$EXT/driver_test.py").read().replace("port=5433", "port=$PG_PORT")
exec(compile(src, "driver_test.py", "exec"))
EOF
then
  ok "psycopg driver suite"
else
  bad "psycopg driver suite"; cat "$WORK/driver.out"
fi

step "durability: kill -9, restart, data intact"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q \
  -c "CREATE TABLE crashy (id int, v text)" \
  -c "INSERT INTO crashy VALUES (1,'pre-crash'),(2,'also here')"
# With asynchronous wal_upload, a commit is durable on local disk immediately
# but its S3 upload drains just after; a trailing query plus a short pause lets
# that drain reach MinIO before the abrupt kill, so the later disk-wipe steps
# (which recover from the bucket) are deterministic. Local recovery below does
# not depend on this — it replays the on-disk journal.
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q -c "SELECT 1" >/dev/null
sleep 1
kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
./target/release/pos3ql --config "$WORK/server.conf" >> "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in {1..50}; do
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q -c "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
out=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -t -A -c "SELECT count(*) FROM crashy" 2>&1)
[[ "$out" == "2" ]] && ok "kill -9 recovery" || bad "kill -9 recovery: '$out'"

step "async WAL upload: commit, wipe disk (no checkpoint), rebuild from MinIO WAL"
# wal_upload = on with the default asynchronous drain. Commit without any
# CHECKPOINT, then destroy the local disk: recovery must come entirely from the
# WAL segments the async drain uploaded to MinIO. A trailing SELECT plus a short
# pause guarantees the event loop has drained the commit's segment to the bucket.
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q \
  -c "CREATE TABLE waltest (id int, v text)" \
  -c "INSERT INTO waltest VALUES (10,'async-a'),(20,'async-b'),(30,'async-c')"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q -c "SELECT 1" >/dev/null
sleep 1
kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
rm -rf "$WORK/data"
./target/release/pos3ql --config "$WORK/server.conf" >> "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in {1..50}; do
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q -c "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
out=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -t -A -c "SELECT string_agg(v, ',' ORDER BY id) FROM waltest" 2>&1)
[[ "$out" == "async-a,async-b,async-c" ]] && ok "async WAL upload recovers from MinIO (no checkpoint)" || bad "async WAL recovery: '$out'"

step "cold start: checkpoint, wipe the disk, rebuild from MinIO"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q -c "CHECKPOINT"
kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
rm -rf "$WORK/data"
./target/release/pos3ql --config "$WORK/server.conf" >> "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for i in {1..50}; do
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -q -c "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
out=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U ext -X -t -A -c "SELECT v FROM crashy ORDER BY id LIMIT 1" 2>&1)
[[ "$out" == "pre-crash" ]] && ok "cold start from bucket" || bad "cold start from bucket: '$out'"

step "differential vs real PostgreSQL 18 (when installed)"
if [[ -x "${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}/postgres" ]]; then
  if tests/external/differential.sh > "$WORK/differential.out" 2>&1; then
    ok "differential suite ($(grep -c '^PASS' "$WORK/differential.out") corpora)"
  else
    bad "differential suite"; tail -30 "$WORK/differential.out"
  fi
else
  print -- "SKIP: real PostgreSQL 18 not installed"
fi

step "summary"
print -- "passed: $PASS  failed: $FAIL"
[[ $FAIL -eq 0 ]]
