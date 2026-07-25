#!/usr/bin/env python3
"""LISTEN / NOTIFY differential: real PostgreSQL vs pos3ql.

Asynchronous notifications are cross-connection, so this drives two connections
per engine over the wire with psycopg: a listener registers channels, a notifier
raises notifications, and the listener drains what arrives. The (channel,
payload) sequences a listener receives must match PostgreSQL's exactly (the
backend PID differs between engines and is not compared).

Covered: a plain NOTIFY, a NOTIFY with payload, several channels, UNLISTEN and
UNLISTEN *, self-notification, transactional delivery (COMMIT delivers, ROLLBACK
discards), and same-transaction de-duplication.

  listen_notify_diff.py --pg PORT --p3 PORT [--host HOST]
"""
import argparse
import sys

try:
    import psycopg
except ImportError:
    print("psycopg not installed", file=sys.stderr)
    sys.exit(2)


def connect(host, port):
    return psycopg.connect(host=host, port=port, user="postgres", dbname="postgres", autocommit=True)


def drain(listener, timeout=1.0):
    """Collect (channel, payload) for whatever notifications have arrived."""
    out = []
    for note in listener.notifies(timeout=timeout, stop_after=None):
        out.append((note.channel, note.payload))
    return out


def scenario(conn_factory):
    """Run the fixed script against one engine, returning what the listener saw."""
    listener = conn_factory()
    notifier = conn_factory()
    got = []

    listener.execute("LISTEN a")
    listener.execute("LISTEN b")

    # Plain notify, payload notify, a channel with no listener (dropped),
    # then another on a live channel.
    notifier.execute("NOTIFY a")
    notifier.execute("NOTIFY b, 'hello'")
    notifier.execute("NOTIFY c, 'nobody home'")
    notifier.execute("NOTIFY a, 'from notifier'")
    got += drain(listener)

    # UNLISTEN one channel: further NOTIFY a is not received, b still is.
    listener.execute("UNLISTEN a")
    notifier.execute("NOTIFY a, 'gone'")
    notifier.execute("NOTIFY b, 'still here'")
    got += drain(listener)

    # Transactional: a rolled-back NOTIFY is discarded, a committed one fires;
    # duplicate (channel, payload) within one transaction collapses to one.
    notifier.execute("BEGIN")
    notifier.execute("NOTIFY b, 'rolled back'")
    notifier.execute("ROLLBACK")
    notifier.execute("BEGIN")
    notifier.execute("NOTIFY b, 'committed'")
    notifier.execute("NOTIFY b, 'committed'")
    notifier.execute("NOTIFY b, 'twice'")
    notifier.execute("COMMIT")
    got += drain(listener)

    # UNLISTEN * drops everything.
    listener.execute("UNLISTEN *")
    notifier.execute("NOTIFY b, 'after unlisten all'")
    got += drain(listener)

    listener.close()
    notifier.close()
    return got


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pg", type=int, required=True)
    ap.add_argument("--p3", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    args = ap.parse_args()

    pg = scenario(lambda: connect(args.host, args.pg))
    p3 = scenario(lambda: connect(args.host, args.p3))

    if pg == p3:
        print("ok: listener received identical notifications (%d)" % len(pg))
        print("listen-notify: 0 check(s) failed")
        sys.exit(0)
    print("DIVERGENCE: notification streams differ")
    print("  postgres: %r" % (pg,))
    print("  pos3ql:   %r" % (p3,))
    print("listen-notify: 1 check(s) failed")
    sys.exit(1)


if __name__ == "__main__":
    main()
