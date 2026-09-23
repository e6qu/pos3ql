#!/usr/bin/env bash
# Reproducible cache, maintenance, group-commit, and PostgreSQL 18 comparison suite.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=tests/external/liveness.sh
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
OBJECT_STORE_CONTAINER=
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
  if [ -n "$OBJECT_STORE_CONTAINER" ]; then
    docker rm -f "$OBJECT_STORE_CONTAINER" >/dev/null 2>&1 || true
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

POS3QL_PORT=
OBJECT_STORE=${POS3QL_BENCH_OBJECT_STORE:-fixture}
case "$OBJECT_STORE" in
  fixture|minio|seaweedfs|external) ;;
  *) echo "POS3QL_BENCH_OBJECT_STORE must be fixture, minio, seaweedfs, or external" >&2; exit 2 ;;
esac
if [ "$OBJECT_STORE" = fixture ]; then
  LATENCY_MS=${POS3QL_BENCH_OBJECT_LATENCY_MS:-2}
  OBJECT_LATENCY_INJECTED=1
else
  LATENCY_MS=${POS3QL_BENCH_OBJECT_LATENCY_MS:-0}
  OBJECT_LATENCY_INJECTED=0
  if [ "$LATENCY_MS" != 0 ]; then
    echo "POS3QL_BENCH_OBJECT_LATENCY_MS must be 0 for MinIO, SeaweedFS, and external services" >&2
    exit 2
  fi
fi
METRICS=
S3_PORT=
OBJECT_STORE_ENDPOINT=
OBJECT_STORE_BUCKET=performance
OBJECT_STORE_REGION=benchmark
OBJECT_STORE_ACCESS_KEY=benchmark
OBJECT_STORE_SECRET_KEY=benchmark-secret
OBJECT_STORE_SESSION_TOKEN=
OBJECT_STORE_ADDRESSING=path
OBJECT_STORE_TLS=off
OBJECT_STORE_TLS_CA_FILE=
OBJECT_STORE_PREFIX_ROOT=
OBJECT_STORE_IMPLEMENTATION=
OBJECT_STORE_BACKING=
OBJECT_STORE_IMAGE=
OBJECT_STORE_IMAGE_ID=
OBJECT_STORE_INDEPENDENT_IMPLEMENTATION=0
OBJECT_STORE_INDEPENDENTLY_OPERATED=0
OBJECT_STORE_REQUEST_METRICS=0
BENCHMARK_HARDWARE_DESCRIPTION="automatic host inventory only"
BENCHMARK_NETWORK_DESCRIPTION="object store on the benchmark host"
CACHE_STORAGE_DESCRIPTION="temporary local directory; host backing unspecified"
DISK_CACHE_MIB=${POS3QL_BENCH_DISK_CACHE_MIB:-128}
BENCH_TIMEOUT_SECONDS=${POS3QL_BENCH_TIMEOUT_SECONDS:-30}
CHECKPOINT_PROFILE=${POS3QL_BENCH_CHECKPOINT_PROFILE:-0}
CHECKPOINT_DURATION=${POS3QL_BENCH_CHECKPOINT_SECONDS:-4}
MATCHED_POSTGRES_CPUS=$(python3 -c 'import os; print(len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count())')
if ! [[ "$MATCHED_POSTGRES_CPUS" =~ ^[1-9][0-9]*$ ]]; then
  echo "could not determine the CPUs available to pos3ql" >&2
  exit 1
