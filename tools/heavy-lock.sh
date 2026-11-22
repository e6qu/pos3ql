#!/usr/bin/env bash
# Runs one heavy local job at a time across every worktree and agent.
#
# Compiles, test suites, differential phases and fuzz loops each need several
# GiB; two at once on a laptop starve the host (see tools/dev-memory-guard.sh).
# This wrapper takes a host-wide lock, runs the command, and releases the lock
# when the command exits, waiting meanwhile for any other holder. A lock whose
# holder process no longer exists is reclaimed.
#
# Usage: tools/heavy-lock.sh <command> [args...]
#   tools/heavy-lock.sh cargo clippy --locked --all-targets -- -Dwarnings
#   tools/heavy-lock.sh bash tests/external/ci_diff.sh "$BIN" "$EVIDENCE"
# Settings: HEAVY_LOCK_DIR (default $HOME/.cache/pos3ql/heavy.lock).
set -u -o pipefail

if (( $# == 0 )); then
    printf 'usage: %s <command> [args...]\n' "$0" >&2
    exit 2
fi
lock=${HEAVY_LOCK_DIR:-$HOME/.cache/pos3ql/heavy.lock}
mkdir -p "$(dirname "$lock")"

# Waiters take a ticket and acquire in ticket order, so a job that starts
# polling later never overtakes one already waiting. A ticket whose owner
# exited is dropped.
queue="$lock.queue"
mkdir -p "$queue"
ticket="$queue/$(date +%s).$(printf '%08d' "$$")"
: > "$ticket"
trap 'rm -f "$ticket"' EXIT
announced=0
while :; do
    first=
    for waiting in "$queue"/*; do
        owner=${waiting##*.}
        if ! kill -0 "$((10#$owner))" 2>/dev/null; then
            rm -f "$waiting"
            continue
        fi
        first=$waiting
        break
    done
    if [[ "$first" == "$ticket" ]] && mkdir "$lock" 2>/dev/null; then
        break
    fi
    holder=$(cat "$lock/pid" 2>/dev/null)
    if [[ -n "$holder" ]] && ! kill -0 "$holder" 2>/dev/null; then
        printf 'heavy-lock: reclaiming lock of exited pid %s\n' "$holder" >&2
        rm -rf "$lock"
        continue
    fi
    if (( ! announced )); then
        printf 'heavy-lock: waiting for pid %s (%s)\n' "${holder:-?}" \
            "$(cat "$lock/command" 2>/dev/null)" >&2
        announced=1
    fi
    sleep 2
done
rm -f "$ticket"
printf '%s\n' "$$" > "$lock/pid"
printf '%s\n' "$PWD: $*" > "$lock/command"
trap 'rm -rf "$lock"' EXIT
trap 'exit 130' INT TERM
"$@"
