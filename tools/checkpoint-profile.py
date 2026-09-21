#!/usr/bin/env python3
"""Summarize fixed-memory checkpoint phase lines from one measured interval."""

import argparse
import json
import math
import pathlib
import statistics


PREFIX = "checkpoint_phase "
FIELDS = {
    "lsn", "phase", "slot", "started_ns", "ended_ns", "elapsed_us",
    "block_gets", "block_puts", "deleted",
}
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
        if event["ended_ns"] < event["started_ns"]:
            raise ValueError(f"checkpoint phase ends before it starts: {line}")
        if event["elapsed_us"] != (event["ended_ns"] - event["started_ns"]) // 1_000:
            raise ValueError(f"checkpoint phase duration disagrees with its timestamps: {line}")
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


def percentile(values, percent):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(percent / 100 * len(ordered)) - 1)]


def latency_summary(operations):
    latencies = [(operation["ended_ns"] - operation["started_ns"]) / 1_000_000
                 for operation in operations]
    return {
        "operations": len(latencies),
        "reads": sum(operation["kind"] == "read" for operation in operations),
        "writes": sum(operation["kind"] == "write" for operation in operations),
        "p50_ms": percentile(latencies, 50),
        "p95_ms": percentile(latencies, 95),
        "p99_ms": percentile(latencies, 99),
        "maximum_ms": max(latencies, default=None),
        "mean_ms": statistics.fmean(latencies) if latencies else None,
    }


def correlate(events, operations):
    for operation in operations:
        if set(operation) != {"worker", "operation", "kind", "started_ns", "ended_ns"}:
            raise ValueError("operation trace has the wrong fields")
        if operation["kind"] not in ("read", "write"):
            raise ValueError("operation trace has an unknown kind")
        if operation["started_ns"] < 0 or operation["ended_ns"] < operation["started_ns"]:
            raise ValueError("operation trace has invalid timestamps")
    phase_indexes = {}
    phase_intersections = {}
    any_overlap = set()
    total_operation_ns = sum(
        operation["ended_ns"] - operation["started_ns"] for operation in operations
    )
    for event in events:
        overlapping = phase_indexes.setdefault(event["phase"], set())
        intersections = phase_intersections.setdefault(event["phase"], [])
        for index, operation in enumerate(operations):
            intersection = max(0, min(operation["ended_ns"], event["ended_ns"])
                               - max(operation["started_ns"], event["started_ns"]))
            if intersection:
                overlapping.add(index)
                any_overlap.add(index)
                intersections.append(intersection)
    if operations and not any_overlap:
        raise ValueError("no traced operation overlaps a checkpoint phase")
    by_phase = {}
    for phase, indexes in sorted(phase_indexes.items()):
        summary = latency_summary([operations[index] for index in sorted(indexes)])
        intersections = phase_intersections[phase]
        summary["operation_intersection_ms"] = sum(intersections) / 1_000_000
        summary["maximum_intersection_ms"] = max(intersections, default=0) / 1_000_000
        by_phase[phase] = summary
    profiled_intersection_ns = sum(sum(values) for values in phase_intersections.values())
    return {
        "clock": "CLOCK_REALTIME",
        "traced_operations": len(operations),
        "total_operation_latency_ms": total_operation_ns / 1_000_000,
        "profiled_phase_intersection_ms": profiled_intersection_ns / 1_000_000,
        "unprofiled_operation_latency_ms": (
            total_operation_ns - profiled_intersection_ns
        ) / 1_000_000,
        "outside_phases": latency_summary([
            operation for index, operation in enumerate(operations)
            if index not in any_overlap
        ]),
        "by_phase": by_phase,
    }


def request_delta(before, after):
    if before.get("schema_version") != 1 or after.get("schema_version") != 1:
        raise ValueError("object-store metrics have the wrong schema")
    operations = ("delete", "get", "list", "put", "range_get")
    requests = {}
    for operation in operations:
        difference = after["requests"][operation] - before["requests"][operation]
        if difference < 0:
            raise ValueError(f"object-store {operation} counter decreased")
        requests[operation] = difference
    return requests


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("log", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--after-byte-offset", type=int, default=0)
    parser.add_argument("--metrics-before", type=pathlib.Path, required=True)
    parser.add_argument("--metrics-after", type=pathlib.Path, required=True)
    parser.add_argument("--operation-trace", type=pathlib.Path, required=True)
    args = parser.parse_args()
    source = args.log.read_bytes()
    if args.after_byte_offset < 0 or args.after_byte_offset > len(source):
        parser.error("profile offset is outside the log")
    result = parse(source, args.after_byte_offset)
    result["object_requests"] = request_delta(
        json.loads(args.metrics_before.read_text()),
        json.loads(args.metrics_after.read_text()),
    )
    benchmark = json.loads(args.operation_trace.read_text())
    operations = benchmark.get("results", {}).get("operation_trace")
    if operations is None:
        raise ValueError("benchmark artifact has no operation trace")
    if len(operations) != benchmark["results"].get("completed_operations"):
        raise ValueError("operation trace does not cover every completed operation")
    result["operation_overlap"] = correlate(result["events"], operations)
    profiled_deletes = sum(phase["deleted"] for phase in result["totals"].values())
    profiled_puts = sum(phase["block_puts"] for phase in result["totals"].values())
    if profiled_deletes > result["object_requests"]["delete"]:
        raise ValueError("phase deletes exceed measured object DELETE requests")
    if profiled_puts > result["object_requests"]["put"]:
        raise ValueError("phase block PUTs exceed measured object PUT requests")
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
