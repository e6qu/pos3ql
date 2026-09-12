#!/usr/bin/env python3
"""Run pinned PostgreSQL regression SQL against PostgreSQL and pos3ql.

The upstream files stay unmodified under vendor/.  This runner splits their
SQL at lexical boundaries, executes each statement against both engines, and
compares SQLSTATE, command status, result shape, and values.  Results without
an outer ORDER BY compare as multisets because SQL does not promise heap order.
"""

import argparse
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from result_diff import has_outer_order_by, rows_key

try:
    import psycopg
except ImportError:
    psycopg = None


def split_sql(source):
    """Yield (first_line, statement, copy_data) at SQL/psql lexical boundaries."""
    statements = []
    buffer = []
    state = "normal"
    dollar = ""
    block_depth = 0
    line = 1
    first_line = 1
    has_code = False
    index = 0

    def starts_dollar(offset):
        match = re.match(r"\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$", source[offset:])
        return match.group(0) if match else None

    while index < len(source):
        char = source[index]
        following = source[index + 1] if index + 1 < len(source) else ""
        if state == "normal":
            if char == "'":
                state = "single"
                has_code = True
                buffer.append(char)
            elif char == '"':
                state = "double"
                has_code = True
                buffer.append(char)
            elif char == "-" and following == "-":
                state = "line_comment"
                buffer.extend((char, following))
                index += 1
            elif char == "/" and following == "*":
                state = "block_comment"
                block_depth = 1
                buffer.extend((char, following))
                index += 1
            elif char == "$" and (tag := starts_dollar(index)) is not None:
                state = "dollar"
                dollar = tag
                has_code = True
                buffer.append(tag)
                index += len(tag) - 1
            elif char == ";":
                buffer.append(char)
                statement = "".join(buffer).strip()
                if has_code:
                    copy_data = None
                    if re.search(r"\bCOPY\b[\s\S]*\bFROM\s+stdin\s*;\s*$", statement,
                                 re.IGNORECASE):
                        data_start = index + 1
                        if source.startswith("\r\n", data_start):
                            data_start += 2
                        elif source.startswith("\n", data_start):
                            data_start += 1
                        terminator = re.search(r"(?m)^\\\.\s*(?:\r?\n|$)", source[data_start:])
                        if terminator is None:
                            raise ValueError(f"unterminated COPY data at line {first_line}")
                        copy_data = source[data_start:data_start + terminator.start()]
                        consumed_end = data_start + terminator.end()
                        line += source[index + 1:consumed_end].count("\n")
                        index = consumed_end - 1
                    statements.append((first_line, statement, copy_data))
                buffer.clear()
                first_line = line
                has_code = False
            else:
                buffer.append(char)
                if not char.isspace():
                    has_code = True
        elif state == "single":
            buffer.append(char)
            if char == "'":
                if following == "'":
                    buffer.append(following)
                    index += 1
                else:
                    state = "normal"
        elif state == "double":
            buffer.append(char)
            if char == '"':
                if following == '"':
                    buffer.append(following)
                    index += 1
                else:
                    state = "normal"
        elif state == "dollar":
            if source.startswith(dollar, index):
                buffer.append(dollar)
                index += len(dollar) - 1
                state = "normal"
            else:
                buffer.append(char)
        elif state == "line_comment":
            buffer.append(char)
            if char == "\n":
                state = "normal"
        else:
            buffer.append(char)
            if char == "/" and following == "*":
                buffer.append(following)
                index += 1
                block_depth += 1
            elif char == "*" and following == "/":
                buffer.append(following)
                index += 1
                block_depth -= 1
                if block_depth == 0:
                    state = "normal"
        if char == "\n":
            line += 1
            if not buffer or not "".join(buffer).strip():
                first_line = line
        index += 1

    if state not in {"normal", "line_comment"}:
        raise ValueError(f"unterminated {state} at end of SQL input")
    trailing = "".join(buffer).strip()
    if has_code:
        statements.append((first_line, trailing, None))
    return statements


def read_upstream(path, first_line, last_line):
    lines = pathlib.Path(path).read_text(encoding="utf-8").splitlines(keepends=True)
    if first_line < 1 or (last_line and last_line < first_line):
        raise ValueError(f"invalid source range {first_line}..{last_line} for {path}")
    if last_line:
        lines = lines[:last_line]
    lines = lines[first_line - 1:]
    # Preserve upstream line numbers in diagnostics even when the executable
    # manifest deliberately starts at a later statement boundary.
    kept = ["\n"] * (first_line - 1)
    in_copy = False
    for number, line in enumerate(lines, first_line):
        if in_copy:
            kept.append(line)
            if line.strip() == "\\.":
                in_copy = False
            continue
        if re.search(r"\bCOPY\b[\s\S]*\bFROM\s+stdin\s*;\s*$", line,
                     re.IGNORECASE):
            in_copy = True
            kept.append(line)
            continue
        if line.lstrip().startswith("\\"):
            command = line.lstrip().split(None, 1)[0]
            # These only change psql's display or describe a relation; neither
            # changes the SQL session exercised by the differential runner.
            if command not in {"\\pset", "\\x", "\\d"}:
                raise ValueError(f"{path}:{number}: unsupported psql command {command}")
            kept.append("\n")
        else:
            kept.append(line)
    return "".join(kept)


