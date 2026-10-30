#!/usr/bin/env bash
# Differential driver conformance: run real PostgreSQL client drivers (pgJDBC,
# jackc/pgx, node-postgres) against a real PostgreSQL AND against pos3ql, and
# diff each driver's transcript. Every driver exercises the wire protocol its
# own way — extended-protocol binds, binary parameter/result formats, and the
# DatabaseMetaData / catalog introspection ORMs and tools issue — so a mismatch
# in any of those shows up as a diff.
#
# Drivers are fetched at CI time (never committed), matching the psycopg
# differential job; only the small test programs in drivers/ are in-tree.
#
# Env: PGHOST/PGPORT/PGUSER (reference PostgreSQL), P3_PORT (pos3ql listen port).

set -u
cd "$(dirname "$0")/../.." || exit
DRV=tests/external/drivers
WORK=$(mktemp -d "${TMPDIR:-/tmp}/pos3ql-drivers.XXXXXX")

PGHOST=${PGHOST:-127.0.0.1}
PGPORT=${PGPORT:-5432}
PGUSER=${PGUSER:-postgres}
export PGHOST PGPORT PGUSER
P3_PORT=${P3_PORT:-15599}
: "${POS3QL_VENV:?set the pinned Python test environment}"
: "${POS3QL_DRIVER_EVIDENCE:?set the client evidence directory}"

PASS=0 FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
cleanup() {
  [[ -n "${P3_PID:-}" ]] && kill "$P3_PID" 2>/dev/null
  mkdir -p "$POS3QL_DRIVER_EVIDENCE"
  find "$WORK" -maxdepth 1 -type f -exec cp {} "$POS3QL_DRIVER_EVIDENCE/" \;
  rm -rf "$WORK"
}
trap cleanup EXIT

# --- start pos3ql (pure SQL/protocol; object storage off) ------------------
cargo build --release -q || { echo "build failed"; exit 1; }
cat > "$WORK/p3.conf" <<EOF
transaction_mode = mvcc
catalog_history_bytes = 16MiB
row_history_slots = 65536
listen_addr = 127.0.0.1:${P3_PORT}
data_dir = ${WORK}/p3data
object_store = off
max_tables = 64
table_rows = 65536
max_value_indexes = 64
memtable_bytes = 256MiB
max_prepared = 64
max_prepared_transactions = 8
EOF
"${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
P3_PID=$!
for _ in $(seq 1 100); do
  psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
echo "reference: $(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc 'SHOW server_version')"
"$POS3QL_VENV/bin/python" -m pip freeze > "$WORK/python-packages.txt" || exit 1
git rev-parse HEAD > "$WORK/revision.txt" || exit 1

# Run a driver transcript against a host:port, writing to $1.
# $2… is the command to run (it receives host port appended).
run() { local outfile=$1; shift; local host=$1 port=$2; shift 2; "$@" "$host" "$port" > "$outfile" 2>&1; }

diff_driver() { # name  build-and-run-fn
  local name=$1 fn=$2
  if ! $fn "$WORK/$name.pg" "$PGHOST" "$PGPORT"; then
    bad "$name (PostgreSQL driver execution failed)"
    cat "$WORK/$name.pg"
    return
  fi
  if ! $fn "$WORK/$name.p3" 127.0.0.1 "$P3_PORT"; then
    bad "$name (pos3ql driver execution failed)"
    cat "$WORK/$name.p3"
    return
  fi
  if diff -u "$WORK/$name.pg" "$WORK/$name.p3" > "$WORK/$name.diff"; then
    ok "$name ($(wc -l < "$WORK/$name.pg" | tr -d ' ') lines match)"
  else
    bad "$name"; cat "$WORK/$name.diff"
  fi
}

# The same raw protocol assertions run against both servers. Snapshot names
# and positions differ; visible rows and SQLSTATEs must agree.
replication_snapshot_probe() {
  POS3QL_HOST=$2 POS3QL_PORT=$3 \
    POS3QL_WIRE_PROBE=test_replication_snapshot_lifetime \
    python3 tests/external/wire_probe.py > "$1" 2>&1
}
diff_driver replication-snapshot replication_snapshot_probe

partition_move_probe() {
  POS3QL_HOST=$2 POS3QL_PORT=$3 \
    POS3QL_WIRE_PROBE=test_partition_move_deferred_constraints \
    python3 tests/external/wire_probe.py > "$1" 2>&1
}
diff_driver partition-move partition_move_probe

