#!/usr/bin/env python3
"""Generative differential SQL fuzzer: real PostgreSQL vs pos3ql.

A seeded grammar produces valid-ish SQL over a fixed, well-typed schema and
runs each statement on both engines, comparing behaviour exactly as
slt_diff does (match / unsupported / DIVERGENCE). Because the corpus is
generated, coverage is effectively unbounded: `--count 100000 --seed 7`
explores a hundred thousand distinct statements, and any divergence prints
the seed + statement for a one-line repro.

The generator deliberately targets the seams where implementations drift:
three-valued logic and NULL propagation, integer overflow and division by
zero, operator precedence, mixed-type comparison and coercion, CASE, IN /
BETWEEN / LIKE, aggregates with GROUP BY / HAVING, ORDER BY with NULL
ordering, LIMIT/OFFSET, and datetime/text/boolean literals.

Everything is deterministic from `--seed`; the PRNG is stdlib random seeded
once, so a reported seed reproduces the exact sequence.

Usage:
  fuzz_diff.py --pg PORT --p3 PORT [--count N] [--seed S] [--max-print K]
"""
import argparse
import random
import sys

try:
    import psycopg
except ImportError:
    print("psycopg not installed; skipping fuzz_diff", file=sys.stderr)
    sys.exit(0)

UNSUPPORTED_STATES = {
    "0A000", "42601", "42883", "42704", "42846", "54000", "22023",
}

# Fixed schema the fuzzer queries. Columns span the type/nullability space.
SCHEMA = [
    "DROP TABLE IF EXISTS fz",
    "CREATE TABLE fz ("
    " id int NOT NULL,"
    " a int,"
    " b int,"
    " r float8,"
    " s text,"
    " flag bool,"
    " d date,"
    " ts timestamptz)",
]

ROWS = [
    "(1, 10, 3, 1.5, 'apple', true,  DATE '2020-01-01', TIMESTAMPTZ '2020-01-01 00:00:00+00')",
    "(2, -7, 0, -2.25,'banana',false, DATE '2021-06-15', TIMESTAMPTZ '2021-06-15 12:30:00+00')",
    "(3, NULL, 5, NULL, NULL,  NULL,  NULL,               NULL)",
    "(4, 2147483647, 1, 3.0, 'pear', true, DATE '1999-12-31', TIMESTAMPTZ '1999-12-31 23:59:59+00')",
    "(5, 0, -4, 0.0, '',     false, DATE '2000-01-01', TIMESTAMPTZ '2000-01-01 00:00:00+00')",
    "(6, 42, 42, 2.5, 'Apple',true,  DATE '2020-01-01', TIMESTAMPTZ '2020-01-01 06:00:00+00')",
]

INT_COLS = ["a", "b", "id"]
NUM_COLS = INT_COLS + ["r"]
TEXT_COLS = ["s"]
BOOL_COLS = ["flag"]
ALL_COLS = NUM_COLS + TEXT_COLS + BOOL_COLS + ["d", "ts"]


