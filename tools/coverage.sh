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
# Ratchet, like tools/check-dups.sh: MIN may be raised as coverage improves and
# is never lowered without a reason.
set -e
here=$(dirname "$0")
cd "$here/.."
MIN=${COVERAGE_MIN:-70}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

command -v cargo-llvm-cov >/dev/null 2>&1 || {
    echo "FAIL: cargo-llvm-cov is not installed (cargo install cargo-llvm-cov)"
    exit 1
}

eval "$(cargo llvm-cov show-env --sh)"
# `cargo llvm-cov clean` shells out to `cargo clean`, which refuses to touch a
# target/ without a CACHEDIR.TAG; drop the raw profiles directly instead.
find target -name '*.profraw' -delete 2>/dev/null || true

# Everything is measured in the release profile so that the unit tests and the
# server binary the external suites drive produce profiles against the same
# objects; mixing profiles makes llvm-cov fail to find one of them.
echo "=== in-process tests ==="
cargo test --lib --release 2>&1 | grep -E '^test result' | tail -1

echo "=== building the instrumented server ==="
# Cargo does not always re-fingerprint on RUSTC_WRAPPER alone, so an existing
# uninstrumented binary can survive this build and silently contribute no
# profile at all. Touching the crate root forces the rebuild.
touch src/lib.rs
cargo build --release 2>&1 | tail -1
BIN="$PWD/target/release/pos3ql"

PGBIN=${POS3QL_PGBIN:-/opt/homebrew/opt/postgresql@18/bin}
if [ -n "$POS3QL_VENV" ] && [ -x "$PGBIN/pg_ctl" ]; then
    echo "=== differential suites (server binary) ==="
    # A pipe would mask the suite's exit status, and a suite that aborts (a
    # stale server on the port, say) produces no profile at all -- which shows
    # up as a plausible-looking but far too low coverage figure rather than as
    # a failure. Fail loudly instead.
    # differential.sh carries the SQL surface and is required. run.sh adds the
    # durability and cold-start paths but needs docker and MinIO, so it counts
    # when it runs and is reported when it does not, rather than being silently
    # absent from a number that claims to cover both layers.
    if ! POS3QL_BIN="$BIN" zsh tests/external/differential.sh > "$TMP/differential.log" 2>&1; then
        tail -6 "$TMP/differential.log"
        echo "FAIL: tests/external/differential.sh did not pass; coverage would understate"
        exit 1
    fi
    tail -2 "$TMP/differential.log"
    if POS3QL_BIN="$BIN" zsh tests/external/run.sh > "$TMP/run.log" 2>&1; then
        tail -2 "$TMP/run.log"
    else
        echo "NOTE: tests/external/run.sh did not run (needs docker + MinIO);"
        echo "      the durability and cold-start paths are not counted below"
        tail -2 "$TMP/run.log"
    fi
else
    # Skipping is not a lower number, it is a different measurement: without the
    # suites the figure covers only what runs in-process, and comparing that to
    # a floor set for both layers fails for a reason that has nothing to do with
    # coverage. Say so and stop, rather than report a figure that reads as real.
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
