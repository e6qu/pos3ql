#!/usr/bin/env bash
# External conformance suite for pos3ql.
#
# Everything here tests from the OUTSIDE: the newest psql client (18.x)
# over the real wire, raw-socket protocol probes, and the psycopg driver.
#
# Requirements: psql 18+ (brew install libpq), python3, cargo.
# Usage: tests/external/run.sh [--keep]

set -u
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
EXT=tests/external
WORK=$(mktemp -d /tmp/pos3ql-external.XXXXXX)
KEEP=${1:-}

PSQL=${POS3QL_PSQL:-/opt/homebrew/opt/libpq/bin/psql}
GATEWAY_PORT=${POS3QL_GATEWAY_PORT:-19311}
PG_PORT=${POS3QL_PG_PORT:-15433}

PASS=0
FAIL=0

# The suite is sharded on CI so no job runs past its time budget:
# POS3QL_RUN_GROUPS selects a comma-separated subset of the step groups
# below, and unset (or "all") runs everything. The groups are independent —
# each talks to its own tables, server or bucket prefix — so any subset is
# a complete run of what it selects.
#   proto      psql golden files, protocol versions, raw wire probes,
#              the psycopg driver suite and the \copy round trip
#   dur        durability cycles on the main server: kill -9 recovery,
#              durable WAL rebuild, commit-durable-on-bucket, cold start
#   overlay    row-map pressure: 5000 rows through a 1024-entry map, plus
#              value-indexed uniqueness enforcement across the spill boundary
#   ingest     beyond-memtable ingest, paced compaction, cold starts
#   torture    randomized crash torture against real PostgreSQL
#              (POS3QL_TORTURE_ROUNDS / POS3QL_TORTURE_SEED size one run,
#              so CI can split the depth across seeds)
#   tls        the durability cycle over HTTPS
#   subscription a real PostgreSQL logical publisher, transactional apply,
#              acknowledgement, and subscriber crash recovery
#   diff       the plain differential suite against PostgreSQL
#   spilldiff  the forced-spill differential suite against PostgreSQL
SELECTED_GROUPS=${POS3QL_RUN_GROUPS:-all}
want() { [[ "$SELECTED_GROUPS" == all || ",$SELECTED_GROUPS," == *",$1,"* ]]; }