fi
if ! [[ "$DISK_CACHE_MIB" =~ ^(0|[1-9][0-9]*)$ && "$BENCH_TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]]; then
  echo "POS3QL_BENCH_DISK_CACHE_MIB must be nonnegative and POS3QL_BENCH_TIMEOUT_SECONDS positive decimal integers" >&2
  exit 2
fi
if [[ "$CHECKPOINT_PROFILE" != 0 && "$CHECKPOINT_PROFILE" != 1 ]]; then
  echo "POS3QL_BENCH_CHECKPOINT_PROFILE must be 0 or 1" >&2
  exit 2
fi
if [[ "$CHECKPOINT_PROFILE" = 1 && "$MODE" != checkpoint ]]; then
  echo "checkpoint profiling requires checkpoint mode" >&2
  exit 2
fi
export POS3QL_BENCH_TIMEOUT_SECONDS=$BENCH_TIMEOUT_SECONDS

if [ "$OBJECT_STORE" = external ]; then
  OBJECT_STORE_ENDPOINT=${POS3QL_BENCH_OBJECT_STORE_ENDPOINT:?POS3QL_BENCH_OBJECT_STORE_ENDPOINT must be host:port for an external object store}
  OBJECT_STORE_BUCKET=${POS3QL_BENCH_OBJECT_STORE_BUCKET:?POS3QL_BENCH_OBJECT_STORE_BUCKET is required for an external object store}
  OBJECT_STORE_REGION=${POS3QL_BENCH_OBJECT_STORE_REGION:?POS3QL_BENCH_OBJECT_STORE_REGION is required for an external object store}
  OBJECT_STORE_ACCESS_KEY=${POS3QL_BENCH_OBJECT_STORE_ACCESS_KEY:?POS3QL_BENCH_OBJECT_STORE_ACCESS_KEY is required for an external object store}
  OBJECT_STORE_SECRET_KEY=${POS3QL_BENCH_OBJECT_STORE_SECRET_KEY:?POS3QL_BENCH_OBJECT_STORE_SECRET_KEY is required for an external object store}
  OBJECT_STORE_SESSION_TOKEN=${POS3QL_BENCH_OBJECT_STORE_SESSION_TOKEN:-}
  OBJECT_STORE_ADDRESSING=${POS3QL_BENCH_OBJECT_STORE_ADDRESSING:-path}
  OBJECT_STORE_TLS=${POS3QL_BENCH_OBJECT_STORE_TLS:-on}
  OBJECT_STORE_TLS_CA_FILE=${POS3QL_BENCH_OBJECT_STORE_TLS_CA_FILE:-}
  OBJECT_STORE_PREFIX_ROOT=${POS3QL_BENCH_OBJECT_STORE_PREFIX:-performance-$(date -u +%Y%m%dT%H%M%SZ)-$$}
  OBJECT_STORE_IMPLEMENTATION=${POS3QL_BENCH_OBJECT_STORE_IMPLEMENTATION:?POS3QL_BENCH_OBJECT_STORE_IMPLEMENTATION must identify the external service and available version}
  OBJECT_STORE_BACKING=${POS3QL_BENCH_OBJECT_STORE_BACKING:?POS3QL_BENCH_OBJECT_STORE_BACKING must describe the external durable tier}
  BENCHMARK_HARDWARE_DESCRIPTION=${POS3QL_BENCH_HARDWARE_DESCRIPTION:?POS3QL_BENCH_HARDWARE_DESCRIPTION must identify the pinned benchmark host}
  BENCHMARK_NETWORK_DESCRIPTION=${POS3QL_BENCH_NETWORK_DESCRIPTION:?POS3QL_BENCH_NETWORK_DESCRIPTION must describe the path to the external object store}
  CACHE_STORAGE_DESCRIPTION=${POS3QL_BENCH_CACHE_STORAGE:?POS3QL_BENCH_CACHE_STORAGE must describe the local cache storage}
  if [ "$MODE" != smoke ]; then
    : "${POS3QL_BENCH_POSTGRES_STORAGE:?POS3QL_BENCH_POSTGRES_STORAGE must describe the host-available PostgreSQL storage}"
    : "${POS3QL_BENCH_MATCHED_POSTGRES_STORAGE:?POS3QL_BENCH_MATCHED_POSTGRES_STORAGE must describe the resource-matched PostgreSQL storage}"
  fi
  if [ "${POS3QL_BENCH_OBJECT_STORE_INDEPENDENTLY_OPERATED:-}" != 1 ]; then
    echo "external object-store runs require POS3QL_BENCH_OBJECT_STORE_INDEPENDENTLY_OPERATED=1" >&2
    exit 2
  fi
  if [ "$OBJECT_STORE_TLS" != on ]; then
    echo "external object-store runs require POS3QL_BENCH_OBJECT_STORE_TLS=on" >&2
    exit 2
  fi
  case "$OBJECT_STORE_ADDRESSING" in
    path|virtual_hosted) ;;
    *) echo "POS3QL_BENCH_OBJECT_STORE_ADDRESSING must be path or virtual_hosted" >&2; exit 2 ;;
  esac
  OBJECT_STORE_INDEPENDENT_IMPLEMENTATION=1
  OBJECT_STORE_INDEPENDENTLY_OPERATED=1
