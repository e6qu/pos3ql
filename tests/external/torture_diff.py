#!/usr/bin/env python3
"""Crash-torture differential: random DML against pos3ql AND real PostgreSQL,
with random kill -9 restarts and wiped-disk cold starts of pos3ql between
batches. After every recovery the full contents of every table must equal the
reference — the reference database *is* the model, so the spill / delta /
tombstone / WAL / manifest machinery is checked against PostgreSQL itself
rather than a hand-maintained oracle.

Determinism: every run is driven by --seed (default fixed), so a failure
reproduces from its command line. Kills are issued between acknowledged
statements: every mirrored operation was acked by pos3ql first, so after any
recovery the two databases must agree exactly.

Env: P3_BIN, P3_CONF, P3_PORT, P3_DATADIR (pos3ql control);
     PGHOST/PGPORT/PGUSER/PGDATABASE (the reference).
"""

import argparse
import os
import random
import signal
import subprocess
import sys
import time

import psycopg


def connect_p3(port):
    deadline = time.time() + 15
    while True:
        try:
            return psycopg.connect(
                host="127.0.0.1", port=port, user="ext", dbname="ext", autocommit=True
            )
        except Exception:
            if time.time() > deadline:
                raise
            time.sleep(0.1)


def connect_pg():
    return psycopg.connect(
        host=os.environ.get("PGHOST", "127.0.0.1"),
        port=int(os.environ.get("PGPORT", "5432")),
        user=os.environ.get("PGUSER", "postgres"),
        dbname=os.environ.get("PGDATABASE", os.environ.get("PGUSER", "postgres")),
        autocommit=True,
    )


class P3Server:
    def __init__(self, binary, conf, port, datadir, log):
        self.binary, self.conf, self.port, self.datadir = binary, conf, port, datadir
        self.log = open(log, "ab")
        self.proc = None

    def start(self):
        self.proc = subprocess.Popen(
            [self.binary, "--config", self.conf], stdout=self.log, stderr=self.log
        )

    def kill9(self):
        if self.proc:
            self.proc.send_signal(signal.SIGKILL)
            self.proc.wait()
            self.proc = None

    def wipe_data(self):
        subprocess.run(["rm", "-rf", self.datadir], check=True)


TABLES = ["ta", "tb"]


def table_digest(cur, table):
    # One row per table: count plus an order-independent content fingerprint.
    cur.execute(
        f"SELECT count(*), coalesce(sum(id), 0), coalesce(sum(length(pad)), 0) FROM {table}"
    )
    summary = cur.fetchone()
    cur.execute(f"SELECT id, md5(pad) FROM {table} ORDER BY id")
    return (summary, cur.fetchall())


def verify(p3cur, pgcur, context):
    for t in TABLES:
        got = table_digest(p3cur, t)
        want = table_digest(pgcur, t)
        if got != want:
            print(f"DIVERGENCE in {t} {context}", file=sys.stderr)
            print(f"  pos3ql summary:   {got[0]}", file=sys.stderr)
            print(f"  postgres summary: {want[0]}", file=sys.stderr)
            g, w = dict(got[1]), dict(want[1])
            missing = sorted(set(w) - set(g))[:10]
            extra = sorted(set(g) - set(w))[:10]
            changed = sorted(k for k in set(g) & set(w) if g[k] != w[k])[:10]
            print(f"  missing ids: {missing}", file=sys.stderr)
            print(f"  extra ids:   {extra}", file=sys.stderr)
            print(f"  changed ids: {changed}", file=sys.stderr)
            return False
    return True