def run_one(cursor, sql, copy_data=None):
    try:
        if copy_data is None:
            cursor.execute(sql)
        else:
            with cursor.copy(sql) as copy:
                copy.write(copy_data.encode())
        status = cursor.statusmessage
        if cursor.description is None:
            return ("ok", status, None, None)
        names = tuple(column.name for column in cursor.description)
        return ("ok", status, names, cursor.fetchall())
    except Exception as error:
        state = getattr(error, "sqlstate", None) or "XXXXX"
        return ("err", state, str(error))


def results_match(postgres, pos3ql, sql):
    if postgres[0] == "err" or pos3ql[0] == "err":
        return postgres[0] == pos3ql[0] and postgres[1] == pos3ql[1]
    if postgres[1] != pos3ql[1] or postgres[2] != pos3ql[2]:
        return False
    ordered = has_outer_order_by(sql)
    return rows_key(postgres[3], ordered) == rows_key(pos3ql[3], ordered)


def summarize(result):
    if result[0] == "err":
        return f"ERROR {result[1]} {result[2].replace(chr(10), ' ')[:180]}"
    if result[3] is None:
        return result[1]
    return f"{result[1]} columns={result[2]!r} rows={result[3][:4]!r}"


def manifest_entries(path):
    entries = []
    ranges = {}
    for number, raw in enumerate(pathlib.Path(path).read_text().splitlines(), 1):
        if not raw or raw.startswith("#"):
            continue
        try:
            filename, first_line, last_line = raw.split("\t")
            first_line = int(first_line)
            last_line = int(last_line)
        except ValueError as error:
            raise ValueError(
                f"{path}:{number}: expected PATH<TAB>FIRST_LINE<TAB>LAST_LINE"
            ) from error
        if first_line < 1 or (last_line and last_line < first_line):
            raise ValueError(f"{path}:{number}: invalid range {first_line}..{last_line}")
        end = last_line or sys.maxsize
        for prior_start, prior_end in ranges.setdefault(filename, []):
            if first_line <= prior_end and prior_start <= end:
                raise ValueError(f"{path}:{number}: overlapping range for {filename}")
        ranges[filename].append((first_line, end))
        entries.append((filename, first_line, last_line))
    return entries


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--pg", type=int, required=True)
    parser.add_argument("--p3", type=int, required=True)
    parser.add_argument("--setup", required=True)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--max-print", type=int, default=30)
    args = parser.parse_args()
    if psycopg is None:
        print("psycopg is required", file=sys.stderr)
        return 2

    pg_conn = psycopg.connect(host="127.0.0.1", port=args.pg, user="postgres",
                              dbname="postgres", autocommit=True)
    p3_conn = psycopg.connect(host="127.0.0.1", port=args.p3, user="postgres",
                              dbname="postgres", autocommit=True)
    pg = pg_conn.cursor()
    p3 = p3_conn.cursor()
    sources = [(args.setup, 1, 0, False)] + [
        (*entry, True) for entry in manifest_entries(args.manifest)
    ]
    setup_total = 0
    upstream_total = 0
    mismatches = 0
    printed = 0
    try:
        for filename, first_line, last_line, upstream in sources:
            source = read_upstream(filename, first_line, last_line)
            for line, sql, copy_data in split_sql(source):
                if upstream:
                    upstream_total += 1
                else:
                    setup_total += 1
                pg_result = run_one(pg, sql, copy_data)
                p3_result = run_one(p3, sql, copy_data)
                if results_match(pg_result, p3_result, sql):
                    continue
                mismatches += 1
                if printed < args.max_print:
                    printed += 1
                    print(f"MISMATCH {filename}:{line}\nSQL: {sql[:800]}")
                    print(f"  PostgreSQL: {summarize(pg_result)}")
                    print(f"  pos3ql:     {summarize(p3_result)}")
    finally:
        pg_conn.close()
        p3_conn.close()
    print(
        f"TOTAL: {upstream_total} upstream statements + "
        f"{setup_total} setup statements  mismatches={mismatches}"
    )
    return 1 if mismatches else 0


if __name__ == "__main__":
    raise SystemExit(main())
