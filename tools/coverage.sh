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
# Two ways to run it:
#   - whole suite (COVERAGE_SHARD unset): everything in one process, the
#     floor enforced at the end. This is the local command.
#   - one shard (COVERAGE_SHARD=sql or COVERAGE_SHARD=run:<groups>): the
#     shard's slice runs strictly (a failure fails the shard — CI runners
#     have docker, MinIO and the reference PostgreSQL, so there is nothing
#     to tolerate), and the profile is written as an lcov tracefile to
#     COVERAGE_LCOV instead of being compared to the floor: one shard's
#     number is meaningless alone. tools/coverage-merge.py unions the
#     tracefiles and holds the floor over the merged whole.
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

command -v cargo-llvm-cov >/dev/null 2>&1 || {
    echo "FAIL: cargo-llvm-cov is not installed (cargo install cargo-llvm-cov)"
    exit 1
}
# Coverage-producing shards need a tracefile to merge; a runtest:* shard does
# not (see its arm below) and is exempt.
case "$SHARD" in
sql | run:*)
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
# The in-process tests belong to the sql shard; the run:* shards measure only
# the server binary their external steps drive.
case "$SHARD" in
"" | sql)
    echo "=== in-process tests ==="
    cargo test --lib --release 2>&1 | grep -E '^test result' | tail -1
    ;;
esac

echo "=== building the instrumented server ==="
# Cargo does not always re-fingerprint on RUSTC_WRAPPER alone, so an existing
# uninstrumented binary can survive this build and silently contribute no
# profile at all. Touching the crate root forces the rebuild.
touch src/lib.rs
cargo build --release 2>&1 | tail -1
BIN="$PWD/target/release/pos3ql"

PGBIN=${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}

run_differential() {
    echo "=== differential suite (server binary) ==="
    # A pipe would mask the suite's exit status, and a suite that aborts (a
    # stale server on the port, say) produces no profile at all -- which shows
    # up as a plausible-looking but far too low coverage figure rather than as
    # a failure. Fail loudly instead.
    if ! POS3QL_BIN="$BIN" zsh tests/external/differential.sh > "$TMP/differential.log" 2>&1; then
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

# Run run.sh for a set of groups, strictly: stream its output into the job log
# as it runs (a shard killed at its CI timeout would otherwise die with
# everything buffered in a file nobody sees — no step times, no culprit), fail
# the shard on any failing step, and refuse to pass a shard that measured
# nothing (an environment gap turning every step into a SKIP). The status file
# carries the exit code across the tee pipe (POSIX sh has no pipefail).
run_groups_strict() {
    groups=$1
    echo "=== external suite, groups: $groups ==="
    { POS3QL_BIN="$BIN" POS3QL_RUN_GROUPS="$groups" \
        zsh tests/external/run.sh 2>&1; echo $? > "$TMP/run.status"; } | tee "$TMP/run.log"
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

case "$SHARD" in
"")
    if [ -n "$POS3QL_VENV" ] && [ -x "$PGBIN/pg_ctl" ]; then
        run_differential
        # run.sh adds the durability and cold-start paths but needs docker and
        # MinIO, which a local machine may not have running; it counts when it
        # runs and is reported when it does not, rather than being silently
        # absent from a number that claims to cover both layers. (The CI
        # shards run it strictly — see the run:* arm.)
        if POS3QL_BIN="$BIN" zsh tests/external/run.sh > "$TMP/run.log" 2>&1; then
            tail -2 "$TMP/run.log"
        else
            echo "NOTE: tests/external/run.sh did not pass in full; its FAIL lines:"
            grep '^FAIL' "$TMP/run.log" || echo "      (none — it exited before any check, likely docker/MinIO)"
            tail -2 "$TMP/run.log"
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
runtest:*)
    # A correctness-only shard: it runs run.sh groups strictly but produces no
    # coverage, so it writes no tracefile and is left out of the merge. This is
    # for crash torture, which kill -9's every server it starts — SIGKILL never
    # runs the profiler's atexit flush, so a torture shard yields zero .profraw
    # and has nothing to report. It has never contributed coverage (the old
    # serial job got none from it either); its value is that it runs and passes.
    run_groups_strict "${SHARD#runtest:}"
    echo "test-only shard: no coverage to write (servers are kill -9'd)"
    ;;
*)
    echo "FAIL: unknown COVERAGE_SHARD '$SHARD' (expected sql, run:<groups> or runtest:<groups>)"
    exit 1
    ;;
esac