def run(cur_pair, sql, params=None):
    """Run on pos3ql first; mirror to the reference only once acked."""
    p3, pg = cur_pair
    p3.execute(sql, params)
    pg.execute(sql, params)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", type=int, default=20260723)
    ap.add_argument("--rounds", type=int, default=12)
    args = ap.parse_args()
    rng = random.Random(args.seed)

    port = int(os.environ["P3_PORT"])
    server = P3Server(
        os.environ["P3_BIN"],
        os.environ["P3_CONF"],
        port,
        os.environ["P3_DATADIR"],
        os.environ.get("P3_LOG", "/tmp/p3-torture.log"),
    )
    # run.sh already started the server; adopt it by connecting (the first
    # kill below targets whatever pid holds the port).
    p3 = connect_p3(port)
    pg = connect_pg()
    p3c, pgc = p3.cursor(), pg.cursor()

    for t in TABLES:
        for cur in (p3c, pgc):
            cur.execute(f"DROP TABLE IF EXISTS {t}")
            cur.execute(f"CREATE TABLE {t} (id serial PRIMARY KEY, pad text)")

    next_marker = [0]

    def random_batch():
        for _ in range(rng.randrange(3, 9)):
            t = rng.choice(TABLES)
            op = rng.random()
            if op < 0.55:
                n = rng.randrange(50, 400)
                width = rng.choice([40, 200, 1024])
                run(
                    (p3c, pgc),
                    f"INSERT INTO {t}(pad) SELECT repeat(chr(97 + (g % 17)), {width}) "
                    f"FROM generate_series(1, {n}) g",
                )
            elif op < 0.75:
                lo = rng.randrange(1, 30000)
                hi = lo + rng.randrange(1, 800)
                marker = next_marker[0]
                next_marker[0] += 1
                run(
                    (p3c, pgc),
                    f"UPDATE {t} SET pad = 'u{marker}-' || length(pad) "
                    f"WHERE id BETWEEN {lo} AND {hi}",
                )
            elif op < 0.93:
                lo = rng.randrange(1, 30000)
                hi = lo + rng.randrange(1, 600)
                run((p3c, pgc), f"DELETE FROM {t} WHERE id BETWEEN {lo} AND {hi}")
            else:
                run((p3c, pgc), f"TRUNCATE {t}")

    total_kills = 0
    total_cold = 0
    for rnd in range(args.rounds):
        random_batch()
        # Sometimes force a checkpoint (beyond the automatic ones).
        if rng.random() < 0.6:
            p3c.execute("CHECKPOINT")
        if not verify(p3c, pgc, f"(round {rnd}, live)"):
            sys.exit(1)

        action = rng.random()
        if action < 0.45:
            continue  # no restart this round
        cold = action >= 0.8
        if cold:
            # Everything must be in the bucket before the disk is wiped: a
            # checkpoint publishes the manifest synchronously and prunes the
            # WAL, so nothing depends on the asynchronous segment upload.
            p3c.execute("CHECKPOINT")
        # Kill the process holding the port (round 0: the run.sh-started one).
        subprocess.run(
            f"lsof -ti tcp:{port} -sTCP:LISTEN | xargs kill -9 2>/dev/null",
            shell=True,
            check=False,
        )
        if server.proc:
            server.proc.wait()
            server.proc = None
        total_kills += 1
        if cold:
            total_cold += 1
            server.wipe_data()
        # The killed listener's port can linger for a moment; binding into it
        # fails the whole run, so wait until it is genuinely free.
        deadline = time.time() + 10
        while time.time() < deadline:
            probe = subprocess.run(
                f"lsof -ti tcp:{port} -sTCP:LISTEN", shell=True, capture_output=True
            )
            if not probe.stdout.strip():
                break
            time.sleep(0.1)
        server.start()
        p3 = connect_p3(port)
        p3c = p3.cursor()
        kind = "cold start" if cold else "kill -9 restart"
        if not verify(p3c, pgc, f"(round {rnd}, after {kind})"):
            sys.exit(1)

    print(
        f"TORTURE OK: seed={args.seed} rounds={args.rounds} "
        f"kills={total_kills} cold_starts={total_cold}"
    )


if __name__ == "__main__":
    main()
