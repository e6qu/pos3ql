#!/usr/bin/env python3
"""Record the PostgreSQL server and durability settings behind a comparison run."""

import argparse
import json
import math
import pathlib
import subprocess

from benchmark import PgConnection


SETTINGS = (
    "block_size",
    "checkpoint_timeout",
    "data_directory",
    "fsync",
    "full_page_writes",
    "max_connections",
    "max_wal_size",
    "shared_buffers",
    "synchronous_commit",
    "wal_level",
)


def docker_inspect(container, template):
    return subprocess.check_output(
        ["docker", "inspect", "--format", template, container], text=True
    ).strip()


def capture(args):
    connection = PgConnection(args.host, args.port, args.user, args.database)
    try:
        identity_rows = connection.query(
            "SELECT current_setting('server_version_num'), version()"
        )
        if len(identity_rows) != 1 or len(identity_rows[0]) != 2:
            raise ValueError("PostgreSQL identity query returned an invalid shape")
        version_number, version_text = identity_rows[0]
        if int(version_number) // 10000 != 18:
            raise ValueError(f"expected PostgreSQL 18, got {version_text}")
        names = ", ".join(f"'{name}'" for name in SETTINGS)
        rows = connection.query(
            "SELECT name, setting, unit, source FROM pg_settings "
            f"WHERE name IN ({names}) ORDER BY name"
        )
    finally:
        connection.close()

    settings = {
        name: {"value": value, "unit": unit, "source": source}
        for name, value, unit, source in rows
    }
    if set(settings) != set(SETTINGS):
        raise ValueError(f"missing PostgreSQL settings: {sorted(set(SETTINGS) - set(settings))}")
    required = {"fsync": "on", "full_page_writes": "on", "synchronous_commit": "on"}
    for name, expected in required.items():
        observed = settings[name]["value"]
        if observed != expected:
            raise ValueError(f"PostgreSQL {name} must be {expected}, got {observed}")

    image_id = None
    mounts = None
    resource_limits = None
    if args.docker_container:
        image_id = docker_inspect(args.docker_container, "{{.Image}}")
        mounts = json.loads(docker_inspect(args.docker_container, "{{json .Mounts}}"))
        resource_limits = {
            "cpu_quota": int(
                docker_inspect(args.docker_container, "{{.HostConfig.NanoCpus}}")
            ) / 1_000_000_000,
            "memory_bytes": int(
                docker_inspect(args.docker_container, "{{.HostConfig.Memory}}")
            ),
            "memory_swap_bytes": int(
                docker_inspect(args.docker_container, "{{.HostConfig.MemorySwap}}")
            ),
        }
        if args.resource_profile == "host-available" and any(resource_limits.values()):
            raise ValueError("host-available PostgreSQL has container resource limits")
        if args.resource_profile == "resource-matched" and (
            args.expected_cpus is None or args.expected_memory_bytes is None
        ):
            raise ValueError("resource-matched PostgreSQL requires expected resource limits")
        if args.expected_cpus is not None and not math.isclose(
            resource_limits["cpu_quota"], args.expected_cpus
        ):
            raise ValueError(
                f"PostgreSQL CPU quota is {resource_limits['cpu_quota']}, "
                f"expected {args.expected_cpus}"
            )
        if (
            args.expected_memory_bytes is not None
            and resource_limits["memory_bytes"] != args.expected_memory_bytes
        ):
            raise ValueError(
                f"PostgreSQL memory limit is {resource_limits['memory_bytes']}, "
                f"expected {args.expected_memory_bytes}"
            )
        if args.expected_memory_bytes is not None and resource_limits[
            "memory_swap_bytes"
        ] != args.expected_memory_bytes:
            raise ValueError("resource-matched PostgreSQL swap limit differs from memory")
    return {
        "schema_version": 1,
        "artifact_type": "postgresql_baseline",
        "version_number": int(version_number),
        "version": version_text,
        "settings": settings,
        "storage_description": args.storage_description,
        "container_image_id": image_id,
        "container_mounts": mounts,
        "resource_profile": args.resource_profile,
        "resource_limits": resource_limits,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--user", default="postgres")
    parser.add_argument("--database", default="postgres")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--storage-description", required=True)
    parser.add_argument("--docker-container")
    parser.add_argument(
        "--resource-profile",
        choices=("host-available", "resource-matched", "external"),
        required=True,
    )
    parser.add_argument("--expected-cpus", type=float)
    parser.add_argument("--expected-memory-bytes", type=int)
    args = parser.parse_args()
    if args.expected_cpus is not None and (
        not math.isfinite(args.expected_cpus) or args.expected_cpus <= 0
    ):
        parser.error("expected CPUs must be positive and finite")
    if args.expected_memory_bytes is not None and args.expected_memory_bytes <= 0:
        parser.error("expected memory bytes must be positive")
    if (args.expected_cpus is not None or args.expected_memory_bytes is not None) and (
        not args.docker_container or args.resource_profile != "resource-matched"
    ):
        parser.error("expected resource limits require resource-matched Docker PostgreSQL")
    if args.resource_profile == "external" and args.docker_container:
        parser.error("external PostgreSQL cannot name a Docker container")
    if args.resource_profile != "external" and not args.docker_container:
        parser.error("container PostgreSQL profiles require a Docker container")
    if args.resource_profile == "resource-matched" and (
        args.expected_cpus is None or args.expected_memory_bytes is None
    ):
        parser.error("resource-matched PostgreSQL requires expected CPU and memory limits")
    result = capture(args)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
