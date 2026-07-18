#!/usr/bin/env python3
"""Differential replay of sqllogictest corpora: real PostgreSQL vs pos3ql.

sqllogictest .test files are used purely as a *source of SQL statements*.
For each statement/query we run it on BOTH engines over persistent
connections and compare their behaviour — the embedded SQLite result
hashes are ignored (PostgreSQL is the oracle). Every block is bucketed:

  match          both engines agree (rows equal, or both error same SQLSTATE)
  unsupported    pos3ql raises a "not implemented / syntax" class error
                 (0A000/42601/42P01/42883/42704/…) where PostgreSQL succeeds
                 — an honest subset gap, not a bug
  DIVERGENCE     both succeed but results differ, or only one errors, or the
                 SQLSTATEs differ — the interesting failures

Exit code is nonzero iff any DIVERGENCE survived, so this gates CI once the
divergence budget is zero. Divergences print the file, SQL, and both
outcomes for a direct repro.

Usage:
  slt_diff.py --pg PORT --p3 PORT [--limit N] [--max-print K] FILE...
"""
import argparse
import sys

try:
    import psycopg
except ImportError:
    print("psycopg not installed; skipping slt_diff", file=sys.stderr)
    sys.exit(0)

# pos3ql SQLSTATEs that mean "this is outside our implemented subset",
# not "your query is wrong". A PostgreSQL success against one of these is a
# documented gap, not a divergence.
UNSUPPORTED_STATES = {
    "0A000",  # feature_not_supported
    "42601",  # syntax_error (grammar we don't parse yet)
    "42P01",  # undefined_table (cascades from an earlier unsupported CREATE)
    "42883",  # undefined_function
    "42704",  # undefined_object (e.g. a type we lack)
    "42846",  # cannot_coerce
    "54000",  # program_limit_exceeded (fixed arenas)
    "22023",  # invalid_parameter_value (some unsupported options)
}


class Block:
    __slots__ = ("kind", "sql", "expect_error")

    def __init__(self, kind, sql, expect_error):
        self.kind = kind          # "statement" | "query"
        self.sql = sql
        self.expect_error = expect_error  # sqllogictest said "statement error"


# The dialect we present as: pos3ql speaks PostgreSQL, and PostgreSQL is
# the oracle, so `onlyif postgresql` blocks run and other-dialect blocks
# are skipped.
DIALECT = "postgresql"


def parse(path):
    """Yields Block objects from a .test file, honoring sqllogictest's
    onlyif/skipif/halt conditional directives for our dialect."""
    with open(path, "r", errors="replace") as f:
        lines = f.read().split("\n")
    i = 0
    n = len(lines)
    # A pending condition set by onlyif/skipif governs the next record.
    pending_skip = False
    while i < n:
        line = lines[i].strip()
        if not line or line.startswith("#"):
            i += 1
            continue
        parts = line.split()
        head = parts[0]

        if head in ("onlyif", "skipif"):
            db = parts[1] if len(parts) > 1 else ""
            if head == "onlyif":
                pending_skip = db != DIALECT
            else:  # skipif
                pending_skip = db == DIALECT
            i += 1
            continue

        skip_this = pending_skip
        pending_skip = False

        if head == "halt":
            # A conditional halt that does not apply to us is a no-op.
            if skip_this:
                i += 1
                continue
            break
        if head == "statement":
            expect_error = len(parts) > 1 and parts[1] == "error"
            i += 1
            sql = []
            while i < n and lines[i].strip() != "":
                sql.append(lines[i])
                i += 1
            if not skip_this:
                yield Block("statement", "\n".join(sql), expect_error)
        elif head == "query":
            i += 1
            sql = []
            while i < n and lines[i].strip() != "----":
                sql.append(lines[i])
                i += 1
            # Skip the ---- separator and the embedded expected results
            # (we do not trust SQLite's oracle).
            while i < n and lines[i].strip() != "":
                i += 1
            if not skip_this:
                yield Block("query", "\n".join(sql), False)
        else:
            # hash-threshold and other directives: skip the line.
            i += 1