elif [ "$OBJECT_STORE" = fixture ]; then
  S3_PORT=$(claim_test_port "${POS3QL_BENCH_S3_PORT:-}" 19500 19599)
  OBJECT_STORE_ENDPOINT="127.0.0.1:$S3_PORT"
  METRICS="$WORK/object-store-metrics.json"
  OBJECT_STORE_REQUEST_METRICS=1
  OBJECT_STORE_IMPLEMENTATION=tests/external/s3_test_server.py
  OBJECT_STORE_BACKING="temporary local filesystem"
  python3 "$ROOT/tests/external/s3_test_server.py" \
    --root "$WORK/objects" --port "$S3_PORT" --bucket "$OBJECT_STORE_BUCKET" \
    --region "$OBJECT_STORE_REGION" --access-key "$OBJECT_STORE_ACCESS_KEY" \
    --secret-key "$OBJECT_STORE_SECRET_KEY" \
    --metrics-file "$METRICS" --latency-ms "$LATENCY_MS" &
  S3_PID=$!
else
  S3_PORT=$(claim_test_port "${POS3QL_BENCH_S3_PORT:-}" 19500 19599)
  OBJECT_STORE_ENDPOINT="127.0.0.1:$S3_PORT"
  command -v docker >/dev/null 2>&1 || {
    echo "Docker is required for the $OBJECT_STORE benchmark backend" >&2
    exit 1
  }
  OBJECT_STORE_CONTAINER="pos3ql-performance-$OBJECT_STORE-$$"
  OBJECT_STORE_REGION=us-east-1
  OBJECT_STORE_INDEPENDENT_IMPLEMENTATION=1
  OBJECT_STORE_BACKING="ephemeral Docker container storage on the benchmark host"
  if [ "$OBJECT_STORE" = minio ]; then
    OBJECT_STORE_IMPLEMENTATION=MinIO
    OBJECT_STORE_IMAGE=${POS3QL_BENCH_MINIO_IMAGE:-quay.io/minio/minio@sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e}
    docker run -d --name "$OBJECT_STORE_CONTAINER" -p "$S3_PORT:9000" \
      -e MINIO_ROOT_USER="$OBJECT_STORE_ACCESS_KEY" \
      -e MINIO_ROOT_PASSWORD="$OBJECT_STORE_SECRET_KEY" \
      "$OBJECT_STORE_IMAGE" server /data >/dev/null
  else
    OBJECT_STORE_IMPLEMENTATION=SeaweedFS
    OBJECT_STORE_IMAGE=${POS3QL_BENCH_SEAWEEDFS_IMAGE:-chrislusf/seaweedfs@sha256:08d516132314207d10c8e37cbffc1f32b147d870169688734cc61c6231625b62}
    docker run -d --name "$OBJECT_STORE_CONTAINER" -p "$S3_PORT:8333" \
      -e AWS_ACCESS_KEY_ID="$OBJECT_STORE_ACCESS_KEY" \
      -e AWS_SECRET_ACCESS_KEY="$OBJECT_STORE_SECRET_KEY" \
      -e S3_BUCKET="$OBJECT_STORE_BUCKET" \
      "$OBJECT_STORE_IMAGE" mini -dir=/data >/dev/null
  fi
fi

if [ "$OBJECT_STORE" != external ]; then
  for attempt in $(seq 1 100); do
    if [ -n "$S3_PID" ] && ! server_alive "$S3_PID"; then
      echo "object-store fixture exited at startup" >&2
      exit 1
    fi
    if [ -n "$OBJECT_STORE_CONTAINER" ] &&
       [ "$(docker inspect --format '{{.State.Running}}' "$OBJECT_STORE_CONTAINER")" != true ]; then
      docker logs "$OBJECT_STORE_CONTAINER" >&2
      exit 1
    fi
    if nc -z 127.0.0.1 "$S3_PORT" >/dev/null 2>&1; then break; fi
    if [ "$attempt" = 100 ]; then
      [ -z "$OBJECT_STORE_CONTAINER" ] || docker logs "$OBJECT_STORE_CONTAINER" >&2
      echo "$OBJECT_STORE object store did not start" >&2
      exit 1
    fi
    sleep 0.05
  done
