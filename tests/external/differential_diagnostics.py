#!/usr/bin/env python3
"""Compare psql data verbatim and diagnostics with their SQL source positions."""

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
    r"^(?:DETAIL|HINT|QUERY|CONTEXT|LOCATION|SCHEMA NAME|TABLE NAME|COLUMN NAME|"
    r"CONSTRAINT NAME|DATATYPE NAME):|^LINE [0-9]+:"
)


def copy_line_positions(source, filename):
    # psql can report a COPY error before or after draining its input. Associate
    # either position with this command, retaining every other source position.
    return {
        (str(filename), line): span.command_end_line
        for span in split_sql(source).copy_inputs
        for line in range(span.command_end_line, span.data_end_line + 1)
    }


def normalize_diagnostics(stderr, diagnostics, copy_positions):
    if diagnostics not in ("sqlstate", "verbose"):
        raise ValueError(f"unknown corpus diagnostics {diagnostics}")
    result = []
    current = None
    auxiliary = False

    def finish():
        if current is None or current["severity"] in ("NOTICE", "WARNING"):
            return
        record = {key: value for key, value in current.items() if key != "message"}
        if diagnostics == "verbose":
            record["message"] = "\n".join(current["message"])
        result.append(record)

    lines = stderr.split("\n")
    if lines[-1] == "":
        lines.pop()
    for line in lines:
        start = START.match(line)
        if start and (start[1] is not None or current is None or current["source"] is None):
            finish()
            source, number, severity, body = start.groups()
            if severity in ("NOTICE", "WARNING"):
                state = None
                message = []
            elif diagnostics == "sqlstate":
                state = re.fullmatch(r"([0-9A-Z]{5})", body)
                message = []
            else:
                state = re.fullmatch(r"([0-9A-Z]{5}): (.*)", body)
                message = [state[2]] if state else []
            if state is None and severity not in ("NOTICE", "WARNING"):
                raise ValueError(f"malformed {diagnostics} diagnostic: {line}")
            current = {
                "source": source,
                "line": (copy_positions.get((source, int(number)), int(number))
                         if number is not None else None),
                "severity": severity,
                "sqlstate": None if state is None else state[1],
                "message": message,
            }
            auxiliary = False
        elif line.startswith("psql:") or current is None:
            finish()
            current = None
            result.append({"client": line})
        elif AUXILIARY.match(line):
            auxiliary = True
            if line.startswith("LOCATION:"):
                finish()
                current = None
        elif not auxiliary:
            current["message"].append(line)
    finish()
    return result


def write_transcript(stdout, byte_count, stderr, diagnostics, copy_positions, output):
    records = normalize_diagnostics(stderr, diagnostics, copy_positions)
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
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--stdout", type=Path, required=True)
    parser.add_argument("--stderr", type=Path, required=True)
    parser.add_argument("--diagnostics", choices=("sqlstate", "verbose"), required=True)
    arguments = parser.parse_args()
    try:
        with arguments.stdout.open("rb") as stdout:
            write_transcript(
                stdout,
                arguments.stdout.stat().st_size,
                arguments.stderr.read_bytes().decode("utf-8", errors="surrogateescape"),
                arguments.diagnostics,
                copy_line_positions(
                    arguments.source.read_bytes().decode("utf-8", errors="surrogateescape"),
                    arguments.source,
                ),
                sys.stdout.buffer,
            )
    except (OSError, ValueError) as error:
        parser.exit(1, f"FAIL: corpus transcript: {error}\n")


if __name__ == "__main__":
    main()
