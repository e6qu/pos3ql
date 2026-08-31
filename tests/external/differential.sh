#!/usr/bin/env bash
# Differential conformance: run the same SQL corpus against REAL
# PostgreSQL 18 and against pos3ql, normalize, and diff.
#
# This is a generic validator for PostgreSQL implementations: rows, tags,
# and column headers must match exactly; errors must match by SQLSTATE
# (message wording is normalized away). Any diff is a semantic divergence.
#
# Usage: tests/external/differential.sh [--keep]

set -u
cd "$(dirname "$0")/../.." || exit
EXT=tests/external
ROOT_VENV=${POS3QL_VENV:-target/external-venv}
EXTENSION_CONTROL_ROOT=${POS3QL_EXTENSION_CONTROL_PATH:-$PWD/$EXT/extensions}
REFERENCE_EXTENSION_CONTROL_ROOT=${POS3QL_REFERENCE_EXTENSION_CONTROL_PATH:-$PWD/$EXT/extensions}
WORK=$(mktemp -d /tmp/pos3ql-diff.XXXXXX)
KEEP=${1:-}

PGBIN=${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}
REFERENCE_HOST=${POS3QL_REFERENCE_PG_HOST:-}
REFERENCE_PORT=${POS3QL_REFERENCE_PG_PORT:-}
REFERENCE_DATABASE=${POS3QL_REFERENCE_PG_DATABASE:-postgres}
if [[ -n "$REFERENCE_HOST" ]]; then
  if [[ -z "$REFERENCE_PORT" || -z "${POS3QL_REFERENCE_PSQL:-}" ]]; then
    printf '%s\n' 'FAIL: an external PostgreSQL reference requires both POS3QL_REFERENCE_PG_PORT and POS3QL_REFERENCE_PSQL'
    exit 1
  fi
  PSQL=$POS3QL_REFERENCE_PSQL
  PG_PORT=$REFERENCE_PORT
  REFERENCE_MODE=external
else
  PSQL="$PGBIN/psql"
  REFERENCE_MODE=local
fi

# A developer machine can already have a PostgreSQL or pos3ql instance on the
# historical defaults. Pick an unused pair for the hermetic local run; an
# explicit port remains an explicit contract and fails before startup if busy.
port_is_free() {
  ! nc -z 127.0.0.1 "$1" >/dev/null 2>&1
}

choose_local_ports() {
  local requested_pg=${POS3QL_DIFF_PG_PORT:-}
  local requested_p3=${POS3QL_DIFF_P3_PORT:-}
  local candidate

  if [[ -n "$requested_pg" ]] && ! port_is_free "$requested_pg"; then
    printf 'FAIL: requested PostgreSQL reference port %s is already in use\n' "$requested_pg"
    exit 1
  fi
  if [[ -n "$requested_p3" ]] && ! port_is_free "$requested_p3"; then
    printf 'FAIL: requested pos3ql port %s is already in use\n' "$requested_p3"
    exit 1
  fi

  if [[ -n "$requested_pg" && -n "$requested_p3" ]]; then
    PG_PORT=$requested_pg
    P3_PORT=$requested_p3
    return
  fi

  for ((candidate = 15498; candidate <= 15598; candidate += 2)); do
    local pg=${requested_pg:-$candidate}
    local p3=${requested_p3:-$((candidate + 1))}
    if port_is_free "$pg" && port_is_free "$p3"; then
      PG_PORT=$pg
      P3_PORT=$p3
      return
    fi
  done
  printf '%s\n' 'FAIL: no free local loopback port pair for differential testing'
  exit 1
}

if [[ "$REFERENCE_MODE" == local ]]; then
  choose_local_ports
else
  P3_PORT=${POS3QL_DIFF_P3_PORT:-15499}