# Every step reports its wall-clock cost so a slow CI shard names its
# culprit instead of being a 15-minute mystery.
STEP_TITLE=""
STEP_STARTED=0
step_close() {
  [[ -n "$STEP_TITLE" ]] && printf 'step time: %.1fs  (%s)\n' $((SECONDS - STEP_STARTED)) "$STEP_TITLE"
}
step() { step_close; STEP_TITLE=$1; STEP_STARTED=$SECONDS; printf '\n=== %s ===\n' "$1"; }
ok()   { PASS=$((PASS+1)); printf 'PASS: %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf 'FAIL: %s\n' "$1"; }

# Start a pos3ql server and wait for it to answer. A listener already on the
# port is never ours — a server leaked by an earlier interrupted run would
# silently serve the whole suite while the fresh binary exits on its bind
# failure (under coverage that reads as the external layer contributing no
# profile). Refuse instead. And the probe succeeding only proves *a*
# server answered, so the started process must still be alive afterwards.
START_PID=0
. "$EXT/liveness.sh"

start_pos3ql() { # <config> <log> <port>
  local conf=$1 log=$2 port=$3
  if nc -z 127.0.0.1 $port 2>/dev/null; then
    bad "port $port is already in use (stale pos3ql from an earlier run?) — kill it first"
    exit 1
  fi
  "${POS3QL_BIN:-./target/release/pos3ql}" --config "$conf" >> "$log" 2>&1 &
  START_PID=$!
  for _ in {1..50}; do
    "$PSQL" -h 127.0.0.1 -p $port -U postgres -X -q -c "SELECT 1" >/dev/null 2>&1 && break
    sleep 0.1
  done
  if ! server_alive "$START_PID"; then
    bad "pos3ql under test exited at startup (see $log)"
    tail -10 "$log"
    exit 1
  fi
}

cleanup() {
  # Stop the base-port server gracefully and wait for it to exit before
  # returning. Under coverage the server flushes its profile on a clean
  # shutdown (a SIGKILL never would), and the caller reads target/ the moment
  # this script exits — so a fire-and-forget SIGTERM races the flush. Give it
  # up to five seconds to go, then force it.
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null
    for _ in {1..50}; do kill -0 "$SERVER_PID" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$SERVER_PID" 2>/dev/null
  fi
  if [[ -n "${GATEWAY_PID:-}" ]]; then
    kill "$GATEWAY_PID" 2>/dev/null
  fi
  if [[ "$KEEP" == "--keep" ]]; then
    printf 'work dir kept: %s\n' "$WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

step "toolchain versions"
"$PSQL" --version || { bad "psql missing"; exit 1; }

step "build pos3ql (release)"
cargo build --release -q || { bad "build"; exit 1; }
ok "build"

step "start generic object-store gateway"
python3 "$EXT/object_store_gateway.py" --root "$WORK/object-store" --port "$GATEWAY_PORT" \
  > "$WORK/object-store.log" 2>&1 &
GATEWAY_PID=$!
for i in {1..50}; do
  nc -z 127.0.0.1 "$GATEWAY_PORT" && break
  sleep 0.1
done
ok "gateway (pid $GATEWAY_PID)"

step "write config and start pos3ql"
cat > "$WORK/server.conf" <<EOF
listen_addr = 127.0.0.1:${PG_PORT}
data_dir = ${WORK}/data
max_connections = 8
memtable_bytes = 16MiB
wal_bytes = 16MiB
object_store = on
object_store_endpoint = 127.0.0.1:${GATEWAY_PORT}
object_store_namespace = pos3ql-external
object_store_prefix = run-$$/
wal_upload = on
wal_upload_sync = on
sql_arena_bytes = 4MiB
wal_buffer_bytes = 4MiB
max_tables = 64
table_rows = 8192
max_value_indexes = 64
max_subscriptions = 2
subscription_relation_capacity = 16
subscription_arena_bytes = 256KiB
# A full scan of spilled rows stages them in the statement work arena (the
# streaming read path is a later stage); size it for the spilled dataset.
work_arena_bytes = 192MiB
# Smaller than one committed WAL batch can be (wal_buffer is 4MiB): the
# object-WAL recovery below proves segments larger than the response buffer
# stream back in ranged windows.
object_store_response_bytes = 256KiB
EOF
start_pos3ql "$WORK/server.conf" "$WORK/server.log" $PG_PORT
SERVER_PID=$START_PID
ok "server up (pid $SERVER_PID)"

psql_run() { # <name>
  local name=$1
  # psql writes echoed SQL and result rows to stdout but errors to stderr.
  # Their ordering after `2>&1` is scheduler-dependent, so compare each
  # stream against the corresponding portion of the established golden file.
  local expected_out="$WORK/$name.expected.out" expected_err="$WORK/$name.expected.err"
  awk -v out="$expected_out" -v err="$expected_err" \
    '/^psql:/ { print > err; next } { print > out }' \
    "$EXT/expected/$name.out"
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -a -q -P pager=off \
    -f "$EXT/sql/$name.sql" > "$WORK/$name.out" 2> "$WORK/$name.err"
  if diff -u "$expected_out" "$WORK/$name.out" > "$WORK/$name.diff" \
    && diff -u "$expected_err" "$WORK/$name.err" >> "$WORK/$name.diff"; then
    ok "psql golden: $name"
  else
    bad "psql golden: $name (see $WORK/$name.diff)"
    head -40 "$WORK/$name.diff"
  fi
}

if want proto; then

step "psql golden tests (SQL dialect over the wire)"
psql_run basic
psql_run errors
psql_run extended

step "protocol 3.0 and 3.2 with the newest psql"
for v in 3.0 3.2; do
  out=$(PGMAXPROTOCOLVERSION=$v "$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -c "SELECT 'proto $v ok'" 2>&1)
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

step "COPY: client-side round trip through psql \\copy"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
  -c "CREATE TABLE copy_rt (id int, v text, w text)" \
  -c "INSERT INTO copy_rt VALUES (1, E'tab\\there', 'plain'), (2, E'nl\\nhere', NULL), (3, 'back\\slash', 'x')"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q -c "\\copy copy_rt TO '$WORK/copy_rt.tsv'"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
  -c "CREATE TABLE copy_rt2 (id int, v text, w text)" \
  -c "\\copy copy_rt2 FROM '$WORK/copy_rt.tsv'"
out=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A \
  -c "SELECT count(*) FROM copy_rt2 t2 JOIN copy_rt t ON t.id = t2.id AND t.v = t2.v AND t.w IS NOT DISTINCT FROM t2.w" 2>&1)
[[ "$out" == "3" ]] && ok "psql \\copy round trip (escapes and NULLs intact)" \
  || bad "copy round trip: '$out'"

fi # proto

if want dur; then

step "durability: kill -9, restart, data intact"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
  -c "CREATE TABLE crashy (id int, v text)" \
  -c "INSERT INTO crashy VALUES (1,'pre-crash'),(2,'also here')" \
  -c "CREATE TABLE crashy_types (a int[], b bool[], c text[], m tsmultirange, r int4range, ip inet, mac macaddr)" \
  -c "INSERT INTO crashy_types VALUES ('{1,2}','{t,f}','{x}','{[2020-01-01,2020-02-01)}','[1,5)','2001:db8::1/64','08:00:2b:01:02:03')" \
  -c "CREATE TABLE crashy_seq (id serial, v int)" \
  -c "INSERT INTO crashy_seq(v) VALUES (1),(2),(3)" \
  -c "TRUNCATE crashy_seq" \
  -c "CREATE SCHEMA crashy_ns" \
  -c "CREATE TABLE crashy_ns.t (a int)" \
  -c "INSERT INTO crashy_ns.t VALUES (7)" \
  -c "CREATE VIEW crashy_ns.v AS SELECT a FROM crashy_ns.t" \
  -c "COMMENT ON TABLE crashy IS 'crash-comment'" \
  -c "COMMENT ON COLUMN crashy.v IS 'crash-col'" \
  -c "CREATE DOMAIN crashy_pos AS int CHECK (VALUE > 0)" \
  -c "CREATE DOMAIN crashy_small AS crashy_pos DEFAULT 7 CHECK (VALUE < 100)" \
  -c "CREATE TABLE crashy_dom (n crashy_small, ns crashy_small[])" \
  -c "INSERT INTO crashy_dom VALUES (42, ARRAY[1,2]::crashy_small[])" \
  -c "CREATE TYPE crashy_mood AS ENUM ('sad','ok','happy')" \
  -c "CREATE TABLE crashy_enum (id int, m crashy_mood, ms crashy_mood[])" \
  -c "INSERT INTO crashy_enum VALUES (1,'happy',ARRAY['happy','ok']::crashy_mood[]),(2,'sad',ARRAY['sad']::crashy_mood[])" \
  -c "ALTER TYPE crashy_mood RENAME VALUE 'happy' TO 'glad'" \
  -c "ALTER TYPE crashy_mood RENAME TO crashy_feeling"
# WAL upload is synchronous in durable mode. The trailing query still gives
# the event loop an ordinary turn before this broader crash/restart fixture;
# local recovery below replays the on-disk journal.
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q -c "SELECT 1" >/dev/null
sleep 1
kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
start_pos3ql "$WORK/server.conf" "$WORK/server.log" $PG_PORT
SERVER_PID=$START_PID
# A column's type is stored as a one-byte code; two families once shared codes,
# so an int4[]/bool[] column came back as a multirange with its values gone.
types=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' \
  -c "SELECT pg_typeof(a),pg_typeof(b),pg_typeof(c),pg_typeof(m),pg_typeof(r) FROM crashy_types" 2>&1)
want="integer[]|boolean[]|text[]|tsmultirange|int4range"
# A serial column's sequence position survives the crash even with the rows
# gone: a max-based scan would restart at 1 and reuse committed ids.
seq_id=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -q \
  -c "INSERT INTO crashy_seq(v) VALUES (9) RETURNING id" 2>&1 | head -1)
[[ "$seq_id" == "4" ]] && ok "serial sequence survives restart" \
  || bad "serial sequence survives restart (got: $seq_id)"
# Schemas, their tables and their views survive the crash: the journal
# replays CREATE SCHEMA and the qualified objects, and the view still
# resolves under its stored creation path.
ns=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -q \
  -c "SELECT (SELECT count(*) FROM crashy_ns.t), (SELECT a FROM crashy_ns.v), (SELECT count(*) FROM pg_namespace WHERE nspname = 'crashy_ns')" 2>&1)
