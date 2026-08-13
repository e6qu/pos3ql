#!/usr/bin/env sh
# Shared process-liveness primitive for external harnesses.

# `kill -0` succeeds for an unreaped zombie. A test server is alive only while
# it can still execute, so callers can stop at the originating crash.
server_alive() {
  kill -0 "$1" 2>/dev/null || return 1
  case "$(ps -o stat= -p "$1" 2>/dev/null)" in *Z*) return 1;; esac
  return 0
}