fi
if [ -n "$OBJECT_STORE_CONTAINER" ]; then
  for attempt in $(seq 1 100); do
    if curl --silent --output /dev/null "http://$OBJECT_STORE_ENDPOINT/"; then break; fi
    if [ "$attempt" = 100 ]; then
      docker logs "$OBJECT_STORE_CONTAINER" >&2
      echo "$OBJECT_STORE S3 API did not become ready" >&2
      exit 1
    fi
    sleep 0.2
  done
fi
if [ -n "$METRICS" ]; then
  for attempt in $(seq 1 100); do
    if [ -s "$METRICS" ]; then break; fi
    if [ "$attempt" = 100 ]; then echo "object-store metrics did not start" >&2; exit 1; fi
    sleep 0.05
  done
elif [ "$OBJECT_STORE" = minio ]; then
  docker exec "$OBJECT_STORE_CONTAINER" mc alias set local \
    http://127.0.0.1:9000 "$OBJECT_STORE_ACCESS_KEY" "$OBJECT_STORE_SECRET_KEY" >/dev/null
  docker exec "$OBJECT_STORE_CONTAINER" mc mb local/"$OBJECT_STORE_BUCKET" >/dev/null
fi
if [ -n "$OBJECT_STORE_CONTAINER" ]; then
  OBJECT_STORE_IMAGE_ID=$(docker inspect --format '{{.Image}}' "$OBJECT_STORE_CONTAINER")
  printf '%s\n' "$OBJECT_STORE_IMAGE_ID" >"$OUTPUT/object-store-image-id.txt"
fi

POS3QL_PORT=$(claim_test_port "${POS3QL_BENCH_PORT:-}" 19600 19699)

if [ "$CHECKPOINT_PROFILE" = 1 ]; then
  cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" \
    --features checkpoint-profile
else
  cargo build --release --locked --manifest-path "$ROOT/Cargo.toml"
fi

write_config() {
  config=$1
  data=$2
  prefix=$3
  port=$4
  object_prefix=$prefix
  if [ -n "$OBJECT_STORE_PREFIX_ROOT" ]; then
    object_prefix="${OBJECT_STORE_PREFIX_ROOT%/}/$prefix"
  fi
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
max_views = $MAX_VIEWS
max_rules = $MAX_RULES
table_rows = $TABLE_CAPACITY
value_index_rows = $TABLE_CAPACITY
max_value_indexes = 24
max_large_objects = 16
large_object_pages = 64
max_large_object_descriptors = 8
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
object_store_endpoint = $OBJECT_STORE_ENDPOINT
object_store_bucket = $OBJECT_STORE_BUCKET
object_store_prefix = $object_prefix
object_store_region = $OBJECT_STORE_REGION
object_store_access_key = $OBJECT_STORE_ACCESS_KEY
object_store_secret_key = $OBJECT_STORE_SECRET_KEY
object_store_session_token = $OBJECT_STORE_SESSION_TOKEN
object_store_addressing = $OBJECT_STORE_ADDRESSING
object_store_tls = $OBJECT_STORE_TLS
object_store_tls_ca_file = $OBJECT_STORE_TLS_CA_FILE
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
  if [ -n "$METRICS" ]; then
    cp "$METRICS" "$WORK/$recovery_label-before.json"
  fi
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
      if [ -n "$METRICS" ]; then
        cp "$METRICS" "$WORK/$recovery_label-after.json"
        if [ "$require_read" = yes ]; then
          python3 "$ROOT/tools/object-metrics-diff.py" \
            --before "$WORK/$recovery_label-before.json" \
            --after "$WORK/$recovery_label-after.json" --require-read \
            --label "$recovery_label" --elapsed-seconds "$startup_seconds" \
            --output "$OUTPUT/$recovery_label.json"
        else
          python3 "$ROOT/tools/object-metrics-diff.py" \
            --before "$WORK/$recovery_label-before.json" \
            --after "$WORK/$recovery_label-after.json" \
            --label "$recovery_label" --elapsed-seconds "$startup_seconds" \
            --output "$OUTPUT/$recovery_label.json"
        fi
      else
        python3 "$ROOT/tools/object-metrics-diff.py" --label "$recovery_label" \
          --elapsed-seconds "$startup_seconds" --output "$OUTPUT/$recovery_label.json"
      fi
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
  if [ -n "$METRICS" ]; then
    python3 "$ROOT/tools/benchmark.py" \
      --port "$POS3QL_PORT" --label "$label" --pid "$SERVER_PID" \
      --fixed-memory-bytes "$MEMORY_PLAN_BYTES" --object-metrics "$METRICS" \
      --output "$OUTPUT/$label.json" --check "$@" >/dev/null
  else
    python3 "$ROOT/tools/benchmark.py" \
      --port "$POS3QL_PORT" --label "$label" --pid "$SERVER_PID" \
      --fixed-memory-bytes "$MEMORY_PLAN_BYTES" \
      --output "$OUTPUT/$label.json" --check "$@" >/dev/null
  fi
}