class Gen:
    def __init__(self, rng):
        self.rng = rng

    def choice(self, xs):
        return self.rng.choice(xs)

    def maybe(self, p=0.5):
        return self.rng.random() < p

    def int_lit(self):
        return str(self.choice([0, 1, -1, 2, 3, 5, 7, 42, -7, 100,
                                2147483647, -2147483648, 999999]))

    def num_expr(self, depth=0):
        if depth >= 3 or self.maybe(0.4):
            return self.choice([self.choice(NUM_COLS), self.int_lit(),
                                self.choice(["1.5", "0.0", "-2.25", "3.14"])])
        op = self.choice(["+", "-", "*", "/", "%"])
        # % only makes sense on ints; keep both sides int for it
        if op == "%":
            return f"({self.int_operand(depth)} % {self.int_operand(depth)})"
        return f"({self.num_expr(depth+1)} {op} {self.num_expr(depth+1)})"

    def int_operand(self, depth):
        if self.maybe(0.5):
            return self.choice(INT_COLS)
        return self.int_lit()

    def text_expr(self):
        if self.maybe(0.5):
            return self.choice(TEXT_COLS)
        return "'" + self.choice(["apple", "banana", "", "Apple", "x%y", "a_b"]) + "'"

    def predicate(self, depth=0):
        r = self.rng.random()
        if depth < 2 and r < 0.25:
            return f"({self.predicate(depth+1)} {self.choice(['AND','OR'])} {self.predicate(depth+1)})"
        if depth < 2 and r < 0.32:
            return f"NOT {self.predicate(depth+1)}"
        r = self.rng.random()
        if r < 0.35:
            op = self.choice(["=", "<>", "<", "<=", ">", ">="])
            return f"{self.num_expr()} {op} {self.num_expr()}"
        if r < 0.5:
            col = self.choice(NUM_COLS)
            return f"{col} BETWEEN {self.int_lit()} AND {self.int_lit()}"
        if r < 0.62:
            vals = ", ".join(self.int_lit() for _ in range(self.rng.randint(1, 4)))
            neg = "NOT " if self.maybe(0.3) else ""
            return f"{self.choice(INT_COLS)} {neg}IN ({vals})"
        if r < 0.74:
            neg = "NOT " if self.maybe(0.3) else ""
            pat = self.choice(["a%", "%e", "%an%", "Apple", "_pp%", "x\\%y"])
            op = self.choice(["LIKE", "ILIKE"])
            return f"{self.text_expr()} {neg}{op} '{pat}'"
        if r < 0.85:
            col = self.choice(ALL_COLS)
            return f"{col} IS {self.choice(['', 'NOT '])}NULL"
        if r < 0.93:
            return f"{self.choice(BOOL_COLS)}"
        return f"{self.text_expr()} {self.choice(['=', '<>', '<', '>'])} {self.text_expr()}"

    def small_int(self):
        return str(self.rng.randint(-3, 6))

    def str_func(self):
        """A string-valued (or string-derived) function over `s`; text results
        compare exactly, so a divergence is a real behavioural bug."""
        s = self.text_expr()
        return self.choice([
            f"upper({s})", f"lower({s})", f"initcap({s})",
            f"length({s})", f"char_length({s})", f"octet_length({s})",
            f"substring({s} FROM {self.small_int()})",
            f"substring({s} FROM {self.small_int()} FOR {self.small_int()})",
            f"left({s}, {self.small_int()})", f"right({s}, {self.small_int()})",
            f"trim({s})", f"ltrim({s})", f"rtrim({s})",
            f"btrim({s}, 'ae')", f"trim(both 'a' FROM {s})",
            f"replace({s}, 'a', 'X')", f"reverse({s})",
            f"repeat({s}, {self.rng.randint(0, 3)})",
            f"position('p' IN {s})", f"strpos({s}, 'a')",
            f"({s} || {self.text_expr()})",
            f"concat({s}, {self.text_expr()}, {self.int_lit()})",
            f"lpad({s}, {self.small_int()}, '*')", f"rpad({s}, {self.small_int()}, '*')",
            f"split_part({s}, 'a', {self.rng.randint(1, 3)})",
            f"to_char({self.num_expr()}, '999.99')",
            f"ascii({s})", f"chr(65 + {self.rng.randint(0, 25)})",
        ])

    def math_func(self):
        """A numeric function. sqrt/power/exp/ln take float8 (`r`) or integer
        arguments only: PostgreSQL computes those in NUMERIC for a numeric
        argument and returns numeric, which pos3ql does not yet do (tracked as
        a subset gap), so the fuzzer stays on the float/int forms it supports."""
        return self.choice([
            f"abs({self.num_expr()})",
            f"mod({self.int_operand(0)}, {self.int_operand(0)})",
            f"power(r, {self.small_int()})", f"power({self.small_int()}, r)",
            f"sqrt(abs(r))", f"exp(r)", f"ln(abs(r) + 1)",
            f"floor({self.num_expr()})", f"ceil({self.num_expr()})",
            f"round({self.num_expr()})", f"round({self.choice(['1.555','2.5','-2.5'])}, 1)",
            f"trunc({self.num_expr()})", f"sign({self.num_expr()})",
            f"greatest({self.num_expr()}, {self.num_expr()}, id)",
            f"least({self.num_expr()}, {self.num_expr()})",
            f"coalesce({self.choice(NUM_COLS)}, {self.int_lit()})",
            f"nullif({self.choice(INT_COLS)}, {self.int_lit()})",
            f"cast({self.int_lit()} AS float8)",
        ])

    def scalar_select_item(self):
        r = self.rng.random()
        if r < 0.30:
            return self.num_expr()
        if r < 0.42:
            return self.text_expr()
        if r < 0.55:
            return self.predicate()
        if r < 0.70:
            # CASE
            arms = []
            for _ in range(self.rng.randint(1, 3)):
                arms.append(f"WHEN {self.predicate()} THEN {self.num_expr()}")
            els = f" ELSE {self.num_expr()}" if self.maybe() else ""
            return f"CASE {' '.join(arms)}{els} END"
        if r < 0.85:
            return self.str_func()
        return self.math_func()

    def agg_item(self):
        fn = self.choice(["count", "sum", "avg", "min", "max"])
        if fn == "count" and self.maybe(0.4):
            return "count(*)"
        col = self.choice(NUM_COLS if fn in ("sum", "avg") else ALL_COLS)
        return f"{fn}({col})"

    def order_by(self, tiebreak=True):
        # Always end with the unique `id` so ordering is total; LIMIT without
        # a total order is unspecified in SQL and its row set would differ by
        # physical layout, not fidelity.
        n = self.rng.randint(1, 2)
        parts = []
        for _ in range(n):
            key = self.choice(ALL_COLS + ["1"])
            dirn = self.choice(["", " ASC", " DESC"])
            nulls = self.choice(["", " NULLS FIRST", " NULLS LAST"])
            parts.append(f"{key}{dirn}{nulls}")
        if tiebreak:
            parts.append("id")
        return "ORDER BY " + ", ".join(parts)

    def statement(self):
        """A random SELECT: plain projection or an aggregate/GROUP BY."""
        if self.maybe(0.3):
            # Aggregate / GROUP BY form.
            grp = self.choice(ALL_COLS)
            items = [grp] + [self.agg_item() for _ in range(self.rng.randint(1, 2))]
            sql = f"SELECT {', '.join(items)} FROM fz"
            if self.maybe(0.5):
                sql += f" WHERE {self.predicate()}"
            sql += f" GROUP BY {grp}"
            if self.maybe(0.3):
                sql += f" HAVING {self.agg_item()} > {self.int_lit()}"
            # Group results are one row per distinct group value; ORDER BY the
            # group key (unique per group) makes the order total.
            sql += " ORDER BY 1 NULLS LAST"
            return sql
        # Plain projection.
        n = self.rng.randint(1, 4)
        items = [self.scalar_select_item() for _ in range(n)]
        sql = f"SELECT {', '.join(items)} FROM fz"
        if self.maybe(0.7):
            sql += f" WHERE {self.predicate()}"
        use_limit = self.maybe(0.4)
        if use_limit or self.maybe(0.6):
            # A LIMIT needs a total order to be deterministic.
            sql += " " + self.order_by()
        if use_limit:
            sql += f" LIMIT {self.rng.randint(0, 6)}"
            if self.maybe(0.3):
                sql += f" OFFSET {self.rng.randint(0, 3)}"
        return sql


