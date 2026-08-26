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
#   SLT_UNSUPPORTED_BUDGET     maximum unsupported blocks per shard (ratchet)
#   FUZZ_COUNT / FUZZ_SEED     generative fuzz statements / seed (default 20000 / 1)
#   FUZZ_BUDGET                allowed fuzz divergences before failing (ratchet; default 0)
#   FUZZ_UNSUPPORTED_BUDGET    allowed unsupported generated statements (default 0)
#   RUN_FAST / RUN_SLT / RUN_FUZZ
#                              select deterministic, sqllogictest, and fuzz phases
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
SLT_UNSUPPORTED_BUDGET=${SLT_UNSUPPORTED_BUDGET:-0}
FUZZ_COUNT=${FUZZ_COUNT:-20000}
FUZZ_SEED=${FUZZ_SEED:-1}
FUZZ_BUDGET=${FUZZ_BUDGET:-0}
FUZZ_UNSUPPORTED_BUDGET=${FUZZ_UNSUPPORTED_BUDGET:-0}
# Sharding, so each CI job fits a wall-clock cap while total coverage is
# preserved: the sqllogictest replay splits each file's query blocks across
# SLT_QUERY_SHARDS shards (this run does shard SLT_QUERY_SHARD, 0-based) — every
# shard runs all files and all statement/DDL blocks, only the read-only query
# blocks are divided, which balances even a single huge file. RUN_FAST /
# RUN_SLT / RUN_FUZZ gate the deterministic and two slow phases independently.
# Defaults run everything in one shard, matching the unsharded behavior.
SLT_QUERY_SHARD=${SLT_QUERY_SHARD:-0}
SLT_QUERY_SHARDS=${SLT_QUERY_SHARDS:-1}
RUN_FAST=${RUN_FAST:-1}
RUN_SLT=${RUN_SLT:-1}
RUN_FUZZ=${RUN_FUZZ:-1}
EXTENSION_CONTROL_ROOT=${POS3QL_EXTENSION_CONTROL_PATH:-$PWD/$EXT/extensions}
REFERENCE_EXTENSION_CONTROL_ROOT=${POS3QL_REFERENCE_EXTENSION_CONTROL_PATH:-$PWD/$EXT/extensions}

PASS=0 FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; }

. "$EXT/liveness.sh"

cleanup() {
  local status=$?
  [[ -n "${P3_PID:-}" ]] && kill "$P3_PID" 2>/dev/null
  rm -rf "$WORK"
  trap - EXIT
  exit "$status"
}
trap cleanup EXIT

# --- psycopg venv (the differential/fuzz scripts need it) -------------------
if [[ ! -x "$VENV/bin/python" ]]; then
  python3 -m venv "$VENV" && "$VENV/bin/pip" install --quiet 'psycopg[binary]'
fi
PY="$VENV/bin/python"

if "$PY" "$EXT/result_diff.py" >/dev/null; then
  ok "differential result comparator"
else
  bad "differential result comparator"
  exit 1
fi

# This suite owns its PostgreSQL service. Clear cluster-wide roles and user
# objects so a root-cause rerun observes the same oracle as the first run.
# Keep the bootstrap public schema: catalog-reference binary values contain
# its OID, so replacing the schema would manufacture a cross-cluster mismatch.
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -X \
  -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
SET client_min_messages = warning;
DO $$
DECLARE
  object record;
  routine_identity text;
  routine_identities text[];
BEGIN
  FOR object IN
    SELECT c.relkind, c.relname
      FROM pg_class c
     WHERE c.relnamespace = 'public'::regnamespace
       AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
  LOOP
    EXECUTE format(
      'DROP %s IF EXISTS public.%I CASCADE',
      CASE object.relkind
        WHEN 'v' THEN 'VIEW'
        WHEN 'm' THEN 'MATERIALIZED VIEW'
        WHEN 'S' THEN 'SEQUENCE'
        WHEN 'f' THEN 'FOREIGN TABLE'
        ELSE 'TABLE'
      END,
      object.relname
    );
  END LOOP;
  SELECT array_agg(p.oid::regprocedure::text ORDER BY p.oid)
    INTO routine_identities
    FROM pg_proc p
   WHERE p.pronamespace = 'public'::regnamespace;
  FOREACH routine_identity IN ARRAY coalesce(routine_identities, ARRAY[]::text[])
  LOOP
    EXECUTE format('DROP ROUTINE IF EXISTS %s CASCADE', routine_identity);
  END LOOP;
  FOR object IN
    SELECT t.typname, t.typtype
      FROM pg_type t
      LEFT JOIN pg_class c ON c.oid = t.typrelid
     WHERE t.typnamespace = 'public'::regnamespace
       AND t.typelem = 0
       AND (t.typrelid = 0 OR c.relkind = 'c')
  LOOP
    EXECUTE format(
      'DROP %s IF EXISTS public.%I CASCADE',
      CASE object.typtype WHEN 'd' THEN 'DOMAIN' ELSE 'TYPE' END,
      object.typname
    );
  END LOOP;
END
$$;
DO $$
DECLARE schema_name text;
BEGIN
  FOR schema_name IN
    SELECT nspname FROM pg_namespace
     WHERE nspname <> 'public'
       AND nspname <> 'information_schema'
       AND nspname !~ '^pg_'
  LOOP
    EXECUTE format('DROP SCHEMA %I CASCADE', schema_name);
  END LOOP;
END
$$;
DO $$
DECLARE role_name text;
BEGIN
  FOR role_name IN
    SELECT rolname FROM pg_roles
     WHERE rolname <> current_user AND rolname !~ '^pg_'
  LOOP
    EXECUTE format('DROP OWNED BY %I CASCADE', role_name);
    EXECUTE format('DROP ROLE %I', role_name);
  END LOOP;
END
$$;
SQL

