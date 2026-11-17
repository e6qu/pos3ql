#!/usr/bin/env bash
# Development memory guard: keeps local test runs from exhausting the host.
#
# A test client that buffers an unbounded result (psql reading a cartesian
# join) or several builds and servers at once can starve the machine until
# the OS watchdog panics it. This guard polls every few seconds and stops
# development processes before that happens:
#   - a test client (psql, python) larger than GUARD_CLIENT_RSS_MIB;
#   - a database server (pos3ql, postgres) larger than GUARD_SERVER_RSS_MIB;
#   - when available system memory falls below GUARD_MIN_AVAILABLE_PCT, the
#     largest guarded process, including compilers (rustc, clippy-driver)
#     and test binaries under a target/ directory.
# It only signals processes owned by the invoking user whose names are in
# that list; browsers, editors and VMs are never touched. Each stop sends
# SIGTERM, then SIGKILL after GUARD_KILL_GRACE seconds, and is logged with
# the process's command line. Free disk below GUARD_MIN_DISK_GIB is logged
# as a warning, because swap needs that space.
#
# Usage: tools/dev-memory-guard.sh [--once]
#   Run it in the background for the whole development session:
#     nohup tools/dev-memory-guard.sh >/dev/null 2>&1 &
#   --once performs a single check and exits (used by its self-test).
# Settings (environment, with defaults):
#   GUARD_CLIENT_RSS_MIB=3072 GUARD_SERVER_RSS_MIB=10240
#   GUARD_MIN_AVAILABLE_PCT=12 GUARD_MIN_DISK_GIB=20
#   GUARD_INTERVAL=2 GUARD_KILL_GRACE=3
#   GUARD_LOG=$HOME/.cache/pos3ql/dev-memory-guard.log
set -u -o pipefail

client_rss_mib=${GUARD_CLIENT_RSS_MIB:-3072}
server_rss_mib=${GUARD_SERVER_RSS_MIB:-10240}
min_available_pct=${GUARD_MIN_AVAILABLE_PCT:-12}
min_disk_gib=${GUARD_MIN_DISK_GIB:-20}
interval=${GUARD_INTERVAL:-2}
kill_grace=${GUARD_KILL_GRACE:-3}
log=${GUARD_LOG:-$HOME/.cache/pos3ql/dev-memory-guard.log}
once=0
case "${1:-}" in
    --once) once=1 ;;
    "") ;;
    *) printf 'usage: %s [--once]\n' "$0" >&2; exit 2 ;;
esac
mkdir -p "$(dirname "$log")"

note() {
    local line
    line="$(date '+%Y-%m-%d %H:%M:%S') $*"
    printf '%s\n' "$line" >> "$log"
    printf '%s\n' "$line" >&2
}

# Percentage of physical memory still available to new allocations.
available_pct() {
    case "$(uname -s)" in
        Darwin)
            memory_pressure 2>/dev/null \
                | sed -n 's/^System-wide memory free percentage: \([0-9]*\)%$/\1/p'
            ;;
        Linux)
            awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2} END{if (t) print int(a*100/t)}' \
                /proc/meminfo
            ;;
    esac
}

# The class of a guarded process: client, server or build; empty otherwise.
process_class() {
    local name command=$2
    name=$(printf '%s' "${1##*/}" | tr '[:upper:]' '[:lower:]')
    case "$name" in
        psql|python|python3|python3.*) echo client ;;
        pos3ql|pos3ql.*|postgres) echo server ;;
        rustc|clippy-driver|cargo) echo build ;;
        *)
            # Test binaries cargo builds (target/debug/deps/pos3ql-<hash>).
            case "$command" in
                */target/*/deps/*) echo build ;;
            esac
            ;;
    esac
}

stop_process() {
    local pid=$1 rss_mib=$2 reason=$3 command
    command=$(ps -o command= -p "$pid" 2>/dev/null | cut -c1-300)
    [[ -n "$command" ]] || return 0
    note "STOP pid=$pid rss=${rss_mib}MiB reason=$reason command=$command"
    kill -TERM "$pid" 2>/dev/null || return 0
    local waited=0
    while kill -0 "$pid" 2>/dev/null && (( waited < kill_grace )); do
        sleep 1
        waited=$((waited + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        note "KILL pid=$pid (still running after SIGTERM)"
        kill -KILL "$pid" 2>/dev/null
    fi
}

check() {
    local pid rss comm command class rss_mib
    local largest_pid="" largest_rss=0
    local me=$$
    while read -r pid rss comm; do
        [[ "$pid" == "$me" ]] && continue
        command=$(ps -o command= -p "$pid" 2>/dev/null) || continue
        class=$(process_class "$comm" "$command")
        [[ -n "$class" ]] || continue
        rss_mib=$((rss / 1024))
        case "$class" in
            client)
                if (( rss_mib > client_rss_mib )); then
                    stop_process "$pid" "$rss_mib" "client over ${client_rss_mib}MiB"
                    continue
                fi
                ;;
            server)
                if (( rss_mib > server_rss_mib )); then
                    stop_process "$pid" "$rss_mib" "server over ${server_rss_mib}MiB"
                    continue
                fi
                ;;
        esac
        if (( rss > largest_rss )); then
            largest_rss=$rss
            largest_pid=$pid
        fi
    done < <(ps -U "$(id -u)" -o pid=,rss=,comm=)

    local available
    available=$(available_pct)
    if [[ -n "$available" ]] && (( available < min_available_pct )) && [[ -n "$largest_pid" ]]; then
        stop_process "$largest_pid" "$((largest_rss / 1024))" \
            "system memory available ${available}% below ${min_available_pct}%"
    fi

    local disk_gib
    disk_gib=$(df -Pk / | awk 'NR==2{print int($4/1048576)}')
    if (( disk_gib < min_disk_gib )); then
        note "WARN free disk ${disk_gib}GiB below ${min_disk_gib}GiB; swap may run out"
    fi
}

if (( once )); then
    check
    exit 0
fi
note "START guard pid=$$ client<=${client_rss_mib}MiB server<=${server_rss_mib}MiB available>=${min_available_pct}% disk>=${min_disk_gib}GiB"
trap 'note "EXIT guard pid=$$"; exit 0' INT TERM
while true; do
    check
    sleep "$interval"
done
