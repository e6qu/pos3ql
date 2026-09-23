#!/usr/bin/env python3
"""Capture the identities needed to reproduce a performance run."""

import argparse
import datetime
import hashlib
import json
import math
import os
import pathlib
import platform
import subprocess
import sys


def command(*arguments):
    try:
        return subprocess.check_output(arguments, text=True, stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def physical_memory():
    try:
        return os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE")
    except (ValueError, OSError, AttributeError):
        return None


def available_cpu_count():
    if hasattr(os, "sched_getaffinity"):
        return len(os.sched_getaffinity(0))
    return os.cpu_count()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--mode", required=True)
    parser.add_argument("--rows", type=int, required=True)
    parser.add_argument("--table-capacity", type=int, required=True)
    parser.add_argument("--operations", type=int, required=True)
    parser.add_argument("--clients", type=int, required=True)
    parser.add_argument("--replicas", type=int, required=True)
    parser.add_argument("--object-latency-ms", type=float, required=True)
    parser.add_argument("--object-latency-injected", type=int, choices=(0, 1), required=True)
    parser.add_argument("--object-store-implementation", required=True)
    parser.add_argument("--object-store-backing", required=True)
    parser.add_argument("--object-store-image", default="")
    parser.add_argument("--object-store-image-id", default="")
    parser.add_argument(
        "--object-store-independent-implementation", type=int, choices=(0, 1), required=True
    )
    parser.add_argument("--object-store-request-metrics", type=int, choices=(0, 1), required=True)
    parser.add_argument("--disk-cache-mib", type=int, required=True)
    parser.add_argument("--timeout-seconds", type=float, required=True)
    parser.add_argument("--checkpoint-profile", type=int, choices=(0, 1), default=0)
    parser.add_argument("--checkpoint-duration-seconds", type=float, default=0.0)
    args = parser.parse_args()
    if not math.isfinite(args.checkpoint_duration_seconds) or args.checkpoint_duration_seconds < 0:
        parser.error("checkpoint duration must be nonnegative and finite")
    if not math.isfinite(args.object_latency_ms) or args.object_latency_ms < 0:
        parser.error("object latency must be nonnegative and finite")
    if args.mode == "checkpoint" and args.checkpoint_duration_seconds == 0:
        parser.error("checkpoint mode requires a positive duration")
    binary_bytes = args.binary.read_bytes()
    status = command("git", "status", "--porcelain")
    result = {
        "schema_version": 1,
        "artifact_type": "environment",
        "generated_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "git_commit": command("git", "rev-parse", "HEAD"),
        "git_dirty": bool(status),
        "pos3ql_binary_sha256": hashlib.sha256(binary_bytes).hexdigest(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor() or None,
        "logical_cpu_count": available_cpu_count(),
        "physical_memory_bytes": physical_memory(),
        "python_version": sys.version.splitlines()[0],
        "rustc_version": command("rustc", "--version"),
        "docker_version": command("docker", "version", "--format", "{{.Server.Version}}"),
        "suite": {
            "mode": args.mode,
            "rows": args.rows,
            "table_capacity": args.table_capacity,
            "operations_per_client": args.operations,
            "clients": args.clients,
            "logical_replicas": args.replicas,
            "object_latency_ms": args.object_latency_ms,
            "object_latency_injected": bool(args.object_latency_injected),
            "disk_cache_mib": args.disk_cache_mib,
            "timeout_seconds": args.timeout_seconds,
            "checkpoint_profile_enabled": bool(args.checkpoint_profile),
            "checkpoint_duration_seconds": args.checkpoint_duration_seconds,
            "checkpoint_settled_before_interference": args.mode == "checkpoint",
        },
        "pos3ql_object_store": {
            "implementation": args.object_store_implementation,
            "backing": args.object_store_backing,
            "container_image": args.object_store_image or None,
            "container_image_id": args.object_store_image_id or None,
            "independent_implementation": bool(args.object_store_independent_implementation),
            "independently_operated": False,
            "request_metrics_available": bool(args.object_store_request_metrics),
        },
    }
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
