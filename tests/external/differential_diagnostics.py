#!/usr/bin/env python3
"""Compare psql data verbatim and diagnostics of every severity with their SQL source positions.

`sqlstate` compares each diagnostic's severity and SQLSTATE, `verbose` adds
its primary message and COPY context, and `full` adds every DETAIL, HINT,
QUERY and CONTEXT line and the error cursor (psql's `LINE n:` excerpt and
its caret line, which place the reported position in the statement)."""

import argparse
import json
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from sql_script import split_sql


START = re.compile(
    r"^(?:psql:(.+?):([0-9]+): )?(ERROR|WARNING|NOTICE|INFO|LOG|FATAL|PANIC|DEBUG[0-9]*):[ \t]*(.*)$"
)
AUXILIARY = re.compile(
    r"^(DETAIL|HINT|QUERY|CONTEXT|LOCATION|SCHEMA NAME|TABLE NAME|COLUMN NAME|"
    r"CONSTRAINT NAME|DATATYPE NAME):|^LINE [0-9]+:"
)
# The auxiliary fields `full` compares, each with its continuation lines.
FULL_FIELDS = ("detail", "hint", "query", "context", "cursor")
# CREATE TABLE repeating a constraint name fails pg_constraint's unique index,
# whose DETAIL names the new table's OID. Each server draws that OID from its
# own counter, so only that one number is masked, and only in that DETAIL.
CATALOG_NAME_KEY = (
    'duplicate key value violates unique constraint "pg_constraint_conrelid_contypid_conname_index"',
    re.compile(r"^(Key \(conrelid, contypid, conname\)=\()[0-9]+(, 0, )"),
)


def mask_server_oids(message, name, text):
    if name == "detail" and message == CATALOG_NAME_KEY[0]:
        masked, count = CATALOG_NAME_KEY[1].subn(r"\1<relation oid>\2", text)
        if count != 1:
            raise ValueError(f"catalog unique violation DETAIL has no relation OID: {text}")
        return masked
    return text


def copy_line_positions(source, filename):
    # psql can report a COPY error before or after draining its input. Associate
    # either position with this command, retaining every other source position.
    return {
        (str(filename), line): span.command_end_line
        for span in split_sql(source).copy_inputs
        for line in range(span.command_end_line, span.data_end_line + 1)
    }


def normalize_diagnostics(stderr, diagnostics, copy_positions, *, input_diagnostics):
    if diagnostics not in ("sqlstate", "verbose", "full"):
        raise ValueError(f"unknown corpus diagnostics {diagnostics}")
    if input_diagnostics not in ("sqlstate", "verbose"):
        raise ValueError(f"unknown input diagnostics {input_diagnostics}")
    if diagnostics != "sqlstate" and input_diagnostics != "verbose":
        raise ValueError(f"{diagnostics} comparison requires verbose input diagnostics")
    result = []
    current = None
    # The auxiliary field the current line continues, or None for the
    # primary message.
    field = None

    def finish():
        if current is None:
            return
        record = {key: value for key, value in current.items()
                  if key not in ("message", "fields")}
        if diagnostics == "verbose":
            record["message"] = "\n".join(current["message"])
            # A COPY line names the input line, column and value an error is about.
            copy_context = [line for line in current["fields"].get("context", [])
                            if line.startswith("COPY ")]
            if copy_context:
                record["copy_context"] = copy_context
        elif diagnostics == "full":
            record["message"] = "\n".join(current["message"])
            for name in FULL_FIELDS:
                if name in current["fields"]:
                    record[name] = mask_server_oids(
                        record["message"], name, "\n".join(current["fields"][name]))
        result.append(record)

    lines = stderr.split("\n")
    if lines[-1] == "":
        lines.pop()
    for line in lines:
        start = START.match(line)
        if start and (start[1] is not None or current is None or current["source"] is None):
            finish()
            source, number, severity, body = start.groups()
            # Notices and warnings carry an SQLSTATE like errors do, so every
            # severity is compared with the same rules.
            if input_diagnostics == "sqlstate":
                state = re.fullmatch(r"([0-9A-Z]{5})", body)
                message = []
            else:
                state = re.fullmatch(r"([0-9A-Z]{5}): (.*)", body)
                message = [state[2]] if state else []
            if state is None:
                raise ValueError(f"malformed {input_diagnostics} diagnostic: {line}")
            current = {
                "source": source,
                "line": (copy_positions.get((source, int(number)), int(number))
                         if number is not None else None),
                "severity": severity,
                "sqlstate": state[1],
                "message": message,
                "fields": {},
            }
            field = None
        elif line.startswith("psql:") or current is None:
            finish()
            current = None
            result.append({"client": line})
        elif auxiliary := AUXILIARY.match(line):
            # A cursor's `LINE n:` excerpt and its caret line are kept
            # verbatim: the caret's column is the reported position.
            field = auxiliary[1].lower() if auxiliary[1] else "cursor"
            if field == "cursor":
                current["fields"].setdefault(field, []).append(line)
            elif field in FULL_FIELDS:
                current["fields"].setdefault(field, []).append(
                    line[len(auxiliary[0]):].lstrip(" "))
            if field == "location":
                finish()
                current = None
        elif field is None:
            current["message"].append(line)
        elif field in FULL_FIELDS:
            current["fields"][field].append(line)
    finish()
    return result