fi
FUZZ_COUNT=${POS3QL_FUZZ_COUNT:-0}
FUZZ_SEED=${POS3QL_FUZZ_SEED:-1}
DIFF_OBJECT_PREFIX=${POS3QL_DIFF_OBJECT_STORE_PREFIX:-}
CORPUS_SHARD=${POS3QL_DIFF_CORPUS_SHARD:-}
CORPUS_SHARD_INDEX=0
CORPUS_SHARD_COUNT=1

if [[ -n "$CORPUS_SHARD" ]]; then
  if [[ ! "$CORPUS_SHARD" =~ ^([0-9]+)-of-([1-9][0-9]*)$ ]]; then
    printf 'FAIL: POS3QL_DIFF_CORPUS_SHARD must be INDEX-of-COUNT\n'
    exit 1
  fi
  CORPUS_SHARD_INDEX=${BASH_REMATCH[1]}
  CORPUS_SHARD_COUNT=${BASH_REMATCH[2]}
  if (( CORPUS_SHARD_INDEX >= CORPUS_SHARD_COUNT )); then
    printf 'FAIL: POS3QL_DIFF_CORPUS_SHARD index must be smaller than count\n'
    exit 1
  fi
fi

# A sharded run has one explicitly assigned owner for the non-corpus probes.
# Repeating them in every shard needlessly consumes the fixed CI time budget.
if [[ -n "$CORPUS_SHARD" && -z "${POS3QL_DIFF_AUXILIARY+x}" ]]; then
  printf '%s\n' 'FAIL: sharded differential runs require POS3QL_DIFF_AUXILIARY=all or none'
  exit 1
fi
if [[ -n "$CORPUS_SHARD" ]]; then
  DIFF_AUXILIARY=$POS3QL_DIFF_AUXILIARY
else
  DIFF_AUXILIARY=all
fi
case "$DIFF_AUXILIARY" in
all | none) ;;
*)
  printf 'FAIL: POS3QL_DIFF_AUXILIARY must be all or none (got %q)\n' "$DIFF_AUXILIARY"
  exit 1
  ;;
esac