start_postgresql_baseline() {
  profile=$1
  case "$profile" in
    host-available)
      POSTGRES_PORT=${POS3QL_BENCH_POSTGRES_PORT:-}
      postgres_storage=${POS3QL_BENCH_POSTGRES_STORAGE:-Docker-managed local volume; host backing unspecified}
      server_output="$OUTPUT/postgresql-server.json"
      image_output="$OUTPUT/postgresql-image-id.txt"
      ;;
    resource-matched)
      POSTGRES_PORT=
      postgres_storage=${POS3QL_BENCH_MATCHED_POSTGRES_STORAGE:-Docker-managed local volume; host backing unspecified; container limits recorded}
      server_output="$OUTPUT/postgresql-matched-server.json"
      image_output="$OUTPUT/postgresql-matched-image-id.txt"
      ;;
    *) echo "unknown PostgreSQL resource profile: $profile" >&2; exit 2 ;;
  esac
  if [ -z "$POSTGRES_PORT" ]; then
    command -v docker >/dev/null 2>&1 || {
      echo "Docker is required for PostgreSQL benchmark baselines" >&2
      exit 1
    }
    POSTGRES_PORT=$(claim_test_port "" 19700 19799)
    POSTGRES_IMAGE=${POS3QL_BENCH_POSTGRES_IMAGE:-postgres:18}
    POSTGRES_CONTAINER="pos3ql-performance-${profile}-$$"
    if [ "$profile" = resource-matched ]; then
      docker run -d --name "$POSTGRES_CONTAINER" -p "$POSTGRES_PORT:5432" \
        --cpus "$MATCHED_POSTGRES_CPUS" --memory "$MEMORY_PLAN_BYTES" \
        --memory-swap "$MEMORY_PLAN_BYTES" \
        -e POSTGRES_HOST_AUTH_METHOD=trust "$POSTGRES_IMAGE" >/dev/null
    else
      docker run -d --name "$POSTGRES_CONTAINER" -p "$POSTGRES_PORT:5432" \
        -e POSTGRES_HOST_AUTH_METHOD=trust "$POSTGRES_IMAGE" >/dev/null
    fi
    for attempt in $(seq 1 200); do
      if docker exec "$POSTGRES_CONTAINER" pg_isready -U postgres >/dev/null 2>&1; then break; fi
      if [ "$attempt" = 200 ]; then docker logs "$POSTGRES_CONTAINER" >&2; exit 1; fi
      sleep 0.1
    done
    python3 "$ROOT/tools/pg-query.py" --port "$POSTGRES_PORT" --expect 1 \
      --timeout 30 "SELECT 1" >/dev/null
    docker inspect --format '{{.Image}}' "$POSTGRES_CONTAINER" >"$image_output"
  else
    postgres_storage=${POS3QL_BENCH_POSTGRES_STORAGE:?POS3QL_BENCH_POSTGRES_STORAGE must describe the external PostgreSQL storage}
  fi
  if [ -n "$POSTGRES_CONTAINER" ]; then
    if [ "$profile" = resource-matched ]; then
      python3 "$ROOT/tools/benchmark-postgresql.py" --port "$POSTGRES_PORT" \
        --storage-description "$postgres_storage" --resource-profile "$profile" \
        --expected-cpus "$MATCHED_POSTGRES_CPUS" \
        --expected-memory-bytes "$MEMORY_PLAN_BYTES" \
        --output "$server_output" --docker-container "$POSTGRES_CONTAINER"
    else
      python3 "$ROOT/tools/benchmark-postgresql.py" --port "$POSTGRES_PORT" \
        --storage-description "$postgres_storage" --resource-profile "$profile" \
        --output "$server_output" --docker-container "$POSTGRES_CONTAINER"
    fi
  else
    python3 "$ROOT/tools/benchmark-postgresql.py" --port "$POSTGRES_PORT" \
      --storage-description "$postgres_storage" --resource-profile external \
      --output "$server_output"
  fi
}