_EXAMPLES = {}


def _stash_example(key, sql):
    if key not in _EXAMPLES:
        _EXAMPLES[key] = " ".join(sql.split())[:160]


def run_one(cur, sql):
    """Executes sql, returns ('ok', rows) or ('err', sqlstate)."""
    try:
        cur.execute(sql)
        try:
            rows = cur.fetchall()
        except psycopg.ProgrammingError:
            rows = None  # non-SELECT
        return ("ok", rows)
    except psycopg.Error as e:
        state = getattr(getattr(e, "diag", None), "sqlstate", None) or "?????"
        msg = getattr(getattr(e, "diag", None), "message_primary", None) or str(e)
        return ("err", state, msg.strip().replace("\n", " ")[:90])


def _cell(c):
    """Stringify a result cell so result sets compare structurally across
    engines; floats are rounded to absorb the last-ULP differences."""
    if isinstance(c, float):
        return "f:%.9g" % c
    if isinstance(c, bool):
        return "b:%d" % c
    # psycopg hands back raw bytes for text columns when the server encoding is
    # SQL_ASCII (it will not guess a decoding); a UTF8 server yields str for the
    # identical data. Decode losslessly so the comparison keys the value, not the
    # server's encoding. latin1 is a total 1:1 map, so real bytea compares by
    # content on both engines too.
    if isinstance(c, (bytes, bytearray, memoryview)):
        return "s:" + bytes(c).decode("latin1")
    return "s:" + str(c)


def normalize_rows(rows):
    """Order-insensitive, type-loose comparison key for a result set."""
    if rows is None:
        return None
    out = []
    for r in rows:
        out.append(tuple(_cell(c) for c in r))
    out.sort()
    return out


def table_names(sql):
    """Best-effort: names created by a CREATE TABLE, for pre-file cleanup."""
    low = sql.lower()
    names = []
    idx = 0
    while True:
        p = low.find("create table", idx)
        if p < 0:
            break
        rest = sql[p + len("create table"):].strip()
        if rest.lower().startswith("if not exists"):
            rest = rest[len("if not exists"):].strip()
        name = ""
        for ch in rest:
            if ch.isalnum() or ch == "_":
                name += ch
            else:
                break
        if name:
            names.append(name)
        idx = p + 1
    return names


def process_file(path, pg, p3, limit, divergences, max_print, unsupp_hist):
    stats = {"match": 0, "unsupported": 0, "divergence": 0, "blocks": 0}
    blocks = list(parse(path))

    # Isolate this file: drop any tables it will (re)create on both engines.
    created = set()
    for b in blocks:
        created.update(table_names(b.sql))
    for name in created:
        for cur in (pg, p3):
            try:
                cur.execute(f"DROP TABLE IF EXISTS {name}")
            except psycopg.Error:
                pass

    for b in blocks:
        if limit and stats["blocks"] >= limit:
            break
        stats["blocks"] += 1
        pg_res = run_one(pg, b.sql)
        p3_res = run_one(p3, b.sql)

        # Both error: agree if SQLSTATE matches, else divergence — but a
        # pos3ql unsupported-class error where PG has a different (real)
        # error is treated as unsupported, not divergence.
        if pg_res[0] == "err" and p3_res[0] == "err":
            if pg_res[1] == p3_res[1]:
                stats["match"] += 1
            elif p3_res[1] in UNSUPPORTED_STATES:
                stats["unsupported"] += 1
                key = f"{p3_res[1]} {p3_res[2] if len(p3_res) > 2 else ''}"
                unsupp_hist[key] = unsupp_hist.get(key, 0) + 1
                _stash_example(key, b.sql)
            else:
                stats["divergence"] += 1
                record(divergences, path, b, pg_res, p3_res, max_print)
            continue

        # PG ok, pos3ql errored.
        if pg_res[0] == "ok" and p3_res[0] == "err":
            if p3_res[1] in UNSUPPORTED_STATES:
                stats["unsupported"] += 1
                key = f"{p3_res[1]} {p3_res[2] if len(p3_res) > 2 else ''}"
                unsupp_hist[key] = unsupp_hist.get(key, 0) + 1
                _stash_example(key, b.sql)
            else:
                stats["divergence"] += 1
                record(divergences, path, b, pg_res, p3_res, max_print)
            continue

        # PG errored, pos3ql ok: we are too lenient — a real divergence.
        if pg_res[0] == "err" and p3_res[0] == "ok":
            stats["divergence"] += 1
            record(divergences, path, b, pg_res, p3_res, max_print)
            continue

        # Both ok: compare result sets.
        if normalize_rows(pg_res[1]) == normalize_rows(p3_res[1]):
            stats["match"] += 1
        else:
            stats["divergence"] += 1
            record(divergences, path, b, pg_res, p3_res, max_print)
    return stats


