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
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
DRV=tests/external/drivers
WORK=$(mktemp -d "${TMPDIR:-/tmp}/pos3ql-drivers.XXXXXX")

PGHOST=${PGHOST:-127.0.0.1}
PGPORT=${PGPORT:-5432}
PGUSER=${PGUSER:-postgres}
export PGHOST PGPORT PGUSER
P3_PORT=${P3_PORT:-15599}

PASS=0 FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }
cleanup() { [[ -n "${P3_PID:-}" ]] && kill "$P3_PID" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

# --- start pos3ql (pure SQL/protocol; object storage off) ------------------
cargo build --release -q || { echo "build failed"; exit 1; }
cat > "$WORK/p3.conf" <<EOF
listen_addr = 127.0.0.1:${P3_PORT}
data_dir = ${WORK}/p3data
s3 = off
max_tables = 64
table_rows = 65536
memtable_bytes = 256MiB
EOF
./target/release/pos3ql --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
P3_PID=$!
for _ in $(seq 1 100); do
  psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1 && break
  sleep 0.1
done
echo "reference: $(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc 'SHOW server_version')"

# Run a driver transcript against a host:port, writing to $1.
# $2… is the command to run (it receives host port appended).
run() { local outfile=$1; shift; local host=$1 port=$2; shift 2; "$@" "$host" "$port" > "$outfile" 2>&1; }

diff_driver() { # name  build-and-run-fn
  local name=$1 fn=$2
  $fn "$WORK/$name.pg" "$PGHOST" "$PGPORT"
  $fn "$WORK/$name.p3" 127.0.0.1 "$P3_PORT"
  if diff -u "$WORK/$name.pg" "$WORK/$name.p3" > "$WORK/$name.diff"; then
    ok "$name ($(wc -l < "$WORK/$name.pg" | tr -d ' ') lines match)"
  else
    bad "$name"; cat "$WORK/$name.diff"
  fi
}

# --- pgJDBC (Java) ---------------------------------------------------------
JDBC_VER=42.7.4
JAR="$WORK/postgresql-$JDBC_VER.jar"
if command -v javac >/dev/null; then
  curl -sSL -o "$JAR" "https://repo1.maven.org/maven2/org/postgresql/postgresql/$JDBC_VER/postgresql-$JDBC_VER.jar"
  javac -d "$WORK" "$DRV/JdbcTest.java"
  jdbc() { java -cp "$WORK:$JAR" JdbcTest "$2" "$3" > "$1" 2>&1; }
  diff_driver jdbc jdbc
else
  echo "SKIP: jdbc (no javac)"
fi

# --- jackc/pgx (Go) --------------------------------------------------------
if command -v go >/dev/null; then
  GD="$WORK/gopgx"; mkdir -p "$GD"; cp "$DRV/pgx_driver.go" "$GD/main.go"
  ( cd "$GD" && go mod init pgxdrv >/dev/null 2>&1 && go get github.com/jackc/pgx/v5@latest >/dev/null 2>&1 )
  pgx() { ( cd "$GD" && go run main.go "$2" "$3" ) > "$1" 2>&1; }
  diff_driver pgx pgx
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