stop_postgresql_baseline() {
  if [ -n "$POSTGRES_CONTAINER" ]; then
    docker rm -f "$POSTGRES_CONTAINER" >/dev/null
    POSTGRES_CONTAINER=
  fi
  POSTGRES_PORT=
}

run_checkpoint_postgresql_suite() {
  prefix=$1
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-point-concurrency-1" --workload point-read --clients 1 \
    --operations "$OPERATIONS" --rows "$ROWS" --setup --check \
    --output "$OUTPUT/$prefix-point-concurrency-1.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-mixed-baseline" --workload mixed --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --duration-seconds "$CHECKPOINT_DURATION" --check \
    --output "$OUTPUT/$prefix-mixed-baseline.json" >/dev/null
  python3 "$ROOT/tools/pg-query.py" --port "$POSTGRES_PORT" \
    --timeout "$BENCH_TIMEOUT_SECONDS" "CHECKPOINT" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-mixed-checkpoint-interference" --workload mixed --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --duration-seconds "$CHECKPOINT_DURATION" \
    --maintenance-interval 0.001 --maintenance-limit 3 \
    --require-maintenance-operations 3 --check \
    --output "$OUTPUT/$prefix-mixed-checkpoint-interference.json" >/dev/null
}

run_full_postgresql_suite() {
  prefix=$1
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-point-concurrency-1" --workload point-read --clients 1 \
    --operations "$OPERATIONS" --rows "$ROWS" --setup --check \
    --output "$OUTPUT/$prefix-point-concurrency-1.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-point" --workload point-read --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/$prefix-point.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-tail-range" --workload tail-range --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/$prefix-tail-range.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-ordered-limit" --workload ordered-limit --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/$prefix-ordered-limit.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-join-probe" --workload join-probe --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/$prefix-join-probe.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-catalog-lookup" --workload catalog-lookup --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/$prefix-catalog-lookup.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-insert" --workload insert --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/$prefix-insert.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-scan" --workload scan --clients 1 \
    --operations "$((OPERATIONS / 20 + 1))" --rows "$ROWS" --check \
    --output "$OUTPUT/$prefix-scan.json" >/dev/null
  python3 "$ROOT/tools/benchmark.py" --port "$POSTGRES_PORT" \
    --label "$prefix-mixed" --workload mixed --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --check \
    --output "$OUTPUT/$prefix-mixed.json" >/dev/null
}

if [ "$MODE" = smoke ]; then
  ROWS=128
  TABLE_CAPACITY=512
  OPERATIONS=8
  CLIENTS=4
  REPLICA_SETTING=0
  CATALOG_RELATIONS=${POS3QL_BENCH_CATALOG_RELATIONS:-16}
else
  ROWS=${POS3QL_BENCH_ROWS:-10000}
  TABLE_CAPACITY=${POS3QL_BENCH_TABLE_CAPACITY:-16384}
  OPERATIONS=${POS3QL_BENCH_OPERATIONS:-500}
  CLIENTS=${POS3QL_BENCH_CLIENTS:-8}
  REPLICA_SETTING=${POS3QL_BENCH_REPLICAS:-2}
  if [ "$MODE" = full ]; then
    CATALOG_RELATIONS=${POS3QL_BENCH_CATALOG_RELATIONS:-128}
  else
    CATALOG_RELATIONS=${POS3QL_BENCH_CATALOG_RELATIONS:-0}
  fi
