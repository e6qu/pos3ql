#!/usr/bin/env python3
"""Summarize fixed-memory checkpoint phase lines from one measured interval."""

import argparse
import json
import pathlib


PREFIX = "checkpoint_phase "
FIELDS = {"lsn", "phase", "slot", "elapsed_us", "block_gets", "block_puts", "deleted"}
MEASURES = ("elapsed_us", "block_gets", "block_puts", "deleted")


def parse(source, offset):
    events = []
    for line in source[offset:].decode("utf-8").splitlines():
        if not line.startswith(PREFIX):
            continue
        entries = [part.split("=", 1) for part in line[len(PREFIX):].split()]
        if any(len(entry) != 2 for entry in entries):
            raise ValueError(f"malformed checkpoint phase line: {line}")
        fields = dict(entries)
        if len(entries) != len(FIELDS) or set(fields) != FIELDS:
            raise ValueError(f"wrong checkpoint phase fields: {line}")
        event = {name: int(fields[name]) for name in FIELDS - {"phase"}}
        if any(event[name] < 0 for name in MEASURES) or event["lsn"] < 0:
            raise ValueError(f"negative checkpoint phase measurement: {line}")
        event["phase"] = fields["phase"]
        event["slot"] = None if event["slot"] == -1 else event["slot"]
        events.append(event)
    if not events:
        raise ValueError("no checkpoint phase lines in measured interval")
    totals = {}
    for event in events:
        phase = event["phase"]
        summary = totals.setdefault(phase, {"events": 0, **{name: 0 for name in MEASURES}})
        summary["events"] += 1
        for name in MEASURES:
            summary[name] += event[name]
    return {"schema_version": 1, "artifact_type": "checkpoint_profile",
            "source_byte_offset": offset, "events": events, "totals": totals}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("log", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--after-byte-offset", type=int, default=0)
    args = parser.parse_args()
    source = args.log.read_bytes()
    if args.after_byte_offset < 0 or args.after_byte_offset > len(source):
        parser.error("profile offset is outside the log")
    result = parse(source, args.after_byte_offset)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
