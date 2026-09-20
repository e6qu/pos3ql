#!/usr/bin/env bash
# Reproducible cache, maintenance, group-commit, and PostgreSQL 18 comparison suite.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
. "$ROOT/tests/external/liveness.sh"
TEST_PORT_HELPER="$ROOT/tests/external/test_ports.py"

MODE=${1:-full}
case "$MODE" in
  smoke|full|checkpoint) ;;
  *) echo "usage: $0 [smoke|full|checkpoint] [output-directory]" >&2; exit 2 ;;
esac
OUTPUT=${2:-"$ROOT/performance-results/$(date -u +%Y%m%dT%H%M%SZ)"}
mkdir -p "$OUTPUT"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/pos3ql-performance.XXXXXX")
SERVER_PID=
S3_PID=
POSTGRES_CONTAINER=
REPLICA_PIDS=

cleanup() {
  if [ -n "$SERVER_PID" ] && server_alive "$SERVER_PID"; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  if [ -n "$S3_PID" ] && server_alive "$S3_PID"; then
    kill "$S3_PID" 2>/dev/null || true
    wait "$S3_PID" 2>/dev/null || true
  fi
  for replica_pid in $REPLICA_PIDS; do
    if server_alive "$replica_pid"; then
      kill "$replica_pid" 2>/dev/null || true
      wait "$replica_pid" 2>/dev/null || true
    fi
  done
  if [ -n "$POSTGRES_CONTAINER" ]; then
    docker rm -f "$POSTGRES_CONTAINER" >/dev/null 2>&1 || true
  fi
  release_test_ports
  if [ -n "$WORK" ] && [ -d "$WORK" ]; then
    rm -rf -- "$WORK"
  fi
}
trap cleanup EXIT INT TERM