def run_one(cur, sql):
    try:
        cur.execute(sql)
        try:
            rows = cur.fetchall()
        except psycopg.ProgrammingError:
            rows = None
        return ("ok", rows)
    except psycopg.Error as e:
        state = getattr(getattr(e, "diag", None), "sqlstate", None) or "?????"
        return ("err", state)
    except Exception as e:
        # A decode error means the server's RowDescription type disagrees
        # with the value it sent — a real protocol bug, surfaced as a
        # divergence rather than crashing the harness.
        return ("decode_error", str(e)[:80])


def _cell(c):
    """Stringify a result cell so result sets compare structurally across
    engines; floats are rounded to absorb the last-ULP differences."""
    if isinstance(c, float):
        return "f:%.9g" % c
    if isinstance(c, bool):
        return "b:%d" % c
    # psycopg returns raw bytes for text columns under a SQL_ASCII server and str
    # under UTF8; decode losslessly (latin1 is a 1:1 map) so the comparison keys
    # the value, not the server's encoding.
    if isinstance(c, (bytes, bytearray, memoryview)):
        return "s:" + bytes(c).decode("latin1")
    return "s:" + str(c)


def key(rows):
    if rows is None:
        return None
    out = []
    for r in rows:
        # Uniformly stringify (floats rounded first) so mixed-type columns
        # across rows still sort and compare deterministically.
        out.append(tuple(_cell(c) for c in r))
    out.sort()
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pg", type=int, required=True)
    ap.add_argument("--p3", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--count", type=int, default=2000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--max-print", type=int, default=25)
    args = ap.parse_args()

    pg = psycopg.connect(host=args.host, port=args.pg, user="postgres",
                         autocommit=True, sslmode="disable").cursor()
    p3 = psycopg.connect(host=args.host, port=args.p3, user="postgres",
                         autocommit=True, sslmode="disable").cursor()

    # Pin the session time zone so timestamptz rendering is deterministic and
    # identical on both engines — otherwise the reference server's default zone
    # (the host's local zone) disagrees with pos3ql's UTC and every timestamptz
    # value reads as a spurious divergence.
    for cur in (pg, p3):
        cur.execute("SET TimeZone='UTC'")

    def setup(engine, cur, stmts):
        # Fail loudly and identifiably: a setup error (e.g. the target engine's
        # table catalog is full) must not surface as a bare traceback with no
        # divergence count.
        for stmt in stmts:
            try:
                cur.execute(stmt)
            except psycopg.Error as e:
                sys.stderr.write(f"fuzzer setup failed on {engine}: {e}\n"
                                 f"  SQL: {stmt}\n")
                sys.exit(2)

    values = "INSERT INTO fz VALUES " + ", ".join(ROWS)
    for engine, cur in (("PostgreSQL", pg), ("pos3ql", p3)):
        setup(engine, cur, SCHEMA)
        setup(engine, cur, [values])

    rng = random.Random(args.seed)
    gen = Gen(rng)
    stats = {"match": 0, "unsupported": 0, "divergence": 0}
    unsupp_hist = {}
    divergences = []

    for i in range(args.count):
        # Per-statement sub-seed so a divergence reproduces exactly.
        sub = rng.randint(0, 2**63)
        gen.rng = random.Random(sub)
        sql = gen.statement()
        pg_res = run_one(pg, sql)
        p3_res = run_one(p3, sql)

        verdict = classify(pg_res, p3_res)
        if verdict == "match":
            stats["match"] += 1
        elif verdict == "unsupported":
            stats["unsupported"] += 1
            unsupp_hist[p3_res[1]] = unsupp_hist.get(p3_res[1], 0) + 1
        else:
            stats["divergence"] += 1
            if len(divergences) < args.max_print:
                divergences.append(
                    f"--- DIVERGENCE (seed {args.seed}, stmt #{i}, substmt-seed {sub}) ---\n"
                    f"SQL: {sql}\n"
                    f"  PostgreSQL: {summarize(pg_res)}\n"
                    f"  pos3ql    : {summarize(p3_res)}"
                )

    for d in divergences:
        print(d)
    if unsupp_hist:
        top = sorted(unsupp_hist.items(), key=lambda kv: -kv[1])
        print("unsupported by SQLSTATE: " + ", ".join(f"{k}×{v}" for k, v in top))
    print(f"\nTOTAL: {args.count} statements (seed {args.seed})  "
          f"match={stats['match']}  unsupported={stats['unsupported']}  "
          f"divergence={stats['divergence']}")
    sys.exit(1 if stats["divergence"] > 0 else 0)


def classify(pg_res, p3_res):
    if pg_res[0] == "decode_error" or p3_res[0] == "decode_error":
        return "divergence"
    if pg_res[0] == "err" and p3_res[0] == "err":
        if pg_res[1] == p3_res[1]:
            return "match"
        return "unsupported" if p3_res[1] in UNSUPPORTED_STATES else "divergence"
    if pg_res[0] == "ok" and p3_res[0] == "err":
        return "unsupported" if p3_res[1] in UNSUPPORTED_STATES else "divergence"
    if pg_res[0] == "err" and p3_res[0] == "ok":
        return "divergence"
    return "match" if key(pg_res[1]) == key(p3_res[1]) else "divergence"


def summarize(res):
    if res[0] == "err":
        return f"ERROR {res[1]}"
    rows = res[1]
    if rows is None:
        return "ok (no rows)"
    return f"{len(rows)} rows e.g. {rows[:4]}"


if __name__ == "__main__":
    main()
