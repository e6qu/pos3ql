#!/usr/bin/env python3
"""Capture the identities needed to reproduce a performance run."""

import argparse
import datetime
import hashlib
import json
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
    args = parser.parse_args()
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
        "logical_cpu_count": os.cpu_count(),
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
        },
    }
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
