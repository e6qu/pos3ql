#!/bin/sh
# Line/region coverage over BOTH test layers, which is the only number that
# means anything here: `cargo test` exercises the crate in-process, while the
# differential corpora, the sqllogictest blocks and the psql golden suite drive
# the *server binary* as a subprocess. Measuring only the first says 56% and
# reports the wire protocol at 6%, which is an artefact of what is instrumented
# rather than of what is tested.
#
# `cargo llvm-cov show-env` exports the RUSTFLAGS and LLVM_PROFILE_FILE that
# make an ordinary `cargo build` emit an instrumented binary; the external
# harnesses then run that binary through POS3QL_BIN and their profiles land in
# the same directory, so one report covers everything.
#
# Ways to run it (COVERAGE_SHARD):
#   - unset — whole suite in one process, the floor enforced at the end. This
#     is the local command.
#   - lib:INDEX-of-COUNT / sql / run:<groups> — a coverage shard: its slice runs
#     strictly against an instrumented binary, and the profile is written as an
#     lcov tracefile. tools/coverage-merge.py unions the tracefiles and holds
#     the floor over the merged whole.
#   - runtest:<groups> — a correctness-only shard: it runs run.sh groups
#     strictly but produces no coverage, so it builds an *uninstrumented*
#     binary (much faster) and writes no tracefile. This is for crash torture,
#     which kill -9's every server it starts — SIGKILL never runs the profiler's
#     atexit flush, so an instrumented torture shard yields zero .profraw and
#     only pays the instrumentation tax for nothing. Its value is that it runs
#     and passes.
#
# Ratchet, like tools/check-dups.sh: MIN may be raised as coverage improves and
# is never lowered without a reason.
set -e
here=$(dirname "$0")
cd "$here/.."
MIN=${COVERAGE_MIN:-70}
SHARD=${COVERAGE_SHARD:-}
LCOV_OUT=${COVERAGE_LCOV:-}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
PGBIN=${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}

# Run run.sh for a set of groups, strictly: stream its output into the job log
# as it runs (a shard killed at its CI timeout would otherwise die with
# everything buffered in a file nobody sees — no step times, no culprit), fail
# the shard on any failing step, and refuse to pass a shard that measured
# nothing (an environment gap turning every step into a SKIP). The status file
# carries the exit code across the tee pipe (POSIX sh has no pipefail). Uses the
# global BIN, which the caller sets first.
run_groups_strict() {
    groups=$1
    echo "=== external suite, groups: $groups ==="
    {
        if POS3QL_BIN="$BIN" POS3QL_RUN_GROUPS="$groups" \
            bash tests/external/run.sh 2>&1; then
            echo 0 > "$TMP/run.status"
        else
            echo $? > "$TMP/run.status"
        fi
    } | tee "$TMP/run.log"
    if [ ! -f "$TMP/run.status" ]; then
        echo "FAIL: tests/external/run.sh did not report an exit status (groups $groups)"
        exit 1
    fi
    if [ "$(cat "$TMP/run.status")" != "0" ]; then
        echo "FAIL: tests/external/run.sh (groups $groups); its FAIL lines:"
        grep '^FAIL' "$TMP/run.log" || true
        exit 1
    fi
    if ! grep -q '^PASS' "$TMP/run.log"; then
        echo "FAIL: the shard produced no PASS lines; nothing was measured"
        exit 1
    fi
}

# A correctness-only shard never touches llvm-cov: it builds a plain release
# binary (no instrumentation tax) and runs its groups. Handle it before any of
# the coverage machinery below.
case "$SHARD" in
runtest:*)
    echo "=== building the server (uninstrumented; correctness shard) ==="
    # Force the rebuild: cargo does not reliably re-fingerprint on the coverage
    # RUSTFLAGS/wrapper changing, so an *instrumented* binary left in target/ by
    # a prior coverage run would otherwise survive into this uninstrumented
    # build — and run several times slower for nothing. (A fresh CI runner has
    # no such binary; this matters for back-to-back local runs.)
    touch src/lib.rs
    cargo build --release 2>&1 | tail -1
    BIN="$PWD/target/release/pos3ql"
    run_groups_strict "${SHARD#runtest:}"
    echo "test-only shard: no coverage to write (uninstrumented, servers kill -9'd)"
    exit 0
    ;;