def record(divergences, path, b, pg_res, p3_res, max_print):
    if len(divergences) < max_print:
        divergences.append(
            f"--- DIVERGENCE in {path} ---\n"
            f"SQL: {b.sql.strip()[:400]}\n"
            f"  PostgreSQL: {summarize(pg_res)}\n"
            f"  pos3ql    : {summarize(p3_res)}"
        )


def summarize(res):
    if res[0] == "err":
        return f"ERROR {res[1]}"
    rows = res[1]
    if rows is None:
        return "ok (no rows)"
    return f"{len(rows)} rows e.g. {rows[:3]}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pg", type=int, required=True)
    ap.add_argument("--p3", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--limit", type=int, default=0, help="max blocks per file")
    ap.add_argument("--max-print", type=int, default=25)
    ap.add_argument("--stmt-timeout-ms", type=int, default=60000,
                    help="per-statement timeout; 0 disables")
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()

    pg_conn = psycopg.connect(host=args.host, port=args.pg, user="postgres",
                              autocommit=True, sslmode="disable")
    p3_conn = psycopg.connect(host=args.host, port=args.p3, user="postgres",
                              autocommit=True, sslmode="disable")
    pg = pg_conn.cursor()
    p3 = p3_conn.cursor()
    # Pin the session time zone so timestamptz rendering matches across engines
    # regardless of the reference server's host zone (pos3ql renders in UTC).
    for c in (pg, p3):
        c.execute("SET TimeZone='UTC'")
    # Cap any single statement so a pathological query cannot wedge the run.
    # An engine that does not enforce statement_timeout says so loudly; we report
    # which one and fall back to the job-level timeout for that engine.
    if args.stmt_timeout_ms > 0:
        for name, c in (("pg", pg), ("p3", p3)):
            try:
                c.execute("SET statement_timeout = %d" % args.stmt_timeout_ms)
            except psycopg.Error as e:
                print(f"note: {name} does not enforce statement_timeout "
                      f"({e}); relying on job-level timeout for it")

    total = {"match": 0, "unsupported": 0, "divergence": 0, "blocks": 0}
    divergences = []
    unsupp_hist = {}
    for path in args.files:
        s = process_file(path, pg, p3, args.limit, divergences, args.max_print, unsupp_hist)
        for k in total:
            total[k] += s[k]
        print(f"  {path}: {s['blocks']} blocks  "
              f"match={s['match']} unsupported={s['unsupported']} "
              f"divergence={s['divergence']}")

    print()
    for d in divergences:
        print(d)
    if len(divergences) >= args.max_print:
        print(f"... (only the first {args.max_print} divergences shown)")

    if unsupp_hist:
        top = sorted(unsupp_hist.items(), key=lambda kv: -kv[1])
        print("unsupported breakdown:")
        for k, v in top:
            print(f"  {v:4d}  {k}")
            if k in _EXAMPLES:
                print(f"        e.g. {_EXAMPLES[k]}")
    print(f"\nTOTAL: {total['blocks']} blocks  match={total['match']}  "
          f"unsupported={total['unsupported']}  divergence={total['divergence']}")
    sys.exit(1 if total["divergence"] > 0 else 0)


if __name__ == "__main__":
    main()