fi
if ! [[ "$ROWS" =~ ^[1-9][0-9]*$ && "$TABLE_CAPACITY" =~ ^[1-9][0-9]*$ &&
  "$OPERATIONS" =~ ^[1-9][0-9]*$ && "$CLIENTS" =~ ^[1-9][0-9]*$ ]]; then
  echo "benchmark rows, table capacity, operations, and clients must be positive decimal integers" >&2
  exit 2
fi
if [ "$MODE" = full ] && [ "$CLIENTS" -lt 4 ]; then
  echo "full mode requires at least four clients for the group-commit amplification gate" >&2
  exit 2
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
if ! [[ "$CATALOG_RELATIONS" =~ ^(0|[1-9][0-9]*)$ ]]; then
  echo "POS3QL_BENCH_CATALOG_RELATIONS must be a nonnegative decimal count" >&2
  exit 2
fi
if [ "$MODE" = checkpoint ] && [ "$CATALOG_RELATIONS" != 0 ]; then
  echo "checkpoint mode requires POS3QL_BENCH_CATALOG_RELATIONS=0" >&2
  exit 2
fi
if [ "$MODE" != checkpoint ] && [ "$CATALOG_RELATIONS" = 0 ]; then
  echo "$MODE mode requires POS3QL_BENCH_CATALOG_RELATIONS to be positive" >&2
  exit 2
fi
if [ "$MODE" != smoke ] && [ "$TABLE_CAPACITY" -lt $((ROWS + CLIENTS * OPERATIONS)) ]; then
  echo "POS3QL_BENCH_TABLE_CAPACITY must cover setup and inserted rows" >&2
  exit 2
fi
MAX_CONNECTIONS=$((CLIENTS + 4))
MAX_VIEWS=$((CATALOG_RELATIONS > 0 ? CATALOG_RELATIONS : 1))
MAX_RULES=$((CATALOG_RELATIONS + 32))
export POS3QL_BENCH_CATALOG_RELATIONS=$CATALOG_RELATIONS
python3 "$ROOT/tools/benchmark-environment.py" \
  --output "$OUTPUT/environment.json" --binary "$ROOT/target/release/pos3ql" \
  --mode "$MODE" --rows "$ROWS" --table-capacity "$TABLE_CAPACITY" \
  --operations "$OPERATIONS" --clients "$CLIENTS" \
  --catalog-relations "$CATALOG_RELATIONS" \
  --replicas "$REPLICA_SETTING" --object-latency-ms "$LATENCY_MS" \
  --object-latency-injected "$OBJECT_LATENCY_INJECTED" \
  --object-store-implementation "$OBJECT_STORE_IMPLEMENTATION" \
  --object-store-backing "$OBJECT_STORE_BACKING" \
  --object-store-image "$OBJECT_STORE_IMAGE" \
  --object-store-image-id "$OBJECT_STORE_IMAGE_ID" \
  --object-store-independent-implementation "$OBJECT_STORE_INDEPENDENT_IMPLEMENTATION" \
  --object-store-independently-operated "$OBJECT_STORE_INDEPENDENTLY_OPERATED" \
  --object-store-request-metrics "$OBJECT_STORE_REQUEST_METRICS" \
  --object-store-endpoint "$OBJECT_STORE_ENDPOINT" \
  --object-store-bucket "$OBJECT_STORE_BUCKET" \
  --object-store-prefix "$OBJECT_STORE_PREFIX_ROOT" \
  --object-store-region "$OBJECT_STORE_REGION" \
  --object-store-addressing "$OBJECT_STORE_ADDRESSING" \
  --object-store-tls "$([ "$OBJECT_STORE_TLS" = on ] && echo 1 || echo 0)" \
  --object-store-tls-ca-file "$OBJECT_STORE_TLS_CA_FILE" \
  --hardware-description "$BENCHMARK_HARDWARE_DESCRIPTION" \
  --network-description "$BENCHMARK_NETWORK_DESCRIPTION" \
  --cache-storage-description "$CACHE_STORAGE_DESCRIPTION" \
  --disk-cache-mib "$DISK_CACHE_MIB" --timeout-seconds "$BENCH_TIMEOUT_SECONDS" \
  --checkpoint-profile "$CHECKPOINT_PROFILE" \
  --checkpoint-duration-seconds "$CHECKPOINT_DURATION"