[[ "$ns" == "1|7|1" ]] && ok "schema objects survive restart" \
  || bad "schema objects after restart: '$ns'"
[[ "$types" == "$want" ]] && ok "column types survive restart" \
  || bad "column types after restart: got '$types' want '$want'"
vals=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -c "SELECT a,b,c FROM crashy_types" 2>&1)
[[ "$vals" == "{1,2}|{t,f}|{x}" ]] && ok "array values survive restart" \
  || bad "array values after restart: '$vals'"
# Network address values survive the crash (the inet/macaddr row codec).
net=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -c "SELECT ip, mac FROM crashy_types" 2>&1)
[[ "$net" == "2001:db8::1/64|08:00:2b:01:02:03" ]] && ok "network values survive restart" \
  || bad "network values after restart: '$net'"
out=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -c "SELECT count(*) FROM crashy" 2>&1)
[[ "$out" == "2" ]] && ok "kill -9 recovery" || bad "kill -9 recovery: '$out'"
# A domain and its column identity + constraints survive the crash: the journal
# replays CREATE DOMAIN, and the domain still enforces and reports its name.
dom=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -q \
  -c "SELECT pg_typeof(n), n, ns::text FROM crashy_dom" 2>&1)
# The domain's CHECK still enforces after replay (psql default verbosity prints
# the message, not the SQLSTATE, so match the message).
dom_bad=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -q \
  -c "INSERT INTO crashy_dom(n) VALUES (-1)" 2>&1 | grep -c 'violates check constraint')
dom_array_bad=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -q \
  -c "INSERT INTO crashy_dom VALUES (50, ARRAY[101]::crashy_small[])" 2>&1 | grep -c 'violates check constraint')
[[ "$dom" == "crashy_small|42|{1,2}" && "$dom_bad" -ge 1 && "$dom_array_bad" -ge 1 ]] && ok "domains survive restart" \
  || bad "domains after restart: '$dom' / '$dom_bad' / '$dom_array_bad'"
# An enum, its ordering, column identity and label enforcement survive the crash:
# the journal replays CREATE TYPE and the enum-typed column binds back to it.
enm=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -q \
  -c "SELECT pg_typeof(m), string_agg(id::text, ',' ORDER BY m) FROM crashy_enum GROUP BY m ORDER BY m" 2>&1 | tr '\n' ';')
enm_bad=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -q \
  -c "INSERT INTO crashy_enum VALUES (3,'bogus',NULL)" 2>&1 | grep -c 'invalid input value for enum')
[[ "$enm" == "crashy_feeling|2;crashy_feeling|1;" && "$enm_bad" -ge 1 ]] && ok "enums survive restart" \
  || bad "enums after restart: '$enm' / '$enm_bad'"
# Object comments survive the crash: the journal replays the COMMENT records.
cmt=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -q \
  -c "SELECT obj_description('crashy'::regclass), col_description('crashy'::regclass, 2)" 2>&1)
[[ "$cmt" == "crash-comment|crash-col" ]] && ok "comments survive restart" \
  || bad "comments after restart: '$cmt'"

step "durable WAL upload: commit, wipe disk (no checkpoint), rebuild from MinIO WAL"
# Commit without any CHECKPOINT, then destroy the local disk: recovery must
# come entirely from WAL segments synchronously acknowledged by MinIO.
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
  -c "CREATE TABLE waltest (id int, v text)" \
  -c "INSERT INTO waltest VALUES (10,'durable-a'),(20,'durable-b'),(30,'durable-c')"
# One commit whose WAL batch (~600 KiB of row images, within the statement
# arena) exceeds the 256 KiB response buffer: its uploaded segment must
# still replay, in ranged windows, after the wipe.
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
  -c "INSERT INTO waltest SELECT 1000 + g, repeat('w', 1024) FROM generate_series(1, 600) g"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q -c "SELECT 1" >/dev/null
sleep 1
kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
rm -rf "$WORK/data"
start_pos3ql "$WORK/server.conf" "$WORK/server.log" $PG_PORT
SERVER_PID=$START_PID
out=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -c "SELECT (SELECT string_agg(v, ',' ORDER BY id) FROM waltest WHERE id < 1000), (SELECT count(*) FROM waltest WHERE id >= 1000)" 2>&1)
[[ "$out" == "durable-a,durable-b,durable-c|600" ]] && ok "durable WAL upload recovers through the gateway (segments beyond the response buffer)" || bad "durable WAL recovery: '$out'"