# --- start pos3ql (object storage off: this suite is pure SQL semantics) ----
cargo build --release -q || { echo "build failed"; exit 1; }
cat > "$WORK/p3.conf" <<EOF
listen_addr = 127.0.0.1:${P3_PORT}
data_dir = ${WORK}/p3data
object_store = off
max_tables = 64
table_rows = 8192
max_value_indexes = 64
memtable_bytes = 256MiB
extension_control_path = ${EXTENSION_CONTROL_ROOT}
EOF
"${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
P3_PID=$!

wait_up() { # host port
  for _ in $(seq 1 100); do
    psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}
wait_p3() {
  for _ in $(seq 1 100); do
    if ! server_alive "$P3_PID"; then
      echo "pos3ql process $P3_PID exited during startup"
      tail -40 "$WORK/p3.log"
      return 1
    fi
    psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" \
      >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}
wait_up "$PGHOST" "$PGPORT" || { echo "PostgreSQL not reachable on $PGHOST:$PGPORT"; exit 1; }
wait_p3 || { echo "pos3ql not reachable on 127.0.0.1:$P3_PORT"; exit 1; }
echo "reference: $(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc 'SHOW server_version')"

restart_p3() {
  kill "$P3_PID" 2>/dev/null
  wait "$P3_PID" 2>/dev/null
  "${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
  P3_PID=$!
  wait_p3 && return 0
  echo "pos3ql did not restart on 127.0.0.1:$P3_PORT"
  return 1
}

restart_p3_fresh() {
  kill "$P3_PID" 2>/dev/null
  wait "$P3_PID" 2>/dev/null
  rm -rf "${WORK}/p3data"
  "${POS3QL_BIN:-./target/release/pos3ql}" --config "$WORK/p3.conf" > "$WORK/p3.log" 2>&1 &
  P3_PID=$!
  wait_p3 && return 0
  echo "pos3ql did not restart on 127.0.0.1:$P3_PORT"
  return 1
}

if [[ "$RUN_FAST" == 1 ]]; then
# --- raw wire-protocol probes ----------------------------------------------
echo "=== wire protocol probes ==="
if POS3QL_PORT=$P3_PORT POS3QL_EXTENSION_WIRE=1 \
  python3 "$EXT/wire_probe.py" > "$WORK/wire.out" 2>&1; then
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
restart_p3_fresh || exit 1
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
  restart_p3 || exit 1
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

# --- extension-aware pg_dump, restored by vanilla PostgreSQL 18 ------------
restart_p3_fresh || exit 1
echo "=== SQL extension pg_dump round trip ==="
psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -X \
  -v ON_ERROR_STOP=1 > "$WORK/extension_setup.out" 2>&1 <<'SQL'
CREATE SCHEMA extension_dump;
CREATE EXTENSION pos3ql_ext VERSION '1.0' SCHEMA extension_dump CASCADE;
ALTER EXTENSION pos3ql_ext UPDATE TO '2.0';
INSERT INTO extension_dump.extension_rows VALUES (1, 'extension member', true);
INSERT INTO extension_dump.extension_config VALUES
  ('user row', false),
  ('built in row', true);
SQL
extension_setup_status=$?
pg_dump -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres \
  --no-owner -f "$WORK/extension.sql" > "$WORK/extension_dump.out" 2>&1
extension_dump_status=$?
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -X \
  -v ON_ERROR_STOP=1 \
  -c 'DROP EXTENSION IF EXISTS pos3ql_ext CASCADE; DROP EXTENSION IF EXISTS pos3ql_base CASCADE; DROP SCHEMA IF EXISTS extension_dump CASCADE' \
  > "$WORK/extension_reference_clean.out" 2>&1
PGOPTIONS="-c extension_control_path=$REFERENCE_EXTENSION_CONTROL_ROOT" \
  psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -X \
    -v ON_ERROR_STOP=1 -f "$WORK/extension.sql" \
    > "$WORK/extension_restore.out" 2>&1
extension_restore_status=$?
if [[ $extension_setup_status -ne 0 || $extension_dump_status -ne 0 || $extension_restore_status -ne 0 ]]; then
  bad "SQL extension pg_dump restores into PostgreSQL 18"
  tail -40 "$WORK/extension_setup.out"
  tail -40 "$WORK/extension_dump.out"
  tail -40 "$WORK/extension_restore.out"
else
  extension_observed=$(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres \
    -X -At -F '|' -v ON_ERROR_STOP=1 -c "
      SELECT e.extname,e.extversion,n.nspname
        FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace
       WHERE e.extname IN ('pos3ql_base','pos3ql_ext') ORDER BY e.extname;
      SELECT count(*) FROM extension_dump.extension_rows;
      SELECT key,built_in FROM extension_dump.extension_config ORDER BY key;
      SELECT extension_dump.extension_identity('restored');
      SELECT value FROM extension_dump.extension_snapshot;
    " 2>/dev/null)
  expected_extension_observed=$'pos3ql_base|1.0|extension_dump\npos3ql_ext|2.0|extension_dump\n0\nuser row|f\nrestored\n42'
  if [[ "$extension_observed" == "$expected_extension_observed" ]]; then
    ok "SQL extension definitions and configuration rows survive pg_dump into PostgreSQL 18"
  else
    bad "SQL extension pg_dump round-trip result"
    printf 'expected:\n%s\nobserved:\n%s\n' \
      "$expected_extension_observed" "$extension_observed"
  fi
fi
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -X \
  -v ON_ERROR_STOP=1 \
  -c 'DROP EXTENSION IF EXISTS pos3ql_ext CASCADE; DROP EXTENSION IF EXISTS pos3ql_base CASCADE; DROP SCHEMA IF EXISTS extension_dump CASCADE' \
  > "$WORK/extension_reference_cleanup.out" 2>&1 || {
    bad "clean SQL extension pg_dump fixture from PostgreSQL"
    tail -40 "$WORK/extension_reference_cleanup.out"
  }

# --- outbound pg_dump, restored by vanilla PostgreSQL 18 -------------------
# The inbound and outbound fixtures are independent durability boundaries.
# Reclaim the first fixture before constructing the second so the configured
# table bound measures each supported workload rather than test accumulation.
restart_p3_fresh || exit 1
echo "=== pos3ql outbound pg_dump round trip ==="
psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -X \
  -v ON_ERROR_STOP=1 > "$WORK/outbound_setup.out" 2>&1 <<'SQL'
CREATE SCHEMA outbound_dump;
CREATE ROLE outbound_reader;
CREATE TYPE outbound_dump.mood AS ENUM ('ok', 'great');
CREATE TYPE outbound_dump.location AS (x integer, y integer);
CREATE TYPE outbound_dump.metadata AS (code varchar(3) COLLATE "C");
CREATE DOMAIN outbound_dump.location_domain AS outbound_dump.location CHECK ((VALUE).x > 0);
CREATE TABLE outbound_dump.items (
  id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  mood outbound_dump.mood NOT NULL,
  location outbound_dump.location NOT NULL,
  moods outbound_dump.mood[] NOT NULL,
  locations outbound_dump.location[] NOT NULL,
  marked_location outbound_dump.location_domain NOT NULL,
  marked_locations outbound_dump.location_domain[] NOT NULL,
  note text DEFAULT 'hello',
  CONSTRAINT outbound_items_note_check CHECK (char_length(note) > 0)
);
INSERT INTO outbound_dump.items(mood,location,moods,locations,marked_location,marked_locations,note) VALUES
  ('ok', ROW(1,2)::outbound_dump.location, ARRAY['ok'::outbound_dump.mood],
   ARRAY[ROW(7,8)::outbound_dump.location], ROW(10,20)::outbound_dump.location_domain,
   ARRAY[ROW(100,200)::outbound_dump.location_domain], 'one'),
  ('great', ROW(3,4)::outbound_dump.location, ARRAY['great'::outbound_dump.mood],
   ARRAY[ROW(9,10)::outbound_dump.location], ROW(30,40)::outbound_dump.location_domain,
   ARRAY[ROW(300,400)::outbound_dump.location_domain], 'two');
ALTER TYPE outbound_dump.location ADD ATTRIBUTE z integer;
ALTER TYPE outbound_dump.location RENAME ATTRIBUTE x TO east;
CREATE SCHEMA outbound_type_target;
ALTER TYPE outbound_dump.mood SET SCHEMA outbound_type_target;
ALTER TYPE outbound_dump.location SET SCHEMA outbound_type_target;
CREATE INDEX outbound_items_note_idx ON outbound_dump.items (note DESC)
  INCLUDE (mood) WHERE note IS NOT NULL;
CREATE TABLE outbound_dump."Odd Table" ("Key Value" integer PRIMARY KEY, "select" text);
CREATE INDEX "Odd Index" ON outbound_dump."Odd Table" ("select" DESC);
COMMENT ON TABLE outbound_dump.items IS 'dumped table comment';
COMMENT ON COLUMN outbound_dump.items.note IS 'dumped column comment';
CREATE VIEW outbound_dump.item_view AS
  SELECT id,mood,location,moods,locations,marked_location,marked_locations,note FROM outbound_dump.items;
CREATE TABLE outbound_dump.view_base (id integer PRIMARY KEY, value integer NOT NULL);
INSERT INTO outbound_dump.view_base VALUES (1, 10), (2, 20);
CREATE VIEW outbound_dump.writable_view AS
  SELECT id,value FROM outbound_dump.view_base;
CREATE FUNCTION outbound_dump.write_writable_view() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN
     IF TG_OP = ''INSERT'' THEN
       INSERT INTO outbound_dump.view_base VALUES (NEW.id, NEW.value);
       RETURN NEW;
     ELSIF TG_OP = ''UPDATE'' THEN
       UPDATE outbound_dump.view_base SET value = NEW.value WHERE id = OLD.id;
       RETURN NEW;
     END IF;
     DELETE FROM outbound_dump.view_base WHERE id = OLD.id;
     RETURN OLD;
   END';
CREATE TRIGGER writable_view_write INSTEAD OF INSERT OR UPDATE OR DELETE
  ON outbound_dump.writable_view FOR EACH ROW
  EXECUTE FUNCTION outbound_dump.write_writable_view();
CREATE TABLE outbound_dump.view_source (id integer PRIMARY KEY, value integer NOT NULL);
INSERT INTO outbound_dump.view_source VALUES (2, 200), (3, 300);
CREATE TABLE outbound_dump.tags (id integer PRIMARY KEY, label text UNIQUE NOT NULL);
INSERT INTO outbound_dump.tags VALUES (1, 'primary');
CREATE TABLE outbound_dump.item_tags (
  item_id integer NOT NULL REFERENCES outbound_dump.items(id) ON DELETE CASCADE,
  tag_id integer NOT NULL REFERENCES outbound_dump.tags(id),
  PRIMARY KEY (item_id, tag_id)
);
INSERT INTO outbound_dump.item_tags VALUES (1, 1);
CREATE TABLE outbound_dump.constraint_parent (id integer PRIMARY KEY);
INSERT INTO outbound_dump.constraint_parent VALUES (1);
CREATE TABLE outbound_dump.constraint_items (
  id integer,
  key_value integer,
  parent_id integer,
  slot int4range,
  active boolean,
  CONSTRAINT outbound_constraint_key UNIQUE (key_value)
    DEFERRABLE INITIALLY DEFERRED,
  CONSTRAINT outbound_constraint_exclusion EXCLUDE USING gist
    (slot WITH &&) WHERE (active)
    DEFERRABLE INITIALLY DEFERRED
);
INSERT INTO outbound_dump.constraint_items VALUES (-1, 1, 999, '[1,4)', false);
ALTER TABLE outbound_dump.constraint_items ADD CONSTRAINT outbound_constraint_check
  CHECK (id > 0) NOT VALID;
ALTER TABLE outbound_dump.constraint_items ADD CONSTRAINT outbound_constraint_fk
  FOREIGN KEY (parent_id) REFERENCES outbound_dump.constraint_parent(id)
  DEFERRABLE INITIALLY DEFERRED NOT VALID;
CREATE SEQUENCE outbound_dump.manual_sequence START WITH 41;
SELECT nextval('outbound_dump.manual_sequence');
CREATE MATERIALIZED VIEW outbound_dump.item_count AS SELECT count(*) AS count FROM outbound_dump.items;
CREATE FUNCTION outbound_dump.dump_answer() RETURNS integer LANGUAGE sql AS 'SELECT 42';
CREATE FUNCTION outbound_dump.dump_total_state(state bigint, value integer)
RETURNS bigint LANGUAGE sql IMMUTABLE AS 'SELECT coalesce(state, 0) + value';
CREATE AGGREGATE outbound_dump.dump_total(integer) (
  SFUNC = outbound_dump.dump_total_state, STYPE = bigint, INITCOND = '0', PARALLEL = SAFE
);
CREATE FUNCTION outbound_dump.dump_first_state(state anyelement, value anyelement)
RETURNS anyelement LANGUAGE sql IMMUTABLE AS 'SELECT coalesce(state, value)';
CREATE AGGREGATE outbound_dump.dump_first(anyelement) (
  SFUNC = outbound_dump.dump_first_state, STYPE = anyelement
);
CREATE FUNCTION outbound_dump.echo_mood(value outbound_type_target.mood) RETURNS outbound_type_target.mood LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION outbound_dump.echo_location(value outbound_type_target.location) RETURNS outbound_type_target.location LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION outbound_dump.echo_marked_location(value outbound_dump.location_domain) RETURNS outbound_dump.location_domain LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION outbound_dump.echo_moods(value outbound_type_target.mood[]) RETURNS outbound_type_target.mood[] LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION outbound_dump.echo_locations(value outbound_type_target.location[]) RETURNS outbound_type_target.location[] LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION outbound_dump.echo_marked_locations(value outbound_dump.location_domain[]) RETURNS outbound_dump.location_domain[] LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION outbound_dump.dump_rows(start_value integer)
RETURNS TABLE(item integer, label text)
LANGUAGE sql AS 'SELECT start_value, ''one'' UNION ALL SELECT start_value + 1, ''two''';
CREATE VIEW outbound_dump.row_view AS
  SELECT item,label,generated,ordinality
    FROM ROWS FROM (
      outbound_dump.dump_rows(1),
      generate_series(10,30,10)
    ) WITH ORDINALITY AS rows(item,label,generated,ordinality);
CREATE TABLE outbound_dump.protected_rows (id integer PRIMARY KEY, owner_name text);
INSERT INTO outbound_dump.protected_rows VALUES (1, 'outbound_reader'), (2, 'other');
ALTER TABLE outbound_dump.protected_rows ENABLE ROW LEVEL SECURITY;
ALTER TABLE outbound_dump.protected_rows FORCE ROW LEVEL SECURITY;
CREATE POLICY outbound_reader_rows ON outbound_dump.protected_rows
  FOR ALL TO outbound_reader
  USING (owner_name = 'outbound_reader') WITH CHECK (owner_name = 'outbound_reader');
GRANT SELECT, INSERT ON outbound_dump.protected_rows TO outbound_reader;
CREATE VIEW outbound_dump.protected_view WITH (security_invoker=true) AS
  SELECT id,owner_name FROM outbound_dump.protected_rows;
GRANT SELECT ON outbound_dump.protected_view TO outbound_reader;
CREATE TABLE outbound_dump.partition_root (id integer, region integer, PRIMARY KEY (id, region))
  PARTITION BY RANGE (id, region);
CREATE TABLE outbound_dump.partition_mid PARTITION OF outbound_dump.partition_root
  FOR VALUES FROM (0, 0) TO (100, 100) PARTITION BY LIST (region);
CREATE TABLE outbound_dump.partition_leaf PARTITION OF outbound_dump.partition_mid
  FOR VALUES IN (1);
CREATE TABLE outbound_dump.partition_other PARTITION OF outbound_dump.partition_mid DEFAULT;
INSERT INTO outbound_dump.partition_root VALUES (10, 1), (20, 2);
CREATE TABLE outbound_dump.partition_trigger_audit (id integer);
CREATE FUNCTION outbound_dump.partition_trigger_write() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO outbound_dump.partition_trigger_audit VALUES (NEW.id); RETURN NEW; END';
CREATE TRIGGER outbound_partition_after AFTER INSERT ON outbound_dump.partition_root
  FOR EACH ROW EXECUTE FUNCTION outbound_dump.partition_trigger_write();
COMMENT ON TRIGGER outbound_partition_after ON outbound_dump.partition_root
  IS 'dumped partition trigger';
CREATE TABLE outbound_dump.constraint_trigger_target (id integer PRIMARY KEY);
CREATE TABLE outbound_dump.constraint_trigger_audit (id integer);
CREATE FUNCTION outbound_dump.constraint_trigger_write() RETURNS trigger LANGUAGE plpgsql AS
  'BEGIN INSERT INTO outbound_dump.constraint_trigger_audit VALUES (NEW.id); RETURN NEW; END';
CREATE CONSTRAINT TRIGGER outbound_constraint_after AFTER INSERT
  ON outbound_dump.constraint_trigger_target DEFERRABLE INITIALLY DEFERRED
  FOR EACH ROW EXECUTE FUNCTION outbound_dump.constraint_trigger_write();
CREATE INDEX outbound_partition_region_idx ON outbound_dump.partition_root
  (id, region DESC) WITH (fillfactor=75);
CREATE STATISTICS outbound_dump.outbound_items_mood_note
  (ndistinct, dependencies, mcv) ON mood, note FROM outbound_dump.items;
ALTER STATISTICS outbound_dump.outbound_items_mood_note SET STATISTICS 25;
CREATE STATISTICS outbound_dump.outbound_items_note_lower
  ON (lower(note)) FROM outbound_dump.items;
ANALYZE outbound_dump.items;
GRANT USAGE ON SCHEMA outbound_dump TO outbound_reader;
GRANT SELECT ON TABLE outbound_dump.items TO outbound_reader;
GRANT USAGE, SELECT ON SEQUENCE outbound_dump.manual_sequence TO outbound_reader;
GRANT EXECUTE ON FUNCTION outbound_dump.dump_answer() TO outbound_reader;
SQL
outbound_setup_status=$?
pg_dump -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres \
  --schema=outbound_dump --schema=outbound_type_target --no-owner \
  -f "$WORK/outbound.sql" > "$WORK/outbound_dump.out" 2>&1
outbound_dump_status=$?
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -X \
  -v ON_ERROR_STOP=1 -c 'DROP SCHEMA IF EXISTS outbound_dump CASCADE; DROP SCHEMA IF EXISTS outbound_type_target CASCADE; DROP ROLE IF EXISTS outbound_reader; CREATE ROLE outbound_reader' \
  > "$WORK/outbound_drop.out" 2>&1
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -X \
  -v ON_ERROR_STOP=1 -f "$WORK/outbound.sql" \
  > "$WORK/outbound_restore.out" 2>&1
outbound_restore_status=$?
if [[ $outbound_setup_status -ne 0 || $outbound_dump_status -ne 0 || $outbound_restore_status -ne 0 ]]; then
  bad "pos3ql pg_dump restores into PostgreSQL 18"
  tail -40 "$WORK/outbound_setup.out"
  tail -40 "$WORK/outbound_dump.out"
  tail -40 "$WORK/outbound_restore.out"
else
  outbound_observed=$(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres \
    -X -At -F '|' -v ON_ERROR_STOP=1 -c "
      SELECT id,mood,(location).east,(location).y,(location).z IS NULL,moods[1],
             (locations[1]).y,(marked_location).east,(marked_locations[1]).y,note
        FROM outbound_dump.item_view ORDER BY id;
      INSERT INTO outbound_dump.items(mood,location,moods,locations,marked_location,marked_locations,note)
        VALUES ('ok', ROW(5,6,NULL)::outbound_type_target.location,
                ARRAY['ok'::outbound_type_target.mood],
                ARRAY[ROW(11,12,NULL)::outbound_type_target.location],
                ROW(50,60,NULL)::outbound_dump.location_domain,
                ARRAY[ROW(500,600,NULL)::outbound_dump.location_domain], 'three') RETURNING id;
      SELECT is_identity,identity_generation
        FROM information_schema.columns
       WHERE table_schema='outbound_dump'
         AND table_name='items'
         AND column_name='id';
      INSERT INTO outbound_dump.writable_view(value,id)
        SELECT value,id FROM (VALUES (30,3)) AS supplied(value,id)
        RETURNING id,value;
      UPDATE outbound_dump.writable_view SET value = 21 WHERE id = 2 RETURNING id,value;
      DELETE FROM outbound_dump.writable_view WHERE id = 1 RETURNING id,value;
      UPDATE outbound_dump.writable_view AS target SET value = source.value
        FROM outbound_dump.view_source AS source WHERE target.id = source.id;
      SELECT id,value FROM outbound_dump.view_base ORDER BY id;
      DELETE FROM outbound_dump.writable_view AS target USING outbound_dump.view_source AS source
        WHERE target.id = source.id AND source.id = 2 RETURNING target.id,target.value;
      SELECT id,value FROM outbound_dump.view_base ORDER BY id;
      SELECT conname FROM pg_constraint
       WHERE conrelid = 'outbound_dump.items'::regclass AND contype = 'c';
      SELECT indexdef LIKE '%INCLUDE (mood)%' AND indexdef LIKE '%WHERE (note IS NOT NULL)%'
        FROM pg_indexes WHERE schemaname='outbound_dump' AND indexname='outbound_items_note_idx';
      SELECT indexdef = 'CREATE INDEX \"Odd Index\" ON outbound_dump.\"Odd Table\" USING btree (\"select\" DESC)'
        FROM pg_indexes WHERE schemaname='outbound_dump' AND indexname='Odd Index';
      SELECT obj_description('outbound_dump.items'::regclass),
             col_description('outbound_dump.items'::regclass, 8);
      SELECT count FROM outbound_dump.item_count;
      SELECT nextval('outbound_dump.manual_sequence');
      SELECT count(*) FROM outbound_dump.item_tags;
      SELECT conname,contype,condeferrable,condeferred,convalidated,conenforced
        FROM pg_constraint
       WHERE conrelid = 'outbound_dump.constraint_items'::regclass
       ORDER BY conname;
      SELECT has_table_privilege('outbound_reader', 'outbound_dump.items', 'SELECT'),
             has_sequence_privilege('outbound_reader', 'outbound_dump.manual_sequence', 'USAGE'),
             has_function_privilege('outbound_reader', 'outbound_dump.dump_answer()', 'EXECUTE');
      SELECT outbound_dump.echo_mood('ok'::outbound_type_target.mood),
             (outbound_dump.echo_location(ROW(9,10,NULL)::outbound_type_target.location)).east,
             (outbound_dump.echo_marked_location(ROW(11,12,NULL)::outbound_dump.location_domain)).y,
             outbound_dump.echo_moods(ARRAY['great'::outbound_type_target.mood])::text,
             ((outbound_dump.echo_locations(ARRAY[ROW(13,14,NULL)::outbound_type_target.location]))[1]).y,
             ((outbound_dump.echo_marked_locations(ARRAY[ROW(15,16,NULL)::outbound_dump.location_domain]))[1]).east;
      SELECT item,label,generated,ordinality FROM outbound_dump.row_view ORDER BY ordinality;
      SELECT relrowsecurity,relforcerowsecurity
        FROM pg_class WHERE oid='outbound_dump.protected_rows'::regclass;
      SELECT policyname,permissive,cmd,roles,qual IS NOT NULL,with_check IS NOT NULL
        FROM pg_policies WHERE schemaname='outbound_dump' AND tablename='protected_rows';
      SELECT reloptions FROM pg_class WHERE oid='outbound_dump.protected_view'::regclass;
      SET ROLE outbound_reader;
      SELECT id,owner_name FROM outbound_dump.protected_view ORDER BY id;
      RESET ROLE;
      SELECT id,region FROM outbound_dump.partition_root ORDER BY id;
      INSERT INTO outbound_dump.partition_root VALUES (30, 1);
      SELECT id FROM outbound_dump.partition_trigger_audit;
      BEGIN;
      INSERT INTO outbound_dump.constraint_trigger_target VALUES (7);
      SELECT count(*) FROM outbound_dump.constraint_trigger_audit;
      COMMIT;
      SELECT id FROM outbound_dump.constraint_trigger_audit;
      SELECT obj_description(root_trigger.oid, 'pg_trigger'),
             (SELECT count(*) FROM pg_trigger clone_trigger
               WHERE clone_trigger.tgname = root_trigger.tgname)
        FROM pg_trigger root_trigger
       WHERE root_trigger.tgname = 'outbound_partition_after'
         AND root_trigger.tgparentid = 0;
      SELECT relation.relkind, count(inheritance.inhrelid)
        FROM pg_class relation
        LEFT JOIN pg_inherits inheritance ON inheritance.inhparent = relation.oid
       WHERE relation.oid = 'outbound_dump.outbound_partition_region_idx'::regclass
       GROUP BY relation.relkind;
      SELECT a.atttypmod,c.collname
        FROM pg_attribute AS a
        JOIN pg_collation AS c ON c.oid = a.attcollation
       WHERE a.attrelid = (SELECT typrelid FROM pg_type
                            WHERE typnamespace = 'outbound_dump'::regnamespace
                              AND typname = 'metadata')
         AND a.attname = 'code';
      SELECT outbound_dump.dump_total(value) FROM (VALUES (2), (3), (5)) input(value);
      SELECT outbound_dump.dump_first(value), outbound_dump.dump_first(label)
        FROM (VALUES (2, 'x'::text), (3, 'y'::text)) input(value,label);
    " 2>/dev/null)
  expected_outbound_observed=$'1|ok|1|2|t|ok|8|10|200|one\n2|great|3|4|t|great|10|30|400|two\n3\nINSERT 0 1\nYES|ALWAYS\n3|30\nINSERT 0 1\n2|21\nUPDATE 1\n1|10\nDELETE 1\nUPDATE 2\n2|200\n3|300\n2|200\nDELETE 1\n3|300\noutbound_items_note_check\nt\nt\ndumped table comment|dumped column comment\n2\n42\n1\noutbound_constraint_check|c|f|f|f|t\noutbound_constraint_exclusion|x|t|t|t|t\noutbound_constraint_fk|f|t|t|f|t\noutbound_constraint_key|u|t|t|t|t\nt|t|t\nok|9|12|{great}|14|15\n1|one|10|1\n2|two|20|2\n||30|3\nt|t\noutbound_reader_rows|PERMISSIVE|ALL|{outbound_reader}|t|t\n{security_invoker=true}\nSET\n1|outbound_reader\nRESET\n10|1\n20|2\nINSERT 0 1\n30\nBEGIN\nINSERT 0 1\n0\nCOMMIT\n7\ndumped partition trigger|4\nI|1\n7|C\n10\n2|x'
  if [[ "$outbound_observed" == "$expected_outbound_observed" ]]; then
    ok "pos3ql pg_dump restores into PostgreSQL 18 with data, identity, and writable views"
  else
    bad "pos3ql pg_dump round-trip result"
    printf 'expected:\n%s\nobserved:\n%s\n' \
      "$expected_outbound_observed" "$outbound_observed"
  fi
  outbound_statistics=$(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres \
    -X -At -F '|' -v ON_ERROR_STOP=1 -c "
      SELECT stxname,stxstattarget,pg_get_statisticsobjdef_columns(oid)
        FROM pg_statistic_ext
       WHERE stxnamespace='outbound_dump'::regnamespace
       ORDER BY stxname;
    " 2>/dev/null)
  expected_outbound_statistics=$'outbound_items_mood_note|25|mood, note\noutbound_items_note_lower||lower(note)'
  if [[ "$outbound_statistics" == "$expected_outbound_statistics" ]]; then
    ok "pos3ql statistics definitions survive pg_dump and PostgreSQL restore"
  else
    bad "pos3ql statistics pg_dump round-trip result"
    printf 'expected:\n%s\nobserved:\n%s\n' \
      "$expected_outbound_statistics" "$outbound_statistics"
  fi
fi
# The curated corpus later creates a public type with the same unqualified
# name. Keep the PostgreSQL oracle as clean as the fresh pos3ql restart below,
# so pg_type cardinality probes do not inherit this tooling fixture.
psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -X \
  -v ON_ERROR_STOP=1 -c 'DROP SCHEMA IF EXISTS outbound_dump CASCADE; DROP SCHEMA IF EXISTS outbound_type_target CASCADE; DROP ROLE IF EXISTS outbound_reader' \
  > "$WORK/outbound_cleanup.out" 2>&1 || {
    bad "clean outbound pg_dump fixture from PostgreSQL"
    tail -40 "$WORK/outbound_cleanup.out"
  }

# --- two-session historical MVCC and table locks ---------------------------
echo "=== historical MVCC and table-lock differential ==="
if P3_PORT="$P3_PORT" "$PY" "$EXT/mvcc_diff.py" > "$WORK/mvcc_diff.out" 2>&1; then
  ok "repeatable-read history, read-only enforcement and table locks"
else
  bad "historical MVCC and table locks"
  cat "$WORK/mvcc_diff.out"
fi

# --- PostgreSQL 15.18 custom archive through pg_restore --------------------
echo "=== PostgreSQL 15.18 custom archive restore ==="
# Version 15's archive format is readable by the PostgreSQL 18 CI client. The
# ownerful archive exercises ALTER ... OWNER, while
# parallel restore exercises independent pg_restore worker connections.
restart_p3_fresh || exit 1
pg_restore --exit-on-error --jobs=4 \
  -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres \
  tests/data/postgresql-15.18-pgrestore.dump \
  > "$WORK/pg_restore_custom.out" 2>&1
archive_restore_status=$?
if [[ $archive_restore_status -ne 0 ]]; then
  bad "PostgreSQL 15.18 custom archive restores in parallel"
  tail -40 "$WORK/pg_restore_custom.out"
else
  pg_restore --exit-on-error --clean --if-exists --jobs=4 \
    -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres \
    tests/data/postgresql-15.18-pgrestore.dump \
    > "$WORK/pg_restore_clean.out" 2>&1
  archive_clean_status=$?
  if [[ $archive_clean_status -ne 0 ]]; then
    bad "pg_restore --clean --if-exists replaces a populated database"
    tail -40 "$WORK/pg_restore_clean.out"
  else
    pg_restore --exit-on-error --clean --if-exists --single-transaction \
      -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres \
      tests/data/postgresql-15.18-pgrestore.dump \
      > "$WORK/pg_restore_single_transaction.out" 2>&1
    archive_single_status=$?
    if [[ $archive_single_status -ne 0 ]]; then
      bad "pg_restore --single-transaction replaces a populated database"
      tail -40 "$WORK/pg_restore_single_transaction.out"
    fi
    pg_restore --exit-on-error --clean --if-exists --transaction-size=10 \
      -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres \
      tests/data/postgresql-15.18-pgrestore.dump \
      > "$WORK/pg_restore_transaction_size.out" 2>&1
    archive_sized_status=$?
    if [[ $archive_sized_status -ne 0 ]]; then
      bad "pg_restore --transaction-size replaces a populated database"
      tail -40 "$WORK/pg_restore_transaction_size.out"
    fi
    restart_p3 || exit 1
    archive_observed=$(psql -h 127.0.0.1 -p "$P3_PORT" -U "$PGUSER" -d postgres -X -At -F '|' \
      -v ON_ERROR_STOP=1 -c "
        SELECT count(*) FROM app.entry;
        SELECT state,n FROM app.entry_counts ORDER BY state;
        SELECT nextval('app.ticket_seq');
      " 2>/dev/null)
    expected_archive_observed=$'2\nsad|1\nhappy|1\n30'
    if [[ $archive_single_status -eq 0
          && $archive_sized_status -eq 0
          && "$archive_observed" == "$expected_archive_observed" ]]; then
      ok "ownerful parallel/single/sized pg_restore, clean replacement and restart"
    else
      bad "PostgreSQL 15.18 custom archive restore result"
      printf 'expected:\n%s\nobserved:\n%s\n' \
        "$expected_archive_observed" "$archive_observed"
    fi
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
    -e '/^psql:[^:]*:[0-9]+: (HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|DATATYPE NAME|NOTICE|WARNING):/d' \
    -e '/^(HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|DATATYPE NAME|NOTICE|WARNING):/d'
}
run_corpus() { # host port outfile file
  if [[ "$1" == "$PGHOST" && "$2" == "$PGPORT" ]]; then
    PGOPTIONS="-c extension_control_path=$REFERENCE_EXTENSION_CONTROL_ROOT" \
      psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -X -a -q -P pager=off \
        -v VERBOSITY=verbose -f "$4" 2>&1
  else
    psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -X -a -q -P pager=off \
      -v VERBOSITY=verbose -f "$4" 2>&1
  fi | normalize > "$3"
  if [[ "$2" == "$P3_PORT" ]] && ! server_alive "$P3_PID"; then
    echo "pos3ql exited while running $(basename "$4")"
    tail -80 "$WORK/p3.log"
    exit 1
  fi
}
reset_user_extensions() { # host port
  psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -X -A -t -q \
    -c "SELECT extname FROM pg_extension WHERE extname LIKE 'pos3ql_%' ORDER BY extname DESC" |
  while IFS= read -r extension; do
    [[ -z "$extension" ]] && continue
    extension=${extension//\"/\"\"}
    psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -X -q \
      -c "DROP EXTENSION \"$extension\" CASCADE" >/dev/null 2>&1
  done
}
reset_user_relations() { # host port
  psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -X -A -t -q \
    -F $'\t' \
    -c "SELECT n.nspname, c.relname FROM pg_class AS c JOIN pg_namespace AS n ON n.oid = c.relnamespace WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND c.relkind IN ('r', 'p')" |
  while IFS=$'\t' read -r schema relation; do
    [[ -z "$relation" ]] && continue
    schema=${schema//\"/\"\"}
    relation=${relation//\"/\"\"}
    psql -h "$1" -p "$2" -U "$PGUSER" -d postgres -X -q \
      -c "DROP TABLE \"$schema\".\"$relation\" CASCADE" >/dev/null 2>&1
  done
}
reset_corpus_pair() {
  reset_user_extensions "$PGHOST" "$PGPORT"
  reset_user_extensions 127.0.0.1 "$P3_PORT"
  reset_user_relations "$PGHOST" "$PGPORT"
  reset_user_relations 127.0.0.1 "$P3_PORT"
}
reset_corpus_pair
for f in "$EXT"/differential/*.sql; do
  n=$(basename "$f" .sql)
  run_corpus "$PGHOST" "$PGPORT" "$WORK/$n.pg" "$f"
  run_corpus 127.0.0.1 "$P3_PORT" "$WORK/$n.p3" "$f"
  if diff -u "$WORK/$n.pg" "$WORK/$n.p3" > "$WORK/$n.diff"; then ok "corpus: $n"
  else bad "corpus: $n"; head -40 "$WORK/$n.diff"; fi
  reset_corpus_pair
done

# --- exact-error corpora (message wording must match) -----------------------
# Each phase owns its fixed 64-table test budget.  Reusing the curated corpus
# server made otherwise independent checks fail only after its catalog filled.
restart_p3_fresh || exit 1
echo "=== exact-error corpora (message wording must match) ==="
normalize_exact() {
  sed -E \
    -e 's/^psql:[^:]*:[0-9]+: ERROR:  ([0-9A-Z]{5}): *(.*)/ERROR \1 \2/' \
    -e 's/^ERROR:  ([0-9A-Z]{5}): *(.*)/ERROR \1 \2/' \
    -e '/^LINE [0-9]+:/d' -e '/^ *\^ *$/d' \
    -e '/^psql:[^:]*:[0-9]+: (HINT|DETAIL|LOCATION|CONTEXT|SCHEMA NAME|TABLE NAME|COLUMN NAME|CONSTRAINT NAME|DATATYPE NAME|NOTICE|WARNING):/d' \
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
restart_p3_fresh || exit 1
echo "=== COPY binary round-trip (real PostgreSQL vs pos3ql) ==="
if "$PY" "$EXT/copy_binary_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" > "$WORK/copybin.out" 2>&1; then
  ok "COPY binary round-trip ($(tail -1 "$WORK/copybin.out"))"
else
  bad "COPY binary round-trip"; cat "$WORK/copybin.out"
fi

# --- generated type fidelity matrix ----------------------------------------
restart_p3_fresh || exit 1
echo "=== accepted-type fidelity matrix (real PostgreSQL vs pos3ql) ==="
if "$PY" "$EXT/type_fidelity_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" > "$WORK/type_fidelity.out" 2>&1; then
  ok "accepted-type fidelity matrix ($(tail -1 "$WORK/type_fidelity.out"))"
else
  bad "accepted-type fidelity matrix"; cat "$WORK/type_fidelity.out"
fi

# --- LISTEN / NOTIFY (cross-connection; needs two live connections per engine) -
restart_p3_fresh || exit 1
echo "=== LISTEN / NOTIFY (real PostgreSQL vs pos3ql) ==="
if "$PY" "$EXT/listen_notify_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" > "$WORK/listen.out" 2>&1; then
  ok "LISTEN / NOTIFY ($(grep '^ok:' "$WORK/listen.out" | tail -1))"
else
  bad "LISTEN / NOTIFY"; cat "$WORK/listen.out"
fi

# --- extended-protocol binary composites (parameters and results) -------------
restart_p3_fresh || exit 1
echo "=== binary composites (real PostgreSQL vs pos3ql) ==="
if "$PY" "$EXT/binary_param_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" > "$WORK/binparam.out" 2>&1; then
  ok "binary composites ($(tail -1 "$WORK/binparam.out"))"
else
  bad "binary composites"; cat "$WORK/binparam.out"
fi
fi

# --- vendored sqllogictest replay (real PostgreSQL is the oracle) ----------
# Query-block sharded: all files, all statements; this shard runs its slice of
# the read-only query blocks.
if [[ "$RUN_SLT" == 1 ]]; then
  # The curated corpora intentionally leave some objects alive. Give the
  # SQLLogicTest files the full bounded catalog: one file creates 64 primary-key
  # tables, exactly matching both configured pools.
  echo "=== restart pos3ql (fresh table space for sqllogictest) ==="
  restart_p3_fresh || exit 1
  echo "=== sqllogictest replay (query shard $SLT_QUERY_SHARD/$SLT_QUERY_SHARDS) ==="
  if "$PY" "$EXT/slt_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" --limit "$SLT_LIMIT" \
       --max-unsupported "$SLT_UNSUPPORTED_BUDGET" \
       --query-shards "$SLT_QUERY_SHARDS" --query-shard "$SLT_QUERY_SHARD" \
       vendor/test/sqllogictest/test/*.test vendor/test/sqllogictest/test/evidence/*.test \
       "$EXT"/sqllogictest/*.test \
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
  echo "=== generative fuzzer (count=$FUZZ_COUNT seed=$FUZZ_SEED, divergence budget=$FUZZ_BUDGET, unsupported budget=$FUZZ_UNSUPPORTED_BUDGET) ==="
  "$PY" "$EXT/fuzz_diff.py" --pg "$PGPORT" --p3 "$P3_PORT" --count "$FUZZ_COUNT" --seed "$FUZZ_SEED" \
    --max-unsupported "$FUZZ_UNSUPPORTED_BUDGET" \
    > "$WORK/fuzz.out" 2>&1 || true
  DIV=$(grep -oE 'divergence=[0-9]+' "$WORK/fuzz.out" | tail -1 | cut -d= -f2)
  DIV=${DIV:-unknown}
  UNSUP=$(grep -oE 'unsupported=[0-9]+' "$WORK/fuzz.out" | tail -1 | cut -d= -f2)
  UNSUP=${UNSUP:-unknown}
  grep '^TOTAL' "$WORK/fuzz.out"
  if [[ ! "$DIV" =~ ^[0-9]+$ || ! "$UNSUP" =~ ^[0-9]+$ ]]; then
    # No complete summary means the fuzzer crashed before finishing.
    bad "fuzzer produced no complete result"; tail -40 "$WORK/fuzz.out"
  elif (( UNSUP > FUZZ_UNSUPPORTED_BUDGET )); then
    bad "fuzzer has unsupported statements ($UNSUP > $FUZZ_UNSUPPORTED_BUDGET)"
    grep -A3 '^unsupported breakdown:' "$WORK/fuzz.out" | head -60
  elif (( DIV <= FUZZ_BUDGET )); then
    ok "fuzzer within budgets (divergence $DIV <= $FUZZ_BUDGET, unsupported $UNSUP <= $FUZZ_UNSUPPORTED_BUDGET)"
  else
    bad "fuzzer over budget ($DIV > $FUZZ_BUDGET)"; grep -A3 DIVERGENCE "$WORK/fuzz.out" | head -60
  fi
fi

echo ""
echo "passed: $PASS  failed: $FAIL"
[[ $FAIL -eq 0 ]]