corpus_file_count=0
for corpus_file in "$EXT"/differential/*.sql; do
  [[ -f "$corpus_file" ]] || continue
  corpus_file_count=$((corpus_file_count + 1))
done
if (( CORPUS_SHARD_COUNT > corpus_file_count )); then
  printf 'FAIL: POS3QL_DIFF_CORPUS_SHARD count exceeds the corpus count\n'
  exit 1
fi

if [[ "${POS3QL_DIFF_OBJECT_STORE:-off}" == "on" && -z "$DIFF_OBJECT_PREFIX" ]]; then
  printf '%s\n' 'FAIL: durable differential runs require POS3QL_DIFF_OBJECT_STORE_PREFIX'
  exit 1
fi

PASS=0
FAIL=0
ok()  { PASS=$((PASS+1)); printf 'PASS: %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf 'FAIL: %s\n' "$1"; }

. "$EXT/liveness.sh"

cleanup() {
  [[ -n "${P3_PID:-}" ]] && kill "$P3_PID" 2>/dev/null
  if [[ "$REFERENCE_MODE" == local && -d "$WORK/pgdata" ]]; then
    "$PGBIN/pg_ctl" -D "$WORK/pgdata" stop -m immediate >/dev/null 2>&1
  fi
  [[ -n "${SOCKDIR:-}" ]] && rm -rf "$SOCKDIR"
  if [[ "$KEEP" == "--keep" ]]; then
    printf 'work dir kept: %s\n' "$WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

if [[ "$REFERENCE_MODE" == local ]]; then
  printf '=== reference: %s ===\n' "$("$PGBIN/postgres" --version)"
else
  if ! reference_version=$("$PSQL" -h "$REFERENCE_HOST" -p "$PG_PORT" -U postgres -d "$REFERENCE_DATABASE" -X -t -A -c 'SHOW server_version' 2>&1); then
    printf 'FAIL: external PostgreSQL reference is unavailable: %s\n' "$reference_version"
    exit 1
  fi
  printf '=== reference: PostgreSQL %s ===\n' "$reference_version"
fi

if python3 "$EXT/result_diff.py" >/dev/null; then
  ok "differential result comparator"
else
  bad "differential result comparator"
  exit 1
fi

# A local reference is hermetic. The explicitly configured external mode is
# used only by CI's disposable PostgreSQL 18 service, avoiding a second server
# installation in its five-minute coverage worker.
if [[ "$REFERENCE_MODE" == local ]]; then
  if ! "$PGBIN/initdb" -D "$WORK/pgdata" -U postgres -A trust --encoding=UTF8 --lc-collate=C --lc-ctype=C \
    > "$WORK/initdb.log" 2>&1; then
    bad initdb
    cat "$WORK/initdb.log"
    exit 1
  fi
  SOCKDIR=$(mktemp -d /tmp/pos3ql-pgsock.XXXX)
  "$PGBIN/pg_ctl" -D "$WORK/pgdata" -o "-p $PG_PORT -k $SOCKDIR -c listen_addresses=127.0.0.1 -c timezone=UTC -c max_prepared_transactions=8 -c extension_control_path=$REFERENCE_EXTENSION_CONTROL_ROOT" \
    -l "$WORK/pg.log" start >/dev/null || { bad "pg start"; exit 1; }
fi

# pos3ql (object storage off by default: this suite is pure SQL semantics).
# POS3QL_EXTRA_CONF appends config lines — the forced-spill mode runs the
# same corpora with a deliberately tiny memtable over a real bucket, so
# every query also exercises the spill/checkpoint/read-back path.
cargo build --release -q || { bad build; exit 1; }
cat > "$WORK/p3.conf" <<EOF
listen_addr = 127.0.0.1:${P3_PORT}
data_dir = ${WORK}/p3data
object_store = ${POS3QL_DIFF_OBJECT_STORE:-off}
max_tables = 64
table_rows = 8192
max_value_indexes = 64
max_prepared_transactions = 8
memtable_bytes = ${POS3QL_DIFF_MEMTABLE:-256MiB}
extension_control_path = ${EXTENSION_CONTROL_ROOT}
${POS3QL_EXTRA_CONF:-}
EOF
if [[ -n "$DIFF_OBJECT_PREFIX" ]]; then
  printf 'object_store_prefix = %scorpus/\n' "$DIFF_OBJECT_PREFIX" >> "$WORK/p3.conf"
fi
# A leftover server on our port would silently answer the readiness probe
# below and the whole run would test a stale binary. Refuse to start.
if nc -z 127.0.0.1 "$P3_PORT" 2>/dev/null; then
  bad "port $P3_PORT is already in use (stale pos3ql from an earlier run?) — kill it first"
  exit 1
fi

"${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
P3_PID=$!

for _ in {1..50}; do
  "$PSQL" -h 127.0.0.1 -p "$PG_PORT" -U postgres -d "$REFERENCE_DATABASE" -X -q -c "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
P3_READY=0
for _ in {1..50}; do
  if "$PSQL" -h 127.0.0.1 -p "$P3_PORT" -U postgres -X -q -c "SELECT 1" >/dev/null 2>&1; then
    P3_READY=1
    break
  fi
  sleep 0.1
done
# The probe succeeding proves *a* server answered — make sure it is ours.
if [[ $P3_READY -ne 1 ]] || ! server_alive "$P3_PID"; then
  bad "pos3ql did not accept connections at startup (see $WORK/p3.log)"
  tail -20 "$WORK/p3.log"
  exit 1
fi

# Normalizer: error wording differs between implementations; SQLSTATEs and
# result rows must not.
normalize() {
  sed -E \
    -e 's/^psql:[^:]*:[0-9]+: ERROR:  ([0-9A-Z]{5}):.*/ERROR \1/' \
    -e 's/^ERROR:  ([0-9A-Z]{5}):.*/ERROR \1/' \
    -e '/^LINE [0-9]+:/d' \
    -e '/^ *\^ *$/d' \
    -e '/^psql:[^:]*:[0-9]+: (HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|DATATYPE NAME|NOTICE|WARNING):/d' \
    -e '/^HINT:/d' \
    -e '/^DETAIL:/d' \
    -e '/^LOCATION:/d' \
    -e '/^CONTEXT:/d' \
    -e '/^SCHEMA NAME:/d' \
    -e '/^TABLE NAME:/d' \
    -e '/^COLUMN NAME:/d' \
    -e '/^CONSTRAINT NAME:/d' \
    -e '/^DATATYPE NAME:/d' \
    -e '/^NOTICE:/d' \
    -e '/^WARNING:/d'
}