step "commit-durable-on-bucket by default: ack, kill -9 at once, wipe, cold start"
# A config that says nothing but `object_store = on` gets the plan-of-record
# posture: the commit batch is published before acknowledgement, so the
# kill needs no drain pause and the wiped-disk recovery needs no checkpoint.
cat > "$WORK/rpo0.conf" <<EOF
listen_addr = 127.0.0.1:$((PG_PORT + 2))
data_dir = ${WORK}/rpo0-data
object_store = on
object_store_endpoint = 127.0.0.1:${GATEWAY_PORT}
object_store_namespace = pos3ql-external
object_store_prefix = rpo0-$$/
EOF
start_pos3ql "$WORK/rpo0.conf" "$WORK/rpo0.log" $((PG_PORT + 2))
RPO0_PID=$START_PID
"$PSQL" -h 127.0.0.1 -p $((PG_PORT + 2)) -U postgres -X -q \
  -c "CREATE TABLE rpo0 (id int, v text)" \
  -c "INSERT INTO rpo0 VALUES (1,'acked-then-killed'),(2,'still-here')"
kill -9 $RPO0_PID 2>/dev/null; wait $RPO0_PID 2>/dev/null
rm -rf "$WORK/rpo0-data"
start_pos3ql "$WORK/rpo0.conf" "$WORK/rpo0.log" $((PG_PORT + 2))
RPO0_PID=$START_PID
out=$("$PSQL" -h 127.0.0.1 -p $((PG_PORT + 2)) -U postgres -X -t -A \
  -c "SELECT string_agg(v, ',' ORDER BY id) FROM rpo0" 2>&1)
kill -9 $RPO0_PID 2>/dev/null; wait $RPO0_PID 2>/dev/null
[[ "$out" == "acked-then-killed,still-here" ]] \
  && ok "commit-durable-on-bucket by default (no drain pause, no checkpoint)" \
  || bad "commit-durable-on-bucket default: '$out'"

step "cold start: checkpoint, wipe the disk, rebuild from MinIO"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q -c "CHECKPOINT"
kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
rm -rf "$WORK/data"
start_pos3ql "$WORK/server.conf" "$WORK/server.log" $PG_PORT
SERVER_PID=$START_PID
out=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -c "SELECT v FROM crashy ORDER BY id LIMIT 1" 2>&1)
[[ "$out" == "pre-crash" ]] && ok "cold start from bucket" || bad "cold start from bucket: '$out'"
# Schemas and their contents rebuild from the manifest alone (wiped disk).
ns=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -q \
  -c "SELECT (SELECT count(*) FROM crashy_ns.t), (SELECT a FROM crashy_ns.v)" 2>&1)
[[ "$ns" == "1|7" ]] && ok "schema objects survive a cold start" \
  || bad "schema objects after cold start: '$ns'"
# Object comments rebuild from the manifest alone (the `cmt` line), wiped disk.
cmt=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -q \
  -c "SELECT obj_description('crashy'::regclass), col_description('crashy'::regclass, 2)" 2>&1)
[[ "$cmt" == "crash-comment|crash-col" ]] && ok "comments survive a cold start" \
  || bad "comments after cold start: '$cmt'"
# Domains rebuild from the manifest `dom2` line alone (wiped disk), including
# immediate parent identity and generated array identity.
dom=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -q \
  -c "SELECT pg_typeof(n), n, ns::text FROM crashy_dom" 2>&1)
[[ "$dom" == "crashy_small|42|{1,2}" ]] && ok "domains survive a cold start" \
  || bad "domains after cold start: '$dom'"
# Enums rebuild from the manifest `enm` line alone (wiped disk); the enum-typed
# column binds back to it and still reports and enforces its type.
enm=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' -q \
  -c "SELECT pg_typeof(m), m, ms::text FROM crashy_enum WHERE id = 1" 2>&1)
[[ "$enm" == "crashy_feeling|glad|{glad,ok}" ]] && ok "enums survive a cold start" \
  || bad "enums after cold start: '$enm'"

fi # dur

if want overlay; then

step "row count beyond table_rows: the map is an overlay, the bucket is the table"
# A table_rows far below the dataset: the row map holds only the working
# set, and reads, counts, updates, deletes and the cold start below all
# reach entry-less rows through the spill list.
cat > "$WORK/overlay.conf" <<EOF
listen_addr = 127.0.0.1:$((PG_PORT + 3))
data_dir = ${WORK}/overlay-data
memtable_bytes = 512KiB
table_rows = 1024
object_store = on
object_store_endpoint = 127.0.0.1:${GATEWAY_PORT}
object_store_namespace = pos3ql-external
object_store_prefix = overlay-$$/
work_arena_bytes = 96MiB
EOF
start_pos3ql "$WORK/overlay.conf" "$WORK/overlay.log" $((PG_PORT + 3))
OVERLAY_PID=$START_PID
# The scale table carries a PRIMARY KEY: the value index makes a
# uniqueness probe a hash seek against the committed rows rather than a scan of
# the whole spilled SST forest, so a constrained table spills and grows past its
# overlay without the old quadratic. This exercises the overlay/spill read path
# (count, point read, update, delete, cold start) over a constrained dataset far
# larger than the map.
"$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -q \
  -c "CREATE TABLE big (id int PRIMARY KEY, v text)"