S3_PORT=$(claim_test_port "${POS3QL_BENCH_S3_PORT:-}" 19500 19599)
POS3QL_PORT=$(claim_test_port "${POS3QL_BENCH_PORT:-}" 19600 19699)
METRICS="$WORK/object-store-metrics.json"
LATENCY_MS=${POS3QL_BENCH_OBJECT_LATENCY_MS:-2}
DISK_CACHE_MIB=${POS3QL_BENCH_DISK_CACHE_MIB:-128}
BENCH_TIMEOUT_SECONDS=${POS3QL_BENCH_TIMEOUT_SECONDS:-30}
if ! [[ "$DISK_CACHE_MIB" =~ ^(0|[1-9][0-9]*)$ && "$BENCH_TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]]; then
  echo "POS3QL_BENCH_DISK_CACHE_MIB must be nonnegative and POS3QL_BENCH_TIMEOUT_SECONDS positive decimal integers" >&2
  exit 2
fi
export POS3QL_BENCH_TIMEOUT_SECONDS=$BENCH_TIMEOUT_SECONDS
python3 "$ROOT/tests/external/s3_test_server.py" \
  --root "$WORK/objects" --port "$S3_PORT" --bucket performance \
  --region benchmark --access-key benchmark --secret-key benchmark-secret \
  --metrics-file "$METRICS" --latency-ms "$LATENCY_MS" &
S3_PID=$!

for attempt in $(seq 1 100); do
  if ! server_alive "$S3_PID"; then echo "object-store fixture exited at startup" >&2; exit 1; fi
  if nc -z 127.0.0.1 "$S3_PORT" >/dev/null 2>&1; then break; fi
  if [ "$attempt" = 100 ]; then echo "object-store fixture did not start" >&2; exit 1; fi
  sleep 0.05
done
for attempt in $(seq 1 100); do
  if [ -s "$METRICS" ]; then break; fi
  if [ "$attempt" = 100 ]; then echo "object-store metrics did not start" >&2; exit 1; fi
  sleep 0.05
done

cargo build --release --locked --manifest-path "$ROOT/Cargo.toml"

write_config() {
  config=$1
  data=$2
  prefix=$3
  port=$4
  cat >"$config" <<EOF
listen_addr = 127.0.0.1:$port
data_dir = $data
auth = trust
max_connections = $MAX_CONNECTIONS
conn_recv_buffer_bytes = 64 KiB
conn_send_buffer_bytes = 64 KiB
sql_arena_bytes = 2 MiB
work_arena_bytes = 32 MiB
max_prepared = 8
max_portals = 4
txn_rows = $TABLE_CAPACITY
memtable_bytes = 16 MiB
wal_bytes = 32 MiB
wal_buffer_bytes = 2 MiB
max_tables = 16
max_indexes = 24
table_rows = $TABLE_CAPACITY
value_index_rows = $TABLE_CAPACITY
max_value_indexes = 24
max_large_objects = 16
large_object_pages = 64
max_large_object_descriptors = 8
max_rules = 32
max_extension_scripts = 16
extension_script_bytes = 256 KiB
max_foreign_data_wrappers = 4
max_foreign_servers = 4
max_user_mappings = 8
block_cache_bytes = 32 MiB
disk_cache_bytes = $DISK_CACHE_MIB MiB
temporary_spill_bytes = 128 MiB
max_replication_slots = 8
max_subscriptions = 4
subscription_relation_capacity = 32
subscription_arena_bytes = 512 KiB
object_store = on
object_store_endpoint = 127.0.0.1:$S3_PORT
object_store_bucket = performance
object_store_prefix = $prefix
object_store_region = benchmark
object_store_access_key = benchmark
object_store_secret_key = benchmark-secret
object_store_response_bytes = 512 KiB
object_store_get_slots = 4
wal_upload = on
wal_upload_sync = on
EOF
}

start_pos3ql() {
  data=$1
  prefix=$2
  recovery_label=$3
  require_read=${4:-no}
  mkdir -p "$data"
  config="$WORK/pos3ql-$prefix.conf"
  log="$WORK/pos3ql-$prefix.log"
  write_config "$config" "$data" "$prefix" "$POS3QL_PORT"
  sleep 0.1
  cp "$METRICS" "$WORK/$recovery_label-before.json"
  startup_started=$(python3 -c 'import time; print(time.monotonic_ns())')
  "$ROOT/target/release/pos3ql" --config "$config" >"$log" 2>&1 &
  SERVER_PID=$!
  for attempt in $(seq 1 200); do
    if nc -z 127.0.0.1 "$POS3QL_PORT" >/dev/null 2>&1; then
      MEMORY_PLAN_BYTES=$(awk '
        $1 == "total" {
          multiplier = ($3 == "GiB" ? 1073741824 : ($3 == "MiB" ? 1048576 : ($3 == "KiB" ? 1024 : 1)));
          print $2 * multiplier;
        }' "$log")
      test -n "$MEMORY_PLAN_BYTES"
      startup_finished=$(python3 -c 'import time; print(time.monotonic_ns())')
      startup_seconds=$(python3 -c 'import sys; print((int(sys.argv[2]) - int(sys.argv[1])) / 1_000_000_000)' "$startup_started" "$startup_finished")
      sleep 0.1
      cp "$METRICS" "$WORK/$recovery_label-after.json"
      read_gate=
      if [ "$require_read" = yes ]; then read_gate=--require-read; fi
      python3 "$ROOT/tools/object-metrics-diff.py" \
        --before "$WORK/$recovery_label-before.json" \
        --after "$WORK/$recovery_label-after.json" --label "$recovery_label" \
        --elapsed-seconds "$startup_seconds" --output "$OUTPUT/$recovery_label.json" \
        $read_gate
      return
    fi
    if ! server_alive "$SERVER_PID"; then cat "$log" >&2; exit 1; fi
    sleep 0.05
  done
  cat "$log" >&2
  echo "pos3ql did not start" >&2
  exit 1
}

launch_replica() {
  replica_name=$1
  replica_port=$2
  replica_data="$WORK/data-$replica_name"
  replica_config="$WORK/$replica_name.conf"
  replica_log="$WORK/$replica_name.log"
  mkdir -p "$replica_data"
  write_config "$replica_config" "$replica_data" "$replica_name" "$replica_port"
  "$ROOT/target/release/pos3ql" --config "$replica_config" >"$replica_log" 2>&1 &
  LAUNCHED_PID=$!
  REPLICA_PIDS="$REPLICA_PIDS $LAUNCHED_PID"
  for attempt in $(seq 1 200); do
    if nc -z 127.0.0.1 "$replica_port" >/dev/null 2>&1; then return; fi
    if ! server_alive "$LAUNCHED_PID"; then cat "$replica_log" >&2; exit 1; fi
    sleep 0.05
  done
  cat "$replica_log" >&2
  echo "$replica_name did not start" >&2
  exit 1
}

stop_pos3ql() {
  kill "$SERVER_PID"
  wait "$SERVER_PID"
  SERVER_PID=
}

bench_pos3ql() {
  label=$1
  shift
  python3 "$ROOT/tools/benchmark.py" \
    --port "$POS3QL_PORT" --label "$label" --pid "$SERVER_PID" \
    --fixed-memory-bytes "$MEMORY_PLAN_BYTES" --object-metrics "$METRICS" \
    --output "$OUTPUT/$label.json" --check "$@" >/dev/null
}

if [ "$MODE" = smoke ]; then
  ROWS=128
  TABLE_CAPACITY=512
  OPERATIONS=8
  CLIENTS=4
  REPLICA_SETTING=0
else
  ROWS=${POS3QL_BENCH_ROWS:-10000}
  TABLE_CAPACITY=${POS3QL_BENCH_TABLE_CAPACITY:-16384}
  OPERATIONS=${POS3QL_BENCH_OPERATIONS:-500}
  CLIENTS=${POS3QL_BENCH_CLIENTS:-8}
  REPLICA_SETTING=${POS3QL_BENCH_REPLICAS:-2}
fi
if [ "$MODE" = checkpoint ]; then
  REPLICA_SETTING=${POS3QL_BENCH_REPLICAS:-0}
  if [ "$REPLICA_SETTING" != 0 ]; then
    echo "checkpoint mode requires POS3QL_BENCH_REPLICAS=0" >&2
    exit 2
  fi
fi
if ! [[ "$REPLICA_SETTING" =~ ^(0|[1-9][0-9]*)$ ]]; then
  echo "POS3QL_BENCH_REPLICAS must be a nonnegative decimal count" >&2
  exit 2
fi
if [ "$MODE" != smoke ] && [ "$TABLE_CAPACITY" -lt $((ROWS + CLIENTS * OPERATIONS)) ]; then
  echo "POS3QL_BENCH_TABLE_CAPACITY must cover setup and inserted rows" >&2
  exit 2
fi
MAX_CONNECTIONS=$((CLIENTS + 4))
python3 "$ROOT/tools/benchmark-environment.py" \
  --output "$OUTPUT/environment.json" --binary "$ROOT/target/release/pos3ql" \
  --mode "$MODE" --rows "$ROWS" --table-capacity "$TABLE_CAPACITY" \
  --operations "$OPERATIONS" --clients "$CLIENTS" \
  --replicas "$REPLICA_SETTING" --object-latency-ms "$LATENCY_MS" \
  --disk-cache-mib "$DISK_CACHE_MIB" --timeout-seconds "$BENCH_TIMEOUT_SECONDS"

DATA_WARM="$WORK/data-warm"
start_pos3ql "$DATA_WARM" primary initial-start
bench_pos3ql point-concurrency-1 --workload point-read --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --setup --require-index
if [ "$MODE" = checkpoint ]; then
  bench_pos3ql mixed-checkpoint-interference --workload mixed --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" \
    --maintenance-interval 0.05 --maintenance-limit 3
  stop_pos3ql
  cp "$WORK/pos3ql-primary.log" "$OUTPUT/pos3ql-startup.log"
  python3 "$ROOT/tools/benchmark-report.py" "$OUTPUT" >"$OUTPUT/report.md"
  echo "performance results: $OUTPUT"
  exit 0
fi
bench_pos3ql warm-memory-point --workload point-read --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-brin-point --workload brin-point --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-brin-inclusion --workload brin-inclusion --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gist-inclusion --workload gist-inclusion --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gist-multirange --workload gist-multirange --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gist-network --workload gist-network --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gist-spatial --workload gist-spatial --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gist-knn --workload gist-knn --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gin-array --workload gin-array --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gin-array-overlap --workload gin-array-overlap --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gin-tsvector --workload gin-tsvector --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gist-tsvector --workload gist-tsvector --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gin-jsonb --workload gin-jsonb --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-gin-jsonb-path --workload gin-jsonb-path --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-spgist-prefix --workload spgist-prefix --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-spgist-range --workload spgist-range --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-spgist-network --workload spgist-network --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-spgist-spatial --workload spgist-spatial --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-spgist-knn --workload spgist-knn --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-tail-range --workload tail-range --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-ordered-limit --workload ordered-limit --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql warm-memory-join-probe --workload join-probe --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql concurrent-update --workload update --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS" --synchronized --require-index

if [ "$MODE" = full ]; then
  bench_pos3ql concurrent-insert --workload insert --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS"
  bench_pos3ql analytical-scan --workload scan --clients 1 \
    --operations "$((OPERATIONS / 20 + 1))" --rows "$ROWS"
  bench_pos3ql mixed-baseline --workload mixed --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS"
  bench_pos3ql mixed-checkpoint-interference --workload mixed --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" \
    --maintenance-interval 0.05 --maintenance-limit 3
fi

stop_pos3ql
start_pos3ql "$DATA_WARM" primary warm-disk-recovery
bench_pos3ql warm-disk-point --workload point-read --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS"
stop_pos3ql

DATA_COLD="$WORK/data-cold"
start_pos3ql "$DATA_COLD" primary cold-object-recovery yes
bench_pos3ql cold-object-tail-range --workload tail-range --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-ordered-limit --workload ordered-limit --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-point --workload point-read --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS"
bench_pos3ql cold-object-brin-point --workload brin-point --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-brin-inclusion --workload brin-inclusion --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gist-inclusion --workload gist-inclusion --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gist-multirange --workload gist-multirange --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gist-network --workload gist-network --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gist-spatial --workload gist-spatial --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gist-knn --workload gist-knn --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gin-array --workload gin-array --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gin-array-overlap --workload gin-array-overlap --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gin-tsvector --workload gin-tsvector --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gist-tsvector --workload gist-tsvector --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gin-jsonb --workload gin-jsonb --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-gin-jsonb-path --workload gin-jsonb-path --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-spgist-prefix --workload spgist-prefix --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-spgist-range --workload spgist-range --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-spgist-network --workload spgist-network --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-spgist-spatial --workload spgist-spatial --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
bench_pos3ql cold-object-spgist-knn --workload spgist-knn --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index
stop_pos3ql

DATA_COLD_JOIN="$WORK/data-cold-join"
start_pos3ql "$DATA_COLD_JOIN" primary cold-object-join-recovery yes
bench_pos3ql cold-object-join-probe --workload join-probe --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --require-index

if [ "$MODE" = full ]; then
  SCALE_ROWS=$((ROWS + CLIENTS * OPERATIONS))
  python3 "$ROOT/tools/pg-query.py" --port "$POS3QL_PORT" \
    "CREATE PUBLICATION benchmark_scale_publication FOR TABLE benchmark_kv" >/dev/null
  REPLICA_TARGETS=
  REPLICA_COUNT=$REPLICA_SETTING
  for ((replica = 1; replica <= REPLICA_COUNT; replica++)); do
    replica_port=$(claim_test_port "" $((19800 + replica * 10)) $((19809 + replica * 10)))
    launch_replica "replica-$replica" "$replica_port"
    python3 "$ROOT/tools/pg-query.py" --port "$replica_port" \
      "CREATE TABLE benchmark_kv(id integer PRIMARY KEY, hash_key integer NOT NULL, brin_key integer NOT NULL, brin_span int4range NOT NULL, gist_span int4range NOT NULL, gist_spans int4multirange NOT NULL, gist_address inet NOT NULL, gist_location point NOT NULL, gin_tags integer[] NOT NULL, gin_document tsvector NOT NULL, gist_document tsvector NOT NULL, json_ops jsonb NOT NULL, json_path jsonb NOT NULL, spgist_label text NOT NULL, spgist_span int4range NOT NULL, spgist_address inet NOT NULL, spgist_location point NOT NULL, payload bigint NOT NULL, padding text NOT NULL DEFAULT repeat('x', 8192)); CREATE INDEX benchmark_hash_lookup ON benchmark_kv USING hash (hash_key); CREATE INDEX benchmark_brin_lookup ON benchmark_kv USING brin (brin_key) WITH (pages_per_range=32, autosummarize=on); CREATE INDEX benchmark_brin_inclusion ON benchmark_kv USING brin (brin_span range_inclusion_ops) WITH (pages_per_range=32); CREATE INDEX benchmark_gist_inclusion ON benchmark_kv USING gist (gist_span); CREATE INDEX benchmark_gist_multirange ON benchmark_kv USING gist (gist_spans); CREATE INDEX benchmark_gist_network ON benchmark_kv USING gist (gist_address inet_ops); CREATE INDEX benchmark_gist_knn ON benchmark_kv USING gist (gist_location) INCLUDE (id, payload); CREATE INDEX benchmark_gin_array ON benchmark_kv USING gin (gin_tags); CREATE INDEX benchmark_gin_document ON benchmark_kv USING gin (gin_document); CREATE INDEX benchmark_gist_document ON benchmark_kv USING gist (gist_document); CREATE INDEX benchmark_gin_json_ops ON benchmark_kv USING gin (json_ops); CREATE INDEX benchmark_gin_json_path ON benchmark_kv USING gin (json_path jsonb_path_ops); CREATE INDEX benchmark_spgist_prefix ON benchmark_kv USING spgist (spgist_label); CREATE INDEX benchmark_spgist_range ON benchmark_kv USING spgist (spgist_span); CREATE INDEX benchmark_spgist_network ON benchmark_kv USING spgist (spgist_address inet_ops); CREATE INDEX benchmark_spgist_knn ON benchmark_kv USING spgist (spgist_location kd_point_ops) INCLUDE (id, payload); CREATE SUBSCRIPTION benchmark_scale_subscription_$replica CONNECTION 'host=127.0.0.1 port=$POS3QL_PORT user=postgres dbname=postgres application_name=performance_replica_$replica sslmode=disable' PUBLICATION benchmark_scale_publication" >/dev/null
    python3 "$ROOT/tools/pg-query.py" --port "$replica_port" --expect "$SCALE_ROWS" \
      --timeout 30 "SELECT count(*) FROM benchmark_kv" >/dev/null
    REPLICA_TARGETS="$REPLICA_TARGETS --target 127.0.0.1:$replica_port"
    python3 "$ROOT/tools/pg-query.py" --port "$POS3QL_PORT" \
      "UPDATE benchmark_kv SET payload = $replica WHERE id = 1" >/dev/null
    python3 "$ROOT/tools/pg-query.py" --port "$replica_port" --expect "$replica" \
      --timeout 30 --output-json "$OUTPUT/logical-replica-$replica-freshness.json" \
      "SELECT payload FROM benchmark_kv WHERE id = 1" >/dev/null
    # shellcheck disable=SC2086 # repeatable --target arguments are intentional
    python3 "$ROOT/tools/benchmark.py" $REPLICA_TARGETS \
      --label "logical-replicas-$replica" --workload point-read \
      --clients "$CLIENTS" --operations "$OPERATIONS" --rows "$ROWS" --check \
      --output "$OUTPUT/logical-replicas-$replica.json" >/dev/null
  done

  POSTGRES_PORT=${POS3QL_BENCH_POSTGRES_PORT:-}
  if [ -z "${POS3QL_BENCH_POSTGRES_PORT:-}" ]; then
    POSTGRES_PORT=$(claim_test_port "" 19700 19799)
    POSTGRES_IMAGE=${POS3QL_BENCH_POSTGRES_IMAGE:-postgres:18}
    POSTGRES_CONTAINER="pos3ql-performance-$$"
    docker run -d --name "$POSTGRES_CONTAINER" -p "$POSTGRES_PORT:5432" \
      -e POSTGRES_HOST_AUTH_METHOD=trust "$POSTGRES_IMAGE" >/dev/null
    for attempt in $(seq 1 200); do
      if docker exec "$POSTGRES_CONTAINER" pg_isready -U postgres >/dev/null 2>&1; then break; fi
      if [ "$attempt" = 200 ]; then docker logs "$POSTGRES_CONTAINER" >&2; exit 1; fi
      sleep 0.1
    done
    python3 "$ROOT/tools/pg-query.py" --port "$POSTGRES_PORT" --expect 1 \
      --timeout 30 "SELECT 1" >/dev/null
    docker inspect --format '{{.Image}}' "$POSTGRES_CONTAINER" >"$OUTPUT/postgresql-image-id.txt"
    POSTGRES_STORAGE=${POS3QL_BENCH_POSTGRES_STORAGE:-Docker-managed local volume; host backing unspecified}
  else
    POSTGRES_STORAGE=${POS3QL_BENCH_POSTGRES_STORAGE:?POS3QL_BENCH_POSTGRES_STORAGE must describe the external PostgreSQL storage}
  fi
  if [ -n "$POSTGRES_CONTAINER" ]; then
    python3 "$ROOT/tools/benchmark-postgresql.py" --port "$POSTGRES_PORT" \
      --storage-description "$POSTGRES_STORAGE" \
      --output "$OUTPUT/postgresql-server.json" \
      --docker-container "$POSTGRES_CONTAINER"
  else
    python3 "$ROOT/tools/benchmark-postgresql.py" --port "$POSTGRES_PORT" \
      --storage-description "$POSTGRES_STORAGE" \
      --output "$OUTPUT/postgresql-server.json"
  fi
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label postgresql18-point-concurrency-1 --workload point-read --clients 1 \
    --operations "$OPERATIONS" --rows "$ROWS" --setup --check \
    --output "$OUTPUT/postgresql18-point-concurrency-1.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label postgresql18-point --workload point-read --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/postgresql18-point.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label postgresql18-tail-range --workload tail-range --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/postgresql18-tail-range.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label postgresql18-ordered-limit --workload ordered-limit --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/postgresql18-ordered-limit.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label postgresql18-join-probe --workload join-probe --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/postgresql18-join-probe.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label postgresql18-insert --workload insert --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/postgresql18-insert.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label postgresql18-scan --workload scan --clients 1 \
    --operations "$((OPERATIONS / 20 + 1))" --rows "$ROWS" --check \
    --output "$OUTPUT/postgresql18-scan.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label postgresql18-mixed --workload mixed --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/postgresql18-mixed.json" >/dev/null
fi

cp "$WORK/pos3ql-primary.log" "$OUTPUT/pos3ql-startup.log"
python3 "$ROOT/tools/benchmark-report.py" "$OUTPUT" >"$OUTPUT/report.md"
echo "performance results: $OUTPUT"