run_corpus() { # port name file
  if [[ "$1" == "$PG_PORT" ]]; then
    PGOPTIONS="-c timezone=UTC -c extension_control_path=$REFERENCE_EXTENSION_CONTROL_ROOT" \
      "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -d "$REFERENCE_DATABASE" -X -a -q -P pager=off \
        -v VERBOSITY=verbose -f "$3" 2>&1
  else
    "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -X -a -q -P pager=off \
      -v VERBOSITY=verbose -f "$3" 2>&1
  fi | normalize > "$WORK/$2"
  if [[ "$1" == "$P3_PORT" ]] && ! server_alive "$P3_PID"; then
    bad "pos3ql exited during $(basename "$3")"
    tail -80 "$WORK/p3.log"
    exit 1
  fi
}
reset_user_extensions() { # port
  local database=postgres
  [[ "$1" == "$PG_PORT" ]] && database=$REFERENCE_DATABASE
  "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -d "$database" -X -A -t -q \
    -c "SELECT extname FROM pg_extension WHERE extname LIKE 'pos3ql_%' ORDER BY extname DESC" |
  while IFS= read -r extension; do
    [[ -z "$extension" ]] && continue
    extension=${extension//\"/\"\"}
    "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -d "$database" -X -q \
      -c "DROP EXTENSION \"$extension\" CASCADE" >/dev/null 2>&1
  done
}

# Independent external suites share one bounded catalog. Drop their user
# relations between suites without replacing `public`, whose identity and ACLs
# are PostgreSQL-visible state.
reset_user_relations() { # port
  local database=postgres
  [[ "$1" == "$PG_PORT" ]] && database=$REFERENCE_DATABASE
  "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -d "$database" -X -A -t -q \
    -F $'\t' \
    -c "SELECT n.nspname, c.relname FROM pg_class AS c JOIN pg_namespace AS n ON n.oid = c.relnamespace WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND c.relkind IN ('r', 'p')" |
  while IFS=$'\t' read -r schema relation; do
    [[ -z "$relation" ]] && continue
    schema=${schema//\"/\"\"}
    relation=${relation//\"/\"\"}
    "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -d "$database" -X -q \
      -c "DROP TABLE \"$schema\".\"$relation\" CASCADE" >/dev/null 2>&1
  done
}

reset_pair() {
  reset_user_extensions "$PG_PORT"
  reset_user_extensions "$P3_PORT"
  reset_user_relations "$PG_PORT"
  reset_user_relations "$P3_PORT"
}

restart_pos3ql_clean() {
  kill "$P3_PID" 2>/dev/null
  wait "$P3_PID" 2>/dev/null
  rm -rf "$WORK/p3data"
  "${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
  P3_PID=$!
  for _ in {1..50}; do
    "$PSQL" -h 127.0.0.1 -p "$P3_PORT" -U postgres -X -q -c "SELECT 1" >/dev/null 2>&1 && return
    sleep 0.1
  done
  bad "pos3ql did not restart with clean differential state"
  tail -40 "$WORK/p3.log"
  exit 1
}

printf '%s\n' '=== corpus diffs (real PostgreSQL vs pos3ql) ==='
reset_pair
corpus_ordinal=0
for f in $EXT/differential/*.sql; do
  if (( corpus_ordinal % CORPUS_SHARD_COUNT != CORPUS_SHARD_INDEX )); then
    corpus_ordinal=$((corpus_ordinal + 1))
    continue
  fi
  name=$(basename "$f" .sql)
  run_corpus "$PG_PORT" "$name.pg" "$f"
  run_corpus "$P3_PORT" "$name.p3" "$f"
  if diff -u "$WORK/$name.pg" "$WORK/$name.p3" > "$WORK/$name.diff"; then
    ok "differential: $name"
  else
    bad "differential: $name"
    head -30 "$WORK/$name.diff"
  fi
  reset_pair
  corpus_ordinal=$((corpus_ordinal + 1))
done

if [[ "$DIFF_AUXILIARY" == "all" ]]; then
# Exact-error corpora: the SQLSTATE normalizer above makes wording invisible,
# which let five message-fidelity fixes ship guarded only by unit tests. These
# corpora compare the full ERROR line — SQLSTATE and message text — dropping
# only PostgreSQL's positional decorations (LINE/caret/HINT/...), which pos3ql
# does not emit and which say where, not what.
normalize_exact() {
  sed -E \
    -e 's/^psql:[^:]*:[0-9]+: ERROR:  ([0-9A-Z]{5}): *(.*)/ERROR \1 \2/' \
    -e 's/^ERROR:  ([0-9A-Z]{5}): *(.*)/ERROR \1 \2/' \
    -e '/^LINE [0-9]+:/d' \
    -e '/^ *\^ *$/d' \
    -e '/^psql:[^:]*:[0-9]+: (HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|DATATYPE NAME|NOTICE|WARNING):/d' \
    -e '/^(HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|DATATYPE NAME|NOTICE|WARNING):/d'
}

run_exact() { # port name file
  local database=postgres
  [[ "$1" == "$PG_PORT" ]] && database=$REFERENCE_DATABASE
  "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -d "$database" -X -a -q -P pager=off \
    -v VERBOSITY=verbose -f "$3" 2>&1 | normalize_exact > "$WORK/$2"
}

printf '%s\n' '' '=== exact-error corpora (message wording must match) ==='
restart_pos3ql_clean
for f in $EXT/differential_exact/*.sql; do
  name=$(basename "$f" .sql)
  run_exact "$PG_PORT" "$name.pg" "$f"
  run_exact "$P3_PORT" "$name.p3" "$f"
  if diff -u "$WORK/$name.pg" "$WORK/$name.p3" > "$WORK/$name.diff"; then
    ok "exact errors: $name"
  else
    bad "exact errors: $name"
    head -30 "$WORK/$name.diff"
  fi
  reset_pair
done

printf '%s\n' '' '=== binary COPY (wire bytes + cross-load) ==='
restart_pos3ql_clean
if [[ -x "$ROOT_VENV/bin/python" ]]; then
  if "$ROOT_VENV/bin/python" "$EXT/copy_binary_diff.py" \
       --pg "$PG_PORT" --p3 "$P3_PORT" > "$WORK/copy-binary.out" 2>&1; then
    ok "COPY BINARY differential"
  else
    bad "COPY BINARY differential"
    cat "$WORK/copy-binary.out"
  fi
else
  printf '%s\n' 'SKIP: COPY BINARY differential (need a psycopg venv at $POS3QL_VENV)'
fi
reset_pair

printf '%s\n' '' '=== accepted-type fidelity matrix ==='
restart_pos3ql_clean
if [[ -x "$ROOT_VENV/bin/python" ]]; then
  if "$ROOT_VENV/bin/python" "$EXT/type_fidelity_diff.py" \
       --pg "$PG_PORT" --p3 "$P3_PORT" > "$WORK/type-fidelity.out" 2>&1; then
    ok "accepted-type fidelity matrix ($(tail -1 "$WORK/type-fidelity.out"))"
  else
    bad "accepted-type fidelity matrix"
    cat "$WORK/type-fidelity.out"
  fi
else
  printf '%s\n' 'SKIP: accepted-type fidelity matrix (need a psycopg venv at $POS3QL_VENV)'
fi
reset_pair

printf '%s\n' '' '=== vendored sqllogictest replay (real PostgreSQL is the oracle) ==='
SLT_VENV=${POS3QL_VENV:-$ROOT_VENV}
if [[ -x "$SLT_VENV/bin/python" ]] && [[ -d vendor/test/sqllogictest/test ]]; then
  # The curated corpus deliberately retains trigger targets and audit tables,
  # while SQLLogicTest owns a complete 64-table catalog. A durable run needs
  # a new object prefix as well as a new local cache for independent state.
  kill "$P3_PID" 2>/dev/null
  wait "$P3_PID" 2>/dev/null
  rm -rf "$WORK/p3data"
  SLT_CONF="$WORK/p3-slt.conf"
  if [[ -n "$DIFF_OBJECT_PREFIX" ]]; then
    sed '/^object_store_prefix = /d' "$WORK/p3.conf" > "$SLT_CONF"
    printf 'object_store_prefix = %sslt/\n' "$DIFF_OBJECT_PREFIX" >> "$SLT_CONF"
  else
    cp "$WORK/p3.conf" "$SLT_CONF"
  fi
  "${POS3QL_BIN:-./target/release/pos3ql}" --config "$SLT_CONF" > "$WORK/p3.log" 2>&1 &
  P3_PID=$!
  P3_READY=0
  for _ in {1..50}; do
    if "$PSQL" -h 127.0.0.1 -p "$P3_PORT" -U postgres -X -q -c "SELECT 1" >/dev/null 2>&1; then
      P3_READY=1
      break
    fi
    sleep 0.1
  done
  if [[ $P3_READY -ne 1 ]] || ! server_alive "$P3_PID"; then
    bad "pos3ql did not restart for sqllogictest (see $WORK/p3.log)"
    tail -20 "$WORK/p3.log"
    exit 1
  fi
  SLT_LIMIT=${POS3QL_SLT_LIMIT:-600}
  if "$SLT_VENV/bin/python" "$EXT/slt_diff.py" --pg "$PG_PORT" --p3 "$P3_PORT" \
       --limit "$SLT_LIMIT" \
       vendor/test/sqllogictest/test/*.test vendor/test/sqllogictest/test/evidence/*.test \
       "$EXT"/sqllogictest/*.test \
       > "$WORK/slt.out" 2>&1; then
    ok "sqllogictest differential ($(grep '^TOTAL' "$WORK/slt.out"))"
  else
    bad "sqllogictest differential"
    tail -30 "$WORK/slt.out"
  fi
else
  printf '%s\n' 'SKIP: sqllogictest replay (need a psycopg venv at $POS3QL_VENV and vendor/)'
fi

if (( FUZZ_COUNT > 0 )); then
  printf '%s\n' '' '=== generated SQL differential (PostgreSQL is the oracle) ==='
  if "$SLT_VENV/bin/python" "$EXT/fuzz_diff.py" --pg "$PG_PORT" --p3 "$P3_PORT" \
      --count "$FUZZ_COUNT" --seed "$FUZZ_SEED" --max-unsupported 0 > "$WORK/fuzz.out" 2>&1; then
    ok "generated SQL differential ($(grep '^TOTAL' "$WORK/fuzz.out"))"
  else
    bad "generated SQL differential"
    cat "$WORK/fuzz.out"
  fi
fi
else
  printf '%s\n' 'SKIP: shared exact-error, binary-COPY, type-fidelity, sqllogictest, and fuzz probes are assigned to another shard'
fi

printf '\npassed: %s  failed: %s\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