DATA_WARM="$WORK/data-warm"
start_pos3ql "$DATA_WARM" primary initial-start
bench_pos3ql point-concurrency-1 --workload point-read --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS" --setup --require-index
if [ "$MODE" = checkpoint ]; then
  bench_pos3ql mixed-baseline --workload mixed --clients "$CLIENTS" \
    --operations "$OPERATIONS" --rows "$ROWS" --duration-seconds "$CHECKPOINT_DURATION"
  # Drain publication and cleanup triggered by the baseline before opening
  # the interference profile window.
  python3 "$ROOT/tools/pg-query.py" --port "$POS3QL_PORT" \
    --timeout "$BENCH_TIMEOUT_SECONDS" "CHECKPOINT" >/dev/null
  PROFILE_OFFSET=0
  if [ "$CHECKPOINT_PROFILE" = 1 ]; then
    PROFILE_OFFSET=$(wc -c < "$WORK/pos3ql-primary.log")
    if [ -n "$METRICS" ]; then cp "$METRICS" "$WORK/checkpoint-profile-before.json"; fi
  fi
  if [ "$CHECKPOINT_PROFILE" = 1 ]; then
    bench_pos3ql mixed-checkpoint-interference --workload mixed --clients "$CLIENTS" \
      --operations "$OPERATIONS" --rows "$ROWS" --duration-seconds "$CHECKPOINT_DURATION" \
      --maintenance-interval 0.001 --maintenance-limit 3 \
      --require-maintenance-operations 3 --trace-operations
  else
    bench_pos3ql mixed-checkpoint-interference --workload mixed --clients "$CLIENTS" \
      --operations "$OPERATIONS" --rows "$ROWS" --duration-seconds "$CHECKPOINT_DURATION" \
      --maintenance-interval 0.001 --maintenance-limit 3 \
      --require-maintenance-operations 3
  fi
  stop_pos3ql
  if [ "$CHECKPOINT_PROFILE" = 1 ]; then
    sleep 0.1
    if [ -n "$METRICS" ]; then cp "$METRICS" "$WORK/checkpoint-profile-after.json"; fi
  fi
  start_postgresql_baseline host-available
  run_checkpoint_postgresql_suite postgresql18
  stop_postgresql_baseline
  start_postgresql_baseline resource-matched
  run_checkpoint_postgresql_suite postgresql18-matched
  stop_postgresql_baseline
  cp "$WORK/pos3ql-primary.log" "$OUTPUT/pos3ql-startup.log"
  if [ "$CHECKPOINT_PROFILE" = 1 ]; then
    if [ -n "$METRICS" ]; then
      python3 "$ROOT/tools/checkpoint-profile.py" \
        "$OUTPUT/pos3ql-startup.log" "$OUTPUT/checkpoint-profile.json" \
        --after-byte-offset "$PROFILE_OFFSET" \
        --metrics-before "$WORK/checkpoint-profile-before.json" \
        --metrics-after "$WORK/checkpoint-profile-after.json" \
        --operation-trace "$OUTPUT/mixed-checkpoint-interference.json"
    else
      python3 "$ROOT/tools/checkpoint-profile.py" \
        "$OUTPUT/pos3ql-startup.log" "$OUTPUT/checkpoint-profile.json" \
        --after-byte-offset "$PROFILE_OFFSET" \
        --operation-trace "$OUTPUT/mixed-checkpoint-interference.json"
    fi
  fi
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
bench_pos3ql warm-memory-catalog-lookup --workload catalog-lookup --clients "$CLIENTS" \
  --operations "$OPERATIONS" --rows "$ROWS"
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
bench_pos3ql cold-object-catalog-lookup --workload catalog-lookup --clients 1 \
  --operations "$OPERATIONS" --rows "$ROWS"

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

  start_postgresql_baseline host-available
  run_full_postgresql_suite postgresql18
  stop_postgresql_baseline
  start_postgresql_baseline resource-matched
  run_full_postgresql_suite postgresql18-matched
  stop_postgresql_baseline
fi

cp "$WORK/pos3ql-primary.log" "$OUTPUT/pos3ql-startup.log"
python3 "$ROOT/tools/benchmark-report.py" "$OUTPUT" >"$OUTPUT/report.md"
echo "performance results: $OUTPUT"
