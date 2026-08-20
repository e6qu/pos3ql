#!/usr/bin/env sh
set -eu

. "$(dirname "$0")/liveness.sh"

sleep 10 &
live_pid=$!
server_alive "$live_pid"
kill "$live_pid"
wait "$live_pid" 2>/dev/null || true

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/pos3ql-liveness.XXXXXX")
trap 'kill "${zombie_parent:-}" 2>/dev/null || true; wait "${zombie_parent:-}" 2>/dev/null || true; rm -rf "$work_dir"' EXIT
python3 -c '
import os
import time
child = os.fork()
if child == 0:
    os._exit(0)
print(child, flush=True)
time.sleep(10)
' > "$work_dir/zombie-pid" &
zombie_parent=$!

zombie_pid=''
attempt=0
while [ "$attempt" -lt 50 ]; do
  if [ -s "$work_dir/zombie-pid" ]; then
    zombie_pid=$(cat "$work_dir/zombie-pid")
    case "$(ps -o stat= -p "$zombie_pid" 2>/dev/null)" in *Z*) break;; esac
  fi
  attempt=$((attempt + 1))
  sleep 0.02
done
[ -n "$zombie_pid" ]
case "$(ps -o stat= -p "$zombie_pid" 2>/dev/null)" in *Z*) ;; *) exit 1;; esac
if server_alive "$zombie_pid"; then
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
