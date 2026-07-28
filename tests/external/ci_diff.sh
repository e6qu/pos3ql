#!/usr/bin/env bash
# CI differential conformance: run the SQL corpora and the generative fuzzer
# against a REAL PostgreSQL and against pos3ql, and diff. Unlike
# differential.sh (which spins up its own hermetic Postgres), this expects an
# already-running PostgreSQL — a GitHub Actions `postgres:` service — reached
# via the standard PG* env vars, so it works headless on CI runners.
#
# Env:
#   PGHOST / PGPORT / PGUSER   PostgreSQL to diff against (default 127.0.0.1 / 5432 / postgres)
#   P3_PORT                    port pos3ql should listen on (default 15599)
#   SLT_LIMIT                  max sqllogictest blocks per file (default 20000 = all vendored)
#   FUZZ_COUNT / FUZZ_SEED     generative fuzz statements / seed (default 20000 / 1)
#   FUZZ_BUDGET                allowed fuzz divergences before failing (ratchet; default 0)
#
# Gating steps (a failure fails CI): wire probe, psycopg driver, the curated
# differential SQL corpus, and the sqllogictest replay. The fuzzer is gated by
# FUZZ_BUDGET so its known edge-case divergences can be ratcheted to zero.

set -u
cd "$(dirname "$0")/../.." || exit
EXT=tests/external
VENV=${POS3QL_VENV:-target/external-venv}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/pos3ql-ci-diff.XXXXXX")

PGHOST=${PGHOST:-127.0.0.1}
PGPORT=${PGPORT:-5432}
PGUSER=${PGUSER:-postgres}
export PGHOST PGPORT PGUSER
P3_PORT=${P3_PORT:-15599}
SLT_LIMIT=${SLT_LIMIT:-20000}
FUZZ_COUNT=${FUZZ_COUNT:-20000}
FUZZ_SEED=${FUZZ_SEED:-1}
FUZZ_BUDGET=${FUZZ_BUDGET:-0}
# Sharding, so each CI job fits a wall-clock cap while total coverage is
# preserved: the sqllogictest replay splits each file's query blocks across
# SLT_QUERY_SHARDS shards (this run does shard SLT_QUERY_SHARD, 0-based) — every
# shard runs all files and all statement/DDL blocks, only the read-only query
# blocks are divided, which balances even a single huge file. RUN_SLT / RUN_FUZZ
# gate the two slow phases so a shard can run one, the other, or both. Defaults
# run everything in one shard, matching the unsharded behavior.
SLT_QUERY_SHARD=${SLT_QUERY_SHARD:-0}
SLT_QUERY_SHARDS=${SLT_QUERY_SHARDS:-1}
RUN_SLT=${RUN_SLT:-1}
RUN_FUZZ=${RUN_FUZZ:-1}

PASS=0 FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }

cleanup() { [[ -n "${P3_PID:-}" ]] && kill "$P3_PID" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

# --- psycopg venv (the differential/fuzz scripts need it) -------------------
if [[ ! -x "$VENV/bin/python" ]]; then
  python3 -m venv "$VENV" && "$VENV/bin/pip" install --quiet 'psycopg[binary]'
fi
PY="$VENV/bin/python"

# --- start pos3ql (object storage off: this suite is pure SQL semantics) ----
cargo build --release -q || { echo "build failed"; exit 1; }
cat > "$WORK/p3.conf" <<EOF
listen_addr = 127.0.0.1:${P3_PORT}
data_dir = ${WORK}/p3data
s3 = off
max_tables = 64
table_rows = 65536
max_value_indexes = 64
memtable_bytes = 256MiB
EOF
"${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
P3_PID=$!

wait_up() { # port
  for _ in $(seq 1 100); do
    psql -h "$PGHOST" -p "$1" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}
wait_up "$PGPORT" || { echo "PostgreSQL not reachable on $PGHOST:$PGPORT"; exit 1; }
psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1 \
  || { for _ in $(seq 1 100); do psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1 && break; sleep 0.1; done; }
echo "reference: $(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc 'SHOW server_version')"

restart_p3_fresh() {
  kill "$P3_PID" 2>/dev/null
  wait "$P3_PID" 2>/dev/null
  rm -rf "${WORK}/p3data"
  "${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
  P3_PID=$!
  for _ in $(seq 1 100); do
    psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "pos3ql did not restart on 127.0.0.1:$P3_PORT"
  return 1
}

# --- raw wire-protocol probes ----------------------------------------------
echo "=== wire protocol probes ==="
if POS3QL_PORT=$P3_PORT python3 "$EXT/wire_probe.py" > "$WORK/wire.out" 2>&1; then
  ok "wire probes"
else bad "wire probes"; cat "$WORK/wire.out"; fi

# --- psycopg driver (extended protocol, binary params) ---------------------
echo "=== psycopg driver ==="
if POS3QL_PORT=$P3_PORT "$PY" - <<EOF > "$WORK/driver.out" 2>&1
import sys
sys.argv = ["driver_test.py"]
src = open("$EXT/driver_test.py").read().replace("port=5433", "port=$P3_PORT")
exec(compile(src, "driver_test.py", "exec"))
EOF
then ok "psycopg driver"; else bad "psycopg driver"; cat "$WORK/driver.out"; fi

# --- PostgreSQL 18.4 pg_dump plain-format restore --------------------------
echo "=== PostgreSQL 18.4 plain dump restore ==="
# PostgreSQL 18 added \restrict guards to plain dumps. Ubuntu's client package
# can lag the server image, so remove only those two client-side guard lines;
# every SQL and COPY byte produced by pg_dump is fed through unchanged.
sed -e '/^\\restrict /d' -e '/^\\unrestrict /d' \
  tests/data/postgresql-18.4-plain-dump.sql |
  psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -X \
    -v ON_ERROR_STOP=1 > "$WORK/pg_dump_restore.out" 2>&1
restore_status=$?
if [[ $restore_status -ne 0 ]]; then
  bad "PostgreSQL 18.4 plain dump restores"; tail -40 "$WORK/pg_dump_restore.out"
else
  # Restart before observing the result: table definitions, owned identity
  # sequences, setval positions, views, matviews, indexes and data must all
  # come back through WAL replay rather than surviving only in memory.
  kill "$P3_PID" 2>/dev/null
  wait "$P3_PID" 2>/dev/null
  "${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
  P3_PID=$!
  wait_up "$P3_PORT"
  dump_observed=$(psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -X -At -F '|' \
    -v ON_ERROR_STOP=1 -c "
      SELECT count(*) FROM app.entry;
      SELECT state,n FROM app.entry_counts ORDER BY state;
      INSERT INTO app.entry(parent_id,state,note,payload,tags,amount)
        VALUES (1,'sad','third','{\"c\":3}',ARRAY['x','y'],3)
        RETURNING id,doubled;
      SELECT nextval('app.ticket_seq');
    " 2>/dev/null)
  expected_dump_observed=$'2\nsad|1\nhappy|1\n3|6\nINSERT 0 1\n30'
  if [[ "$dump_observed" == "$expected_dump_observed" ]]; then
    ok "PostgreSQL 18.4 plain dump restores and survives restart"
  else
    bad "PostgreSQL 18.4 plain dump restore result"
    printf 'expected:\n%s\nobserved:\n%s\n' "$expected_dump_observed" "$dump_observed"
  fi
fi
# The differential corpora assume an empty pos3ql catalog.
restart_p3_fresh || exit 1

# --- curated differential SQL corpus (rows + SQLSTATEs must match) ----------
echo "=== differential SQL corpus (real PostgreSQL vs pos3ql) ==="
normalize() {
  sed -E \
    -e 's/^psql:[^:]*:[0-9]+: ERROR:  ([0-9A-Z]{5}):.*/ERROR \1/' \
    -e 's/^ERROR:  ([0-9A-Z]{5}):.*/ERROR \1/' \
    -e '/^LINE [0-9]+:/d' -e '/^ *\^ *$/d' \
    -e '/^(HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|DATATYPE NAME|NOTICE|WARNING):/d'
}
run_corpus() { # host port outfile file
  psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -X -a -q -P pager=off -v VERBOSITY=verbose -f "$4" 2>&1 | normalize > "$3"
}
for f in "$EXT"/differential/*.sql; do
  n=$(basename "$f" .sql)
  run_corpus "$PGHOST" "$PGPORT" "$WORK/$n.pg" "$f"
  run_corpus 127.0.0.1 "$P3_PORT" "$WORK/$n.p3" "$f"
  if diff -u "$WORK/$n.pg" "$WORK/$n.p3" > "$WORK/$n.diff"; then ok "corpus: $n"
  else bad "corpus: $n"; head -40 "$WORK/$n.diff"; fi
done

# --- exact-error corpora (message wording must match) -----------------------
echo "=== exact-error corpora (message wording must match) ==="
normalize_exact() {
  sed -E \
    -e 's/^psql:[^:]*:[0-9]+: ERROR:  ([0-9A-Z]{5}): *(.*)/ERROR \1 \2/' \
    -e 's/^ERROR:  ([0-9A-Z]{5}): *(.*)/ERROR \1 \2/' \
    -e '/^LINE [0-9]+:/d' -e '/^ *\^ *$/d' \
    -e '/^(HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|DATATYPE NAME|NOTICE|WARNING):/d'
}
run_exact() { # host port outfile file
  psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -X -a -q -P pager=off -v VERBOSITY=verbose -f "$4" 2>&1 | normalize_exact > "$3"
}
for f in "$EXT"/differential_exact/*.sql; do
  n=$(basename "$f" .sql)
  run_exact "$PGHOST" "$PGPORT" "$WORK/$n.pg" "$f"
  run_exact 127.0.0.1 "$P3_PORT" "$WORK/$n.p3" "$f"
  if diff -u "$WORK/$n.pg" "$WORK/$n.p3" > "$WORK/$n.diff"; then ok "exact errors: $n"
  else bad "exact errors: $n"; head -40 "$WORK/$n.diff"; fi
done

# --- COPY binary round-trip (binary data cannot be fed through a psql corpus) -
echo "=== COPY binary round-trip (real PostgreSQL vs pos3ql) ==="
if "$PY" "$EXT/copy_binary_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" > "$WORK/copybin.out" 2>&1; then
  ok "COPY binary round-trip ($(tail -1 "$WORK/copybin.out"))"
else
  bad "COPY binary round-trip"; cat "$WORK/copybin.out"
fi

# --- LISTEN / NOTIFY (cross-connection; needs two live connections per engine) -
echo "=== LISTEN / NOTIFY (real PostgreSQL vs pos3ql) ==="
if "$PY" "$EXT/listen_notify_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" > "$WORK/listen.out" 2>&1; then
  ok "LISTEN / NOTIFY ($(grep '^ok:' "$WORK/listen.out" | tail -1))"
else
  bad "LISTEN / NOTIFY"; cat "$WORK/listen.out"
fi

# --- extended-protocol binary parameters (composite types over the wire) ------
echo "=== binary parameters (real PostgreSQL vs pos3ql) ==="
if "$PY" "$EXT/binary_param_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" > "$WORK/binparam.out" 2>&1; then
  ok "binary parameters ($(tail -1 "$WORK/binparam.out"))"
else
  bad "binary parameters"; cat "$WORK/binparam.out"
fi

# --- vendored sqllogictest replay (real PostgreSQL is the oracle) ----------
# Query-block sharded: all files, all statements; this shard runs its slice of
# the read-only query blocks.
if [[ "$RUN_SLT" == 1 ]]; then
  # The curated corpora intentionally leave some objects alive. Give the
  # sqllogictest files the full bounded catalog: one file creates 64 primary-key
  # tables, exactly matching both configured pools.
  echo "=== restart pos3ql (fresh table space for sqllogictest) ==="
  restart_p3_fresh || exit 1
  echo "=== sqllogictest replay (query shard $SLT_QUERY_SHARD/$SLT_QUERY_SHARDS) ==="
  if "$PY" "$EXT/slt_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" --limit "$SLT_LIMIT" \
       --query-shards "$SLT_QUERY_SHARDS" --query-shard "$SLT_QUERY_SHARD" \
       vendor/test/sqllogictest/test/*.test vendor/test/sqllogictest/test/evidence/*.test \
       > "$WORK/slt.out" 2>&1; then
    ok "sqllogictest replay ($(grep '^TOTAL' "$WORK/slt.out"))"
  else bad "sqllogictest replay"; tail -40 "$WORK/slt.out"; fi
fi

if [[ "$RUN_FUZZ" == 1 ]]; then
  # The corpus replay fills pos3ql's bounded table catalog to its limit, so give
  # the generative fuzzer its own fresh instance (a clean table space) rather
  # than letting its schema setup fail against a full catalog.
  echo "=== restart pos3ql (fresh table space for the fuzzer) ==="
  restart_p3_fresh || exit 1

  # --- generative differential fuzzer (gated by a ratchet budget) ----------
  echo "=== generative fuzzer (count=$FUZZ_COUNT seed=$FUZZ_SEED, budget=$FUZZ_BUDGET) ==="
  "$PY" "$EXT/fuzz_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" --count "$FUZZ_COUNT" --seed "$FUZZ_SEED" \
    > "$WORK/fuzz.out" 2>&1 || true
  DIV=$(grep -oE 'divergence=[0-9]+' "$WORK/fuzz.out" | tail -1 | cut -d= -f2)
  DIV=${DIV:-unknown}
  grep '^TOTAL' "$WORK/fuzz.out"
  if [[ ! "$DIV" =~ ^[0-9]+$ ]]; then
    # No divergence count means the fuzzer crashed before finishing — show why.
    bad "fuzzer produced no divergence count (crashed)"; tail -40 "$WORK/fuzz.out"
  elif (( DIV <= FUZZ_BUDGET )); then
    ok "fuzzer within budget ($DIV <= $FUZZ_BUDGET)"
  else
    bad "fuzzer over budget ($DIV > $FUZZ_BUDGET)"; grep -A3 DIVERGENCE "$WORK/fuzz.out" | head -60
  fi
fi

echo ""
echo "passed: $PASS  failed: $FAIL"
[[ $FAIL -eq 0 ]]
