#!/bin/zsh
# Differential conformance: run the same SQL corpus against REAL
# PostgreSQL 18 and against pos3ql, normalize, and diff.
#
# This is a generic validator for PostgreSQL implementations: rows, tags,
# and column headers must match exactly; errors must match by SQLSTATE
# (message wording is normalized away). Any diff is a semantic divergence.
#
# Usage: tests/external/differential.sh [--keep]

set -u
cd "$(dirname "$0")/../.."
EXT=tests/external
ROOT_VENV=${POS3QL_VENV:-target/external-venv}
WORK=$(mktemp -d /tmp/pos3ql-diff.XXXXXX)
KEEP=${1:-}

PGBIN=${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}
PSQL="$PGBIN/psql"
PG_PORT=${POS3QL_DIFF_PG_PORT:-15498}
P3_PORT=${POS3QL_DIFF_P3_PORT:-15499}

PASS=0
FAIL=0
ok()  { PASS=$((PASS+1)); print -- "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); print -- "FAIL: $1"; }

cleanup() {
  [[ -n "${P3_PID:-}" ]] && kill "$P3_PID" 2>/dev/null
  [[ -d "$WORK/pgdata" ]] && "$PGBIN/pg_ctl" -D "$WORK/pgdata" stop -m immediate >/dev/null 2>&1
  [[ -n "${SOCKDIR:-}" ]] && rm -rf "$SOCKDIR"
  if [[ "$KEEP" == "--keep" ]]; then
    print -- "work dir kept: $WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

print -- "=== reference: $("$PGBIN/postgres" --version) ==="

# Real PostgreSQL, hermetic cluster.
"$PGBIN/initdb" -D "$WORK/pgdata" -U postgres -A trust --encoding=UTF8 --lc-collate=C --lc-ctype=C >/dev/null 2>&1 || { bad initdb; exit 1; }
SOCKDIR=$(mktemp -d /tmp/pos3ql-pgsock.XXXX)
"$PGBIN/pg_ctl" -D "$WORK/pgdata" -o "-p $PG_PORT -k $SOCKDIR -c listen_addresses=127.0.0.1 -c timezone=UTC" \
  -l "$WORK/pg.log" start >/dev/null || { bad "pg start"; exit 1; }

# pos3ql (object storage off by default: this suite is pure SQL semantics).
# POS3QL_EXTRA_CONF appends config lines — the forced-spill mode runs the
# same corpora with a deliberately tiny memtable over a real bucket, so
# every query also exercises the spill/checkpoint/read-back path.
cargo build --release -q || { bad build; exit 1; }
cat > "$WORK/p3.conf" <<EOF
listen_addr = 127.0.0.1:${P3_PORT}
data_dir = ${WORK}/p3data
s3 = ${POS3QL_DIFF_S3:-off}
max_tables = 64
table_rows = 65536
memtable_bytes = ${POS3QL_DIFF_MEMTABLE:-256MiB}
${POS3QL_EXTRA_CONF:-}
EOF
# A leftover server on our port would silently answer the readiness probe
# below and the whole run would test a stale binary. Refuse to start.
if nc -z 127.0.0.1 $P3_PORT 2>/dev/null; then
  bad "port $P3_PORT is already in use (stale pos3ql from an earlier run?) — kill it first"
  exit 1
fi

"${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
P3_PID=$!

for i in {1..50}; do
  "$PSQL" -h 127.0.0.1 -p $PG_PORT -U postgres -X -q -c "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
for i in {1..50}; do
  "$PSQL" -h 127.0.0.1 -p $P3_PORT -U postgres -X -q -c "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
# The probe succeeding proves *a* server answered — make sure it is ours.
if ! kill -0 "$P3_PID" 2>/dev/null; then
  bad "pos3ql under test exited at startup (see $WORK/p3.log)"
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
    -e '/^HINT:/d' \
    -e '/^DETAIL:/d' \
    -e '/^LOCATION:/d' \
    -e '/^CONTEXT:/d' \
    -e '/^SCHEMA NAME:/d' \
    -e '/^TABLE NAME:/d' \
    -e '/^COLUMN NAME:/d' \
    -e '/^CONSTRAINT NAME:/d' \
    -e '/^NOTICE:/d' \
    -e '/^WARNING:/d'
}

run_corpus() { # port name file
  "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -X -a -q -P pager=off \
    -v VERBOSITY=verbose -f "$3" 2>&1 | normalize > "$WORK/$2"
}

print -- "=== corpus diffs (real PostgreSQL vs pos3ql) ==="
for f in $EXT/differential/*.sql; do
  name=$(basename "$f" .sql)
  run_corpus $PG_PORT "$name.pg" "$f"
  run_corpus $P3_PORT "$name.p3" "$f"
  if diff -u "$WORK/$name.pg" "$WORK/$name.p3" > "$WORK/$name.diff"; then
    ok "differential: $name"
  else
    bad "differential: $name"
    head -30 "$WORK/$name.diff"
  fi
done

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
    -e '/^(HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|NOTICE|WARNING):/d'
}

run_exact() { # port name file
  "$PSQL" -h 127.0.0.1 -p "$1" -U postgres -X -a -q -P pager=off \
    -v VERBOSITY=verbose -f "$3" 2>&1 | normalize_exact > "$WORK/$2"
}

print -- "\n=== exact-error corpora (message wording must match) ==="
for f in $EXT/differential_exact/*.sql; do
  name=$(basename "$f" .sql)
  run_exact $PG_PORT "$name.pg" "$f"
  run_exact $P3_PORT "$name.p3" "$f"
  if diff -u "$WORK/$name.pg" "$WORK/$name.p3" > "$WORK/$name.diff"; then
    ok "exact errors: $name"
  else
    bad "exact errors: $name"
    head -30 "$WORK/$name.diff"
  fi
done

print -- "\n=== vendored sqllogictest replay (real PostgreSQL is the oracle) ==="
SLT_VENV=${POS3QL_VENV:-$ROOT_VENV}
if [[ -x "$SLT_VENV/bin/python" ]] && [[ -d vendor/test/sqllogictest/test ]]; then
  SLT_LIMIT=${POS3QL_SLT_LIMIT:-600}
  if "$SLT_VENV/bin/python" "$EXT/slt_diff.py" --pg $PG_PORT --p3 $P3_PORT \
       --limit "$SLT_LIMIT" \
       vendor/test/sqllogictest/test/*.test vendor/test/sqllogictest/test/evidence/*.test \
       > "$WORK/slt.out" 2>&1; then
    ok "sqllogictest differential ($(grep '^TOTAL' "$WORK/slt.out"))"
  else
    bad "sqllogictest differential"
    tail -30 "$WORK/slt.out"
  fi
else
  print -- "SKIP: sqllogictest replay (need a psycopg venv at \$POS3QL_VENV and vendor/)"
fi

print -- "\npassed: $PASS  failed: $FAIL"
[[ $FAIL -eq 0 ]]