esac

command -v cargo-llvm-cov >/dev/null 2>&1 || {
    echo "FAIL: cargo-llvm-cov is not installed (cargo install cargo-llvm-cov)"
    exit 1
}
# Coverage-producing shards need a tracefile to merge.
case "$SHARD" in
lib:* | sql | run:*)
    if [ -z "$LCOV_OUT" ]; then
        echo "FAIL: COVERAGE_SHARD=$SHARD is set but COVERAGE_LCOV is not; a"
        echo "      shard's floor-less percentage would vanish without a"
        echo "      tracefile to merge"
        exit 1
    fi
    ;;
esac

eval "$(cargo llvm-cov show-env --sh)"
# `cargo llvm-cov clean` shells out to `cargo clean`, which refuses to touch a
# target/ without a CACHEDIR.TAG; drop the raw profiles directly instead.
find target -name '*.profraw' -delete 2>/dev/null || true

# Everything is measured in the release profile so that the unit tests and the
# server binary the external suites drive produce profiles against the same
# objects; mixing profiles makes llvm-cov fail to find one of them.
# The in-process tests have their own trace shard so their growing runtime
# cannot consume the differential shard's server-build budget.
case "$SHARD" in
"")
    echo "=== in-process tests ==="
    if ! cargo test --lib --release > "$TMP/lib.log" 2>&1; then
        tail -80 "$TMP/lib.log"
        echo "FAIL: in-process tests did not pass; coverage cannot be reported"
        exit 1
    fi
    grep -E '^test result' "$TMP/lib.log" | tail -1
    ;;
lib:*)
    partition=${SHARD#lib:}
    partition_index=${partition%%-of-*}
    partition_count=${partition#*-of-}
    case "$partition_index:$partition_count" in
    *[!0-9:]* | :* | *: | *:*:*)
        echo "FAIL: malformed lib partition '$partition' (expected INDEX-of-COUNT)"
        exit 1
        ;;
    esac
    if [ "$partition_count" -lt 2 ] || [ "$partition_index" -ge "$partition_count" ]; then
        echo "FAIL: invalid lib partition '$partition' (COUNT >= 2 and INDEX < COUNT required)"
        exit 1
    fi

    echo "=== in-process tests, partition $partition ==="
    if ! cargo test --lib --release -- --list > "$TMP/lib.list" 2>&1; then
        tail -80 "$TMP/lib.list"
        echo "FAIL: in-process tests could not be enumerated"
        exit 1
    fi
    set -- --exact
    ordinal=0
    selected=0
    while IFS= read -r line; do
        case "$line" in
        *': test')
            test_name=${line%: test}
            if [ $((ordinal % partition_count)) -eq "$partition_index" ]; then
                selected=$((selected + 1))
            else
                set -- "$@" --skip "$test_name"
            fi
            ordinal=$((ordinal + 1))
            ;;
        esac
    done < "$TMP/lib.list"
    if [ "$selected" -eq 0 ] || [ "$ordinal" -eq "$selected" ]; then
        echo "FAIL: lib partition $partition selected $selected of $ordinal tests"
        exit 1
    fi
    if ! cargo test --lib --release -- "$@" > "$TMP/lib.log" 2>&1; then
        tail -80 "$TMP/lib.log"
        echo "FAIL: in-process test partition $partition did not pass"
        exit 1
    fi
    grep -E '^test result' "$TMP/lib.log" | tail -1
    ;;
esac
# The in-process tests' profiles are the baseline; the external suites must
# add server profiles on top.
LIB_PROFILES=$(find target -name '*.profraw' 2>/dev/null | wc -l | tr -d ' ')

case "$SHARD" in
lib:*)
    echo "=== lcov tracefile ==="
    cargo llvm-cov report --release --lcov --output-path "$LCOV_OUT"
    echo "wrote $LCOV_OUT"
    exit 0
    ;;
esac

echo "=== building the instrumented server ==="
# Cargo does not always re-fingerprint on RUSTC_WRAPPER alone, so an existing
# uninstrumented binary can survive this build and silently contribute no
# profile at all. Touching the crate root forces the rebuild.
touch src/lib.rs
cargo build --release 2>&1 | tail -1
BIN="$PWD/target/release/pos3ql"

