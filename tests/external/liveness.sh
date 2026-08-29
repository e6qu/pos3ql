#!/usr/bin/env sh
# Shared process-liveness primitive for external harnesses.

process_state() {
  ps -o stat= -p "$1" 2>/dev/null
}

# `kill -0` succeeds for an unreaped zombie. A test server is alive only while
# it can still execute, so callers can stop at the originating crash.
server_alive() {
  kill -0 "$1" 2>/dev/null || return 1
  case "$(process_state "$1")" in *Z*) return 1;; esac
  return 0
}

# Return an unused loopback TCP port from the requested inclusive range. Test
# harnesses use this only when the caller did not provide a port; an explicit
# port is checked separately so CI configuration remains deterministic.
choose_free_port() {
  port=$1
  last=$2
  while [ "$port" -le "$last" ]; do
    if ! nc -z 127.0.0.1 "$port" >/dev/null 2>&1; then
      printf '%s\n' "$port"
      return 0
    fi
    port=$((port + 1))
  done
  return 1
}
