#!/usr/bin/env python3
"""Reserve local harness ports across build delays and concurrent processes."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import socket
import sys
import tempfile


def owner_alive(owner):
    try:
        os.kill(owner, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def port_available(port):
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            listener.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False


def update_reservations(state, owner, requested=None, first=None, last=None):
    if owner <= 0 or not owner_alive(owner):
        raise ValueError("test-port owner must be a live positive process identifier")
    if first is not None:
        if requested:
            first = last = int(requested)
        if not 1 <= first <= last <= 65535:
            raise ValueError("test ports must be an ordered range within 1..65535")
    descriptor = os.open(str(state) + ".lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "r+") as stream:
        # A single OS lock makes stale-owner reclamation and new claims atomic.
        # It is released automatically even if a claiming process crashes.
        fcntl.flock(stream, fcntl.LOCK_EX)
        if state.exists():
            state_descriptor = os.open(state, os.O_RDONLY | os.O_NOFOLLOW)
            with os.fdopen(state_descriptor) as state_stream:
                records = json.load(state_stream)
        else:
            records = {}
        if not isinstance(records, dict) or any(
            not port.isdecimal()
            or not 1 <= int(port) <= 65535
            or type(pid) is not int
            or pid <= 0
            for port, pid in records.items()
        ):
            raise ValueError("malformed test-port reservation state")
        records = {port: pid for port, pid in records.items() if owner_alive(pid)}
        selected = None
        if first is None:
            records = {port: pid for port, pid in records.items() if pid != owner}
        else:
            for port in range(first, last + 1):
                if str(port) not in records and port_available(port):
                    records[str(port)] = owner
                    selected = port
                    break
            if selected is None:
                raise ValueError(f"no unreserved available test port in {first}..{last}")
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w", dir=state.parent, prefix=state.name + ".", delete=False
            ) as output:
                temporary = Path(output.name)
                json.dump(records, output, sort_keys=True)
            os.replace(temporary, state)
            temporary = None
        finally:
            if temporary is not None:
                temporary.unlink()
        return selected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("claim", "release"))
    parser.add_argument("owner", type=int)
    parser.add_argument("requested", nargs="?", default="")
    parser.add_argument("first", nargs="?", type=int)
    parser.add_argument("last", nargs="?", type=int)
    args = parser.parse_args()
    if args.action == "claim" and (args.first is None or args.last is None):
        parser.error("claim requires requested, first, and last arguments")
    state = Path(os.environ.get(
        "POS3QL_TEST_PORT_STATE", f"/tmp/pos3ql-test-ports-{os.getuid()}.json"
    ))
    try:
        port = update_reservations(
            state, args.owner, args.requested,
            args.first if args.action == "claim" else None,
            args.last if args.action == "claim" else None,
        )
        if port is not None:
            print(port)
    except (OSError, ValueError) as error:
        print(f"FAIL: test-port reservation: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