# 5000 rows through a 1024-entry map: batches with checkpoints between, so
# entries spill and evict as the data outgrows the overlay.
for batch in 0 1 2 3 4; do
  "$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -q \
    -c "INSERT INTO big SELECT $batch * 1000 + g, 'r' || ($batch * 1000 + g) FROM generate_series(0, 999) g" \
    -c "CHECKPOINT"
done
"$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -q \
  -c "DELETE FROM big WHERE id % 100 = 7" \
  -c "UPDATE big SET v = 'updated' WHERE id = 4321" \
  -c "CHECKPOINT"
out=$("$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -t -A -F'|' \
  -c "SELECT (SELECT count(*) FROM big), (SELECT v FROM big WHERE id = 4321), (SELECT count(*) FROM big WHERE id % 100 = 7), (SELECT v FROM big WHERE id = 2500)" 2>&1)
[[ "$out" == "4950|updated|0|r2500" ]] && ok "5000 rows through a 1024-entry map" \
  || bad "overlay row count: '$out'"

# Uniqueness must hold across the spill boundary: a duplicate of a key long
# evicted from the overlay must still be caught against its spilled row, not
# silently inserted. Now that the probe is a value-index seek, this runs
# at 5000 rows — far past the 1024 map — without the old quadratic cost.
"$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -q \
  -c "CREATE TABLE uniq (id int PRIMARY KEY, v text)"
for batch in 0 1 2 3 4 5 6 7 8 9; do
  "$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -q \
    -c "INSERT INTO uniq SELECT $batch * 500 + g, 'r' || ($batch * 500 + g) FROM generate_series(0, 499) g" \
    -c "CHECKPOINT"
done
dup_spilled=$("$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -t -A \
  -c "INSERT INTO uniq VALUES (5, 'dup')" 2>&1 | head -1)
dup_fresh=$("$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -t -A \
  -c "INSERT INTO uniq VALUES (99999, 'fresh')" 2>&1 | head -1)
uniq_count=$("$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -t -A \
  -c "SELECT count(*) FROM uniq" 2>&1)
[[ "$dup_spilled" == *"duplicate key value"* && "$dup_fresh" == "INSERT 0 1" && "$uniq_count" == "5001" ]] \
  && ok "uniqueness enforced across the spill boundary at scale" \
  || bad "spill-boundary uniqueness: dup='$dup_spilled' fresh='$dup_fresh' count='$uniq_count'"

kill -9 $OVERLAY_PID 2>/dev/null; wait $OVERLAY_PID 2>/dev/null
rm -rf "$WORK/overlay-data"
start_pos3ql "$WORK/overlay.conf" "$WORK/overlay.log" $((PG_PORT + 3))
OVERLAY_PID=$START_PID
out=$("$PSQL" -h 127.0.0.1 -p $((PG_PORT + 3)) -U postgres -X -t -A -F'|' \
  -c "SELECT count(*), (SELECT v FROM big WHERE id = 4321) FROM big" 2>&1 | head -1)
kill -9 $OVERLAY_PID 2>/dev/null; wait $OVERLAY_PID 2>/dev/null
[[ "$out" == "4950|updated" ]] && ok "wiped-disk cold start of a dataset larger than table_rows" \
  || bad "overlay cold start: '$out'"

fi # overlay

if want ingest; then

step "ingest beyond memtable_bytes: rows spill to the bucket and read back"
# The Stage D milestone: sustained inserts well past the heap's capacity,
# with checkpoints spilling committed bytes to block SSTs. Reads (point and
# aggregate) then fetch spilled rows back through the cache tiers.
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q -c "CREATE TABLE spilly (id serial, pad text)"
# ~24 MiB of row bytes against a 16 MiB memtable, in modest batches so the
# auto-checkpoint between messages can drain the heap.
for i in {1..24}; do
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q     -c "INSERT INTO spilly(pad) SELECT repeat('x', 1024) FROM generate_series(1, 1000)"     || { bad "spill ingest batch $i"; break; }
done
spill_count=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -c "SELECT count(*) FROM spilly" 2>&1)
[[ "$spill_count" == "24000" ]] && ok "ingest 1.5x memtable_bytes (24000 rows)"   || bad "ingest beyond memtable (count: $spill_count)"
spill_point=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -c "SELECT length(pad) FROM spilly WHERE id = 12345" 2>&1)
[[ "$spill_point" == "1024" ]] && ok "point read of a spilled row"   || bad "point read of a spilled row (got: $spill_point)"
# Deltas and tombstones across a cold start: delete a slice of spilled rows,
# update one, checkpoint (a delta SST with tombstones joins the table's list),
# wipe the disk, and rebuild from the bucket. The deleted rows must not
# resurrect from older SSTs; the update must win over its old version.
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
  -c "DELETE FROM spilly WHERE id BETWEEN 100 AND 599" \
  -c "UPDATE spilly SET pad = 'updated' WHERE id = 700" \
  -c "CHECKPOINT"
kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
rm -rf "$WORK/data"
start_pos3ql "$WORK/server.conf" "$WORK/server.log" $PG_PORT
SERVER_PID=$START_PID
after=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' \
  -c "SELECT count(*), count(*) FILTER (WHERE id BETWEEN 100 AND 599), max(CASE WHEN id = 700 THEN pad END) FROM spilly" 2>&1)
[[ "$after" == "23500|0|updated" ]] && ok "delta SSTs + tombstones survive a cold start" \
  || bad "delta/tombstone cold start (got: $after)"
# Paced compaction: enough checkpointed delta cycles to cross the merge
# trigger several times over, with interleaved deletes and updates so merges
# see duplicates and tombstones. Every value must survive the merges, the
# repointed spilled rows must still point-read, and a final wiped-disk cold
# start must rebuild the merged lists from the manifest alone.
for i in {1..7}; do
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
    -c "INSERT INTO spilly(pad) SELECT repeat('m', 512) FROM generate_series(1, 200)" \
    -c "DELETE FROM spilly WHERE id BETWEEN $((23000 + i * 100)) AND $((23000 + i * 100 + 49))" \
    -c "UPDATE spilly SET pad = 'cycle-$i' WHERE id = 650" \
    -c "CHECKPOINT" \
    || { bad "paced merge cycle $i"; break; }
done
merged=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' \
  -c "SELECT count(*), (SELECT pad FROM spilly WHERE id = 650), (SELECT count(*) FROM spilly WHERE id BETWEEN 23100 AND 23749), (SELECT length(pad) FROM spilly WHERE id = 12345) FROM spilly" 2>&1)
[[ "$merged" == "24550|cycle-7|300|1024" ]] && ok "paced compaction keeps every row" \
  || bad "paced compaction (got: $merged)"
kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
rm -rf "$WORK/data"
start_pos3ql "$WORK/server.conf" "$WORK/server.log" $PG_PORT
SERVER_PID=$START_PID
merged_cold=$("$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' \
  -c "SELECT count(*), (SELECT pad FROM spilly WHERE id = 650) FROM spilly" 2>&1)
[[ "$merged_cold" == "24550|cycle-7" ]] && ok "merged SST lists survive a cold start" \
  || bad "merged-list cold start (got: $merged_cold)"
"$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q -c "DROP TABLE spilly" >/dev/null 2>&1

fi # ingest

if want torture; then

step "crash torture: random DML + kill -9 + cold starts vs real PostgreSQL"
TORTURE_PGBIN="${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}"
if [[ -n "${POS3QL_VENV:-}" && -x "$POS3QL_VENV/bin/python" && ( -x "$TORTURE_PGBIN/postgres" || -n "${POS3QL_REFERENCE_PG_HOST:-}" ) ]]; then
  # Local runs own a hermetic cluster. CI supplies the same PostgreSQL 18
  # service used by the other oracle jobs, avoiding a slow package download
  # before the bounded torture workload begins.
  TORTURE_PG_HOST=127.0.0.1
  TORTURE_PG_PORT=15497
  TORTURE_OWNS_REFERENCE=true
  if [[ ! -x "$TORTURE_PGBIN/postgres" ]]; then
    TORTURE_PG_HOST=$POS3QL_REFERENCE_PG_HOST
    TORTURE_PG_PORT=${POS3QL_REFERENCE_PG_PORT:-5432}
    TORTURE_OWNS_REFERENCE=false
  fi
  if [[ $TORTURE_OWNS_REFERENCE == true ]]; then
  "$TORTURE_PGBIN/initdb" -D "$WORK/torture-pgdata" -U postgres -A trust \
    --encoding=UTF8 --lc-collate=C --lc-ctype=C >/dev/null 2>&1
  TORTURE_SOCK=$(mktemp -d /tmp/pos3ql-torture-sock.XXXX)
  "$TORTURE_PGBIN/pg_ctl" -D "$WORK/torture-pgdata" \
    -o "-p $TORTURE_PG_PORT -k $TORTURE_SOCK -c listen_addresses=127.0.0.1 -c timezone=UTC" \
    -l "$WORK/torture-pg.log" start >/dev/null
  fi
  if P3_BIN="${POS3QL_BIN:-./target/release/pos3ql}" P3_CONF="$WORK/server.conf" \
     P3_PORT=$PG_PORT P3_DATADIR="$WORK/data" P3_LOG="$WORK/server.log" P3_INITIAL_PID=$SERVER_PID \
     PGHOST=$TORTURE_PG_HOST PGPORT=$TORTURE_PG_PORT PGUSER=postgres PGDATABASE=postgres \
     "$POS3QL_VENV/bin/python" tests/external/torture_diff.py \
       --rounds "${POS3QL_TORTURE_ROUNDS:-12}" --seed "${POS3QL_TORTURE_SEED:-20260723}" \
       > "$WORK/torture.out" 2>&1; then
    ok "crash torture ($(tail -1 "$WORK/torture.out"))"
  else
    bad "crash torture"
    tail -40 "$WORK/torture.out"
    printf '%s\n' '--- pos3ql server log after crash torture ---'
    tail -80 "$WORK/server.log"
  fi
  if [[ $TORTURE_OWNS_REFERENCE == true ]]; then
    "$TORTURE_PGBIN/pg_ctl" -D "$WORK/torture-pgdata" stop -m immediate >/dev/null 2>&1
    rm -rf "$TORTURE_SOCK"
  fi
  # The torture script may have restarted the server under its own pid.
  lsof -ti tcp:$PG_PORT -sTCP:LISTEN | xargs kill -9 2>/dev/null || true
  sleep 0.3
  start_pos3ql "$WORK/server.conf" "$WORK/server.log" $PG_PORT
  SERVER_PID=$START_PID
else
  printf '%s\n' 'SKIP: torture needs POS3QL_VENV and a reference PostgreSQL'
fi

fi # torture

if want subscription; then

step "logical subscription apply from a real PostgreSQL publisher"
SUB_PGBIN="${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}"
SUB_PG_PORT=15496
if [[ -x "$SUB_PGBIN/postgres" ]]; then
  if ! "$SUB_PGBIN/initdb" -D "$WORK/subscription-pgdata" -U postgres -A trust \
    --encoding=UTF8 --lc-collate=C --lc-ctype=C > "$WORK/subscription-initdb.log" 2>&1; then
    bad "logical subscription publisher initialization"
    tail -20 "$WORK/subscription-initdb.log"
  else
  SUB_PG_SOCK=$(mktemp -d /tmp/pos3ql-subscription-pgsock.XXXX)
  if ! "$SUB_PGBIN/pg_ctl" -D "$WORK/subscription-pgdata" \
    -o "-p $SUB_PG_PORT -k $SUB_PG_SOCK -c listen_addresses=127.0.0.1 -c wal_level=logical -c max_replication_slots=4 -c max_wal_senders=4" \
    -l "$WORK/subscription-pg.log" start >/dev/null; then
    bad "logical subscription publisher start"
    tail -20 "$WORK/subscription-pg.log"
    rm -rf "$SUB_PG_SOCK"
  else
  SUB_PSQL="$SUB_PGBIN/psql"
  if ! "$SUB_PSQL" -h 127.0.0.1 -p $SUB_PG_PORT -U postgres -X -q \
    -c "CREATE TABLE subscription_target (id int PRIMARY KEY, body text NOT NULL)" \
    -c "CREATE PUBLICATION subscription_apply_pub FOR TABLE subscription_target" \
    -c "CREATE PUBLICATION subscription_apply_pub_after_alter FOR TABLE subscription_target" \
    -c "SELECT pg_create_logical_replication_slot('subscription_apply_slot', 'pgoutput')" \
    >/dev/null 2>&1; then
    bad "logical subscription publisher setup"
  elif ! "$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
    -c "CREATE TABLE subscription_target (id int PRIMARY KEY, body text NOT NULL)" \
    -c "CREATE SUBSCRIPTION subscription_apply CONNECTION 'host=127.0.0.1 port=$SUB_PG_PORT user=postgres dbname=postgres application_name=pos3ql_subscription_apply sslmode=disable' PUBLICATION subscription_apply_pub WITH (create_slot = false, copy_data = false, slot_name = subscription_apply_slot)" \
    >/dev/null 2>&1; then
    bad "logical subscription subscriber setup"
  elif ! "$SUB_PSQL" -h 127.0.0.1 -p $SUB_PG_PORT -U postgres -X -q \
    -c "BEGIN; INSERT INTO subscription_target VALUES (1, 'first'), (2, 'second'); COMMIT" \
    >/dev/null 2>&1; then
    bad "logical subscription publisher transaction"
  else
  subscription_rows() {
    "$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -t -A -F'|' \
      -c "SELECT id, body FROM subscription_target ORDER BY id" 2>&1
  }
  subscription_wait() { # <expected rows>
    local expected=$1 actual=""
    for _ in {1..100}; do
      actual=$(subscription_rows)
      [[ "$actual" == "$expected" ]] && return 0
      sleep 0.1
    done
    printf '%s\n' "$actual"
    return 1
  }
  subscription_diagnostics() {
    printf '%s\n' '--- pos3ql subscription server log ---'
    tail -100 "$WORK/server.log"
    printf '%s\n' '--- PostgreSQL publisher log ---'
    tail -100 "$WORK/subscription-pg.log"
  }
  if subscription_wait $'1|first\n2|second'; then
    ok "subscription applies one PostgreSQL transaction atomically"
  else
    bad "subscription initial apply (got $(subscription_rows))"
    subscription_diagnostics
  fi
  if ! "$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q \
    -c "ALTER SUBSCRIPTION subscription_apply SET PUBLICATION subscription_apply_pub_after_alter WITH (refresh = false)" \
    -c "ALTER SUBSCRIPTION subscription_apply CONNECTION 'host=127.0.0.1 port=$SUB_PG_PORT user=postgres dbname=postgres application_name=pos3ql_subscription_rebound sslmode=disable'" \
    >/dev/null 2>&1; then
    bad "logical subscription definition alteration"
  elif ! "$SUB_PSQL" -h 127.0.0.1 -p $SUB_PG_PORT -U postgres -X -q \
    -c "INSERT INTO subscription_target VALUES (3, 'after-alter')" >/dev/null 2>&1; then
    bad "logical subscription post-alter publisher transaction"
  elif subscription_wait $'1|first\n2|second\n3|after-alter'; then
    ok "subscription reconnects from its committed altered definition"
  else
    bad "subscription post-alter apply (got $(subscription_rows))"
    subscription_diagnostics
  fi
  kill -9 $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
  start_pos3ql "$WORK/server.conf" "$WORK/server.log" $PG_PORT
  SERVER_PID=$START_PID
  if subscription_wait $'1|first\n2|second\n3|after-alter'; then
    if ! "$SUB_PSQL" -h 127.0.0.1 -p $SUB_PG_PORT -U postgres -X -q \
      -c "INSERT INTO subscription_target VALUES (4, 'after-crash')" >/dev/null 2>&1; then
    bad "logical subscription post-crash publisher transaction"
    elif subscription_wait $'1|first\n2|second\n3|after-alter\n4|after-crash'; then
      slot_lsn=$("$SUB_PSQL" -h 127.0.0.1 -p $SUB_PG_PORT -U postgres -X -t -A \
        -c "SELECT confirmed_flush_lsn <> '0/0'::pg_lsn FROM pg_replication_slots WHERE slot_name = 'subscription_apply_slot'")
      [[ "$slot_lsn" == "t" ]] && ok "subscription crash recovery resumes from durable acknowledgement" \
        || bad "subscription publisher acknowledgement was not durable (got $slot_lsn)"
    else
      bad "subscription post-crash apply (got $(subscription_rows))"
    fi
  else
    bad "subscription local WAL recovery (got $(subscription_rows))"
    subscription_diagnostics
  fi
  fi
  "$SUB_PGBIN/pg_ctl" -D "$WORK/subscription-pgdata" stop -m immediate >/dev/null
  rm -rf "$SUB_PG_SOCK"
  fi
  fi
else
  bad "logical subscription apply needs POS3QL_PGBIN with a PostgreSQL publisher"
fi

fi # subscription

if want tls; then

step "object-store TLS is covered by the gateway unit suite"
if cargo test --lib object_store::http::tests::tls_round_trip >/dev/null; then
  ok "generic gateway TLS round trip"
else
  bad "generic gateway TLS round trip"
fi
# --- Server-side TLS: the client connects over TLS (no object store needed) --
step "server-side TLS: psql connects with sslmode=require"
STLS_PORT=$((PG_PORT + 2))
mkdir -p "$WORK/server-tls"
openssl req -x509 -newkey rsa:2048 -keyout "$WORK/server-tls/key.pem" \
  -out "$WORK/server-tls/cert.pem" -days 30 -nodes -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null
cat > "$WORK/server-tls.conf" <<EOF
listen_addr = 127.0.0.1:${STLS_PORT}
data_dir = ${WORK}/server-tls-data
object_store = off
sql_arena_bytes = 32MiB
work_arena_bytes = 64MiB
tls_on = on
tls_cert_file = ${WORK}/server-tls/cert.pem
tls_key_file = ${WORK}/server-tls/key.pem
EOF
start_pos3ql "$WORK/server-tls.conf" "$WORK/server-tls.log" ${STLS_PORT}
STLS_PID=$START_PID
# A query under sslmode=require only completes if the server negotiated TLS
# (psql aborts if the SSLRequest is declined), so this both runs SQL over the
# encrypted link and proves it is encrypted.
enc=$("$PSQL" "host=127.0.0.1 port=${STLS_PORT} user=postgres sslmode=require" -X -t -A -c "SELECT 'ok'" 2>&1)
# A plaintext client must still connect (the SSLRequest is declined with 'N').
plain=$("$PSQL" "host=127.0.0.1 port=${STLS_PORT} user=postgres sslmode=disable" -X -t -A -c "SELECT 'plain'" 2>&1)
# A large result (>64KiB, the send buffer) exercises the streaming drain through
# the session: its bytes must match the same query over plaintext exactly.
"$PSQL" "host=127.0.0.1 port=${STLS_PORT} user=postgres sslmode=require" -X -q \
  -c "CREATE TABLE stls (n int, s text)" \
  -c "INSERT INTO stls SELECT g, repeat('x',100) FROM generate_series(1,5000) g" >/dev/null 2>&1
big_tls=$("$PSQL" "host=127.0.0.1 port=${STLS_PORT} user=postgres sslmode=require" -X -t -A -c "SELECT n, s FROM stls ORDER BY n" 2>&1 | md5sum | cut -d' ' -f1)
big_plain=$("$PSQL" "host=127.0.0.1 port=${STLS_PORT} user=postgres sslmode=disable" -X -t -A -c "SELECT n, s FROM stls ORDER BY n" 2>&1 | md5sum | cut -d' ' -f1)
kill -9 $STLS_PID 2>/dev/null; wait $STLS_PID 2>/dev/null
if [[ "$enc" == "ok" && "$plain" == "plain" && "$big_tls" == "$big_plain" && -n "$big_tls" ]]; then
  ok "server-side TLS (sslmode=require works, plaintext coexists, streaming byte-exact)"
else
  bad "server-side TLS (enc=$enc plain=$plain tls_md5=$big_tls plain_md5=$big_plain)"
  tail -10 "$WORK/server-tls.log"
fi

fi # tls

if want diff; then

step "differential vs real PostgreSQL 18 (when installed)"
if [[ -n "${POS3QL_REFERENCE_PG_HOST:-}" || -x "${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}/postgres" ]]; then
  if tests/external/differential.sh > "$WORK/differential.out" 2>&1; then
    ok "differential suite ($(grep -c '^PASS' "$WORK/differential.out") corpora)"
  else
    bad "differential suite"
    grep -A 32 -B 2 '^FAIL:' "$WORK/differential.out" | head -80 || true
    tail -30 "$WORK/differential.out"
  fi
else
  printf '%s\n' 'SKIP: real PostgreSQL 18 not installed'
fi

fi # diff

if want spilldiff; then

step "forced-spill differential: the whole suite with a 256KiB memtable over the bucket"
# Every corpus and sqllogictest block runs against a pos3ql whose memtable is
# three orders of magnitude under the dataset churn, so ordinary queries
# continuously spill, checkpoint (paced merges included), and read rows back
# through the cache tiers — while the reference PostgreSQL sees plain SQL.
# Pure-SQL semantics must be indistinguishable from the in-memory run.
if [[ -n "${POS3QL_REFERENCE_PG_HOST:-}" || -x "${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}/postgres" ]]; then
  if POS3QL_DIFF_OBJECT_STORE=on POS3QL_DIFF_MEMTABLE=256KiB POS3QL_DIFF_OBJECT_STORE_PREFIX="spilldiff-$$/" POS3QL_EXTRA_CONF="object_store_endpoint = 127.0.0.1:${GATEWAY_PORT}
object_store_namespace = pos3ql-external
wal_upload = on
wal_upload_sync = on
work_arena_bytes = 192MiB" tests/external/differential.sh > "$WORK/spilldiff.out" 2>&1; then
    ok "forced-spill differential ($(grep -c '^PASS' "$WORK/spilldiff.out") corpora)"
  else
    bad "forced-spill differential"
    grep -A 32 -B 2 '^FAIL:' "$WORK/spilldiff.out" | head -80 || true
    tail -30 "$WORK/spilldiff.out"
  fi
else
  printf '%s\n' 'SKIP: forced-spill differential needs real PostgreSQL 18'
fi

fi # spilldiff

step "summary"
printf 'groups: %s\n' "$SELECTED_GROUPS"
printf 'passed: %s  failed: %s\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