run_differential() {
    echo "=== differential suite (server binary) ==="
    # A pipe would mask the suite's exit status, and a suite that aborts (a
    # stale server on the port, say) produces no profile at all -- which shows
    # up as a plausible-looking but far too low coverage figure rather than as
    # a failure. Fail loudly instead.
    if ! POS3QL_BIN="$BIN" bash tests/external/differential.sh > "$TMP/differential.log" 2>&1; then
        tail -6 "$TMP/differential.log"
        echo "FAIL: tests/external/differential.sh did not pass; coverage would understate"
        exit 1
    fi
    tail -2 "$TMP/differential.log"
}

write_lcov() {
    echo "=== lcov tracefile ==="
    cargo llvm-cov report --release --lcov --output-path "$LCOV_OUT"
    echo "wrote $LCOV_OUT"
}

case "$SHARD" in
"")
    if [ -n "$POS3QL_VENV" ] && [ -x "$PGBIN/pg_ctl" ]; then
        run_differential
        # run.sh adds durability and cold-start coverage.  When its required
        # local services are present, a failed check must fail coverage too.
        if ! POS3QL_BIN="$BIN" bash tests/external/run.sh > "$TMP/run.log" 2>&1; then
            tail -20 "$TMP/run.log"
            echo "FAIL: tests/external/run.sh did not pass"
            exit 1
        fi
        tail -2 "$TMP/run.log"
        # The external suites drive the *server binary* as a subprocess; if
        # they produced no profile beyond the in-process tests', the external
        # layer contributed nothing and the combined figure would read as a
        # plausible-looking but far too low number. Fail loudly rather
        # than report a figure that claims to cover both layers but does not.
        ALL_PROFILES=$(find target -name '*.profraw' 2>/dev/null | wc -l | tr -d ' ')
        if [ "$ALL_PROFILES" -le "$LIB_PROFILES" ]; then
            echo "FAIL: the external suites wrote no server profile" \
                 "($ALL_PROFILES profraw file(s), the in-process tests left $LIB_PROFILES);"
            echo "      the external layer would contribute nothing. Suspect a stale" \
                 "uninstrumented binary, kill -9'd servers flushing nothing, or a"
            echo "      profile-signature mismatch. llvm-cov/profdata detail:"
            cargo llvm-cov report --release --summary-only 2>&1 | grep -iE "warning|error|fail|no .*prof" || true
            exit 1
        fi
    else
        # Skipping is not a lower number, it is a different measurement:
        # without the suites the figure covers only what runs in-process, and
        # comparing that to a floor set for both layers fails for a reason
        # that has nothing to do with coverage. Say so and stop, rather than
        # report a figure that reads as real.
        echo "=== differential suites SKIPPED ==="
        echo "    POS3QL_VENV is unset or no pg_ctl at $PGBIN"
        echo "    (set POS3QL_PGBIN if PostgreSQL 18 lives elsewhere)"
        echo "SKIP: cannot measure both layers, so the floor does not apply"
        exit 0
    fi

    echo "=== combined report ==="
    cargo llvm-cov report --release --summary-only 2>&1 | tail -3
    PCT=$(cargo llvm-cov report --release --summary-only 2>/dev/null | awk '/^TOTAL/ {gsub("%","",$10); print int($10)}')
    echo "line coverage: ${PCT}%  (floor ${MIN}%)"
    [ "$PCT" -ge "$MIN" ] || { echo "FAIL: line coverage ${PCT}% is below the ${MIN}% floor"; exit 1; }
    echo "OK"
    ;;
sql)
    if [ -z "$POS3QL_VENV" ] || [ ! -x "$PGBIN/pg_ctl" ]; then
        echo "FAIL: the sql shard needs POS3QL_VENV and PostgreSQL 18 (POS3QL_PGBIN)"
        exit 1
    fi
    run_differential
    write_lcov
    ;;
run:*)
    run_groups_strict "${SHARD#run:}"
    write_lcov
    ;;
*)
    echo "FAIL: unknown COVERAGE_SHARD '$SHARD' (expected lib:INDEX-of-COUNT, sql, run:<groups> or runtest:<groups>)"
    exit 1
    ;;
esac
