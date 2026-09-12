#!/usr/bin/env python3
"""Write one schema-versioned object-store metric interval."""

import argparse
import json
import pathlib

from benchmark import subtract_metrics


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--before", type=pathlib.Path, required=True)
    parser.add_argument("--after", type=pathlib.Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--elapsed-seconds", type=float, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--require-read", action="store_true")
    args = parser.parse_args()
    before = json.loads(args.before.read_text(encoding="utf-8"))
    after = json.loads(args.after.read_text(encoding="utf-8"))
    metrics = subtract_metrics(after, before)
    if args.require_read and metrics["requests"]["get"] + metrics["requests"]["range_get"] == 0:
        raise SystemExit("recovery observed no shared-object-store reads")
    result = {
        "schema_version": 1,
        "artifact_type": "recovery",
        "label": args.label,
        "elapsed_seconds": args.elapsed_seconds,
        "object_store": metrics,
    }
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