# Exercise real Python clients and migration tools; do not substitute raw SQL
# for their generated queries or normalize failed application operations.
for suite in psycopg2 sqlalchemy alembic; do
  python_client() {
    "$POS3QL_VENV/bin/python" "$DRV/python_clients.py" --suite "$suite" \
      --host "$2" --port "$3" --user "$PGUSER" --database postgres > "$1" 2>&1
  }
  diff_driver "$suite" python_client
done

# Retain the complete live catalog comparison separately from executable
# client tests. Missing entries remain visible; this is not a support claim.
if ! "$POS3QL_VENV/bin/python" tests/external/builtin_inventory.py \
    --reference-host "$PGHOST" --reference-port "$PGPORT" \
    --candidate-host 127.0.0.1 --candidate-port "$P3_PORT" \
    --user "$PGUSER" --database postgres --revision "$(git rev-parse HEAD)" \
    --output "$WORK/builtin-inventory.json"; then
  bad "built-in catalog inventory capture"
fi

# --- pgJDBC (Java) ---------------------------------------------------------
JDBC_VER=42.7.4
JDBC_SHA256=188976721ead8e8627eb6d8389d500dccc0c9bebd885268a3047180274a6031e
JDBC_URL="https://repo.maven.apache.org/maven2/org/postgresql/postgresql/$JDBC_VER/postgresql-$JDBC_VER.jar"
JAR="$DRV/.artifacts/postgresql-$JDBC_VER.jar"
verify_jdbc() {
  printf '%s  %s\n' "$JDBC_SHA256" "$1" | shasum -a 256 --check --status \
    && jar tf "$1" >/dev/null
}
if command -v javac >/dev/null; then
  # Publish only verified bytes to the CI cache; every restored artifact is
  # checked again. Failed responses never become Java classpath entries.
  if [[ ! -e "$JAR" ]]; then
    if curl --fail --silent --show-error --location --retry 2 --retry-all-errors \
        --connect-timeout 10 --max-time 30 -o "$WORK/jdbc.jar" "$JDBC_URL" \
        && verify_jdbc "$WORK/jdbc.jar"; then
      mkdir -p "$DRV/.artifacts" && mv "$WORK/jdbc.jar" "$JAR"
    fi
  fi
  if [[ -f "$JAR" ]] && verify_jdbc "$JAR"; then
    if javac -cp "$JAR" -d "$WORK" "$DRV/JdbcTest.java"; then
      jdbc() { java -cp "$WORK:$JAR" JdbcTest "$2" "$3" > "$1" 2>&1; }
      diff_driver jdbc jdbc
    else
      bad "jdbc (fixture compilation failed)"
    fi
  else
    bad "jdbc (driver artifact unavailable or invalid)"
  fi
else
  echo "SKIP: jdbc (no javac)"
fi

# --- jackc/pgx (Go) --------------------------------------------------------
if command -v go >/dev/null; then
  GD="$WORK/gopgx"; mkdir -p "$GD"; cp "$DRV/pgx_driver.go" "$GD/main.go"
  # Build once so Go's fetch/toolchain-download noise stays out of the runtime
  # transcript (`go run`'s first invocation would print `go: downloading …`).
  ( cd "$GD" && go mod init pgxdrv >/dev/null 2>&1 \
      && go get github.com/jackc/pgx/v5@latest >/dev/null 2>&1 \
      && go build -o pgxbin main.go >/dev/null 2>&1 )
  if [[ -x "$GD/pgxbin" ]]; then
    pgx() { "$GD/pgxbin" "$2" "$3" > "$1" 2>&1; }
    diff_driver pgx pgx
  else
    bad "pgx (build failed)"; ( cd "$GD" && go build -o pgxbin main.go ) 2>&1 | head -20
  fi
else
  echo "SKIP: pgx (no go)"
fi

# --- node-postgres (Node) --------------------------------------------------
if command -v node >/dev/null; then
  ND="$WORK/node"; mkdir -p "$ND"; cp "$DRV/node_test.js" "$ND/"
  ( cd "$ND" && npm init -y >/dev/null 2>&1 && npm install pg@8 >/dev/null 2>&1 )
  nodedrv() { ( cd "$ND" && node node_test.js "$2" "$3" ) > "$1" 2>&1; }
  diff_driver node nodedrv
else
  echo "SKIP: node (no node)"
fi

echo ""
echo "passed: $PASS  failed: $FAIL"
[[ $FAIL -eq 0 ]]
