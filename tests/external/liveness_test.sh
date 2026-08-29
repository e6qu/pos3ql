#!/usr/bin/env sh
set -eu

. "$(dirname "$0")/liveness.sh"

sleep 10 &
live_pid=$!
server_alive "$live_pid"
kill "$live_pid"
wait "$live_pid" 2>/dev/null || true

process_state() {
  printf '%s\n' Z
}
if server_alive "$$"; then
  echo "server_alive accepted an unreaped zombie" >&2
  exit 1
fi

free_port=$(choose_free_port 20000 20100)
python3 - "$free_port" <<'PY' &
import socket
import sys
import time

listener = socket.socket()
listener.bind(("127.0.0.1", int(sys.argv[1])))
listener.listen()
time.sleep(10)
PY
listener_pid=$!
sleep 0.1
if choose_free_port "$free_port" "$free_port" >/dev/null; then
  echo "choose_free_port selected a listening port" >&2
  kill "$listener_pid" 2>/dev/null || true
  wait "$listener_pid" 2>/dev/null || true
  exit 1
fi
kill "$listener_pid" 2>/dev/null || true
wait "$listener_pid" 2>/dev/null || true