CURSOR_GAP_KINDS = ("missing", "wrong", "extra")


def read_cursor_gaps(text):
    """The statements whose error cursor pos3ql does not report as PostgreSQL
    does, one `fixture.sql:line kind` per line (`#` starts a comment): kind
    `missing` where PostgreSQL positions an error and pos3ql does not,
    `wrong` where both position it differently, `extra` where only pos3ql
    does."""
    gaps = {}
    for number, line in enumerate(text.split("\n"), 1):
        entry = line.split("#", 1)[0].strip()
        if not entry:
            continue
        match = re.fullmatch(r"([0-9]+_[A-Za-z0-9_]+\.sql):([0-9]+) +(missing|wrong|extra)", entry)
        if match is None:
            raise ValueError(f"malformed cursor gap on line {number}: {line}")
        key = (match[1], int(match[2]))
        if key in gaps:
            raise ValueError(f"repeated cursor gap on line {number}: {line}")
        gaps[key] = match[3]
    return gaps


def apply_cursor_gaps(records, gaps, source, role):
    """Removes the cursors a listed statement of `source` reports, so the
    servers match only by showing the listed gap. A reference that does not
    show it makes the entry stale and fails; a candidate that does not is
    marked, so the comparison fails and the entry must go."""
    if role not in ("reference", "candidate"):
        raise ValueError(f"unknown cursor gap role {role}")
    name = Path(source).name
    listed = {line: kind for (fixture, line), kind in gaps.items() if fixture == name}
    if not listed:
        return records
    positioned = {}
    for record in records:
        line = record.get("line")
        if line in listed:
            positioned[line] = positioned.get(line, False) or "cursor" in record
    reference_positions = {"missing": True, "wrong": True, "extra": False}
    candidate_positions = {"missing": False, "wrong": True, "extra": True}
    if role == "reference":
        stale = sorted(line for line, kind in listed.items()
                       if positioned.get(line, False) != reference_positions[kind])
        if stale:
            raise ValueError(f"cursor gaps of {name} do not match PostgreSQL's cursors on lines "
                             + ", ".join(str(line) for line in stale))
    for record in records:
        line = record.get("line")
        if line not in listed:
            continue
        if role == "candidate" and positioned.get(line, False) != candidate_positions[listed[line]]:
            record["cursor"] = f"the {listed[line]} cursor gap is no longer shown"
        elif "cursor" in record:
            del record["cursor"]
    return records


def write_transcript(stdout, byte_count, stderr, diagnostics, copy_positions, output, *,
                     input_diagnostics, cursor_gaps=None, source=None, role=None):
    records = normalize_diagnostics(stderr, diagnostics, copy_positions,
                                    input_diagnostics=input_diagnostics)
    if cursor_gaps is not None:
        records = apply_cursor_gaps(records, cursor_gaps, source, role)
    # Length framing keeps data that resembles these markers unambiguous.
    output.write(f"STDOUT {byte_count} bytes\n".encode())
    copied = 0
    while chunk := stdout.read(64 * 1024):
        output.write(chunk)
        copied += len(chunk)
    if copied != byte_count:
        raise ValueError("corpus stdout changed while capturing its transcript")
    output.write(b"\nDIAGNOSTICS\n")
    for record in records:
        output.write((json.dumps(record, ensure_ascii=True) + "\n").encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True)
    parser.add_argument("--stdout", type=Path, required=True)
    parser.add_argument("--stderr", type=Path, required=True)
    parser.add_argument("--diagnostics", choices=("sqlstate", "verbose", "full"), required=True)
    parser.add_argument("--input-diagnostics", choices=("sqlstate", "verbose"), required=True)
    parser.add_argument("--cursor-gaps", type=Path,
                        help="statements whose error cursor pos3ql does not report yet")
    parser.add_argument("--role", choices=("reference", "candidate"),
                        help="which server's transcript this is, with --cursor-gaps")
    arguments = parser.parse_args()
    if (arguments.cursor_gaps is None) != (arguments.role is None):
        parser.exit(2, "FAIL: --cursor-gaps and --role go together\n")
    try:
        with arguments.stdout.open("rb") as stdout:
            write_transcript(
                stdout,
                arguments.stdout.stat().st_size,
                arguments.stderr.read_bytes().decode("utf-8", errors="surrogateescape"),
                arguments.diagnostics,
                copy_line_positions(
                    Path(arguments.source).read_bytes().decode("utf-8", errors="surrogateescape"),
                    arguments.source,
                ),
                sys.stdout.buffer,
                input_diagnostics=arguments.input_diagnostics,
                cursor_gaps=(read_cursor_gaps(arguments.cursor_gaps.read_text())
                             if arguments.cursor_gaps is not None else None),
                source=arguments.source,
                role=arguments.role,
            )
    except (OSError, ValueError) as error:
        parser.exit(1, f"FAIL: corpus transcript: {error}\n")


if __name__ == "__main__":
    main()
