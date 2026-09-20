#!/usr/bin/env python3
"""Record the PostgreSQL server and durability settings behind a comparison run."""

import argparse
import json
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
    if args.docker_container:
        image_id = docker_inspect(args.docker_container, "{{.Image}}")
        mounts = json.loads(docker_inspect(args.docker_container, "{{json .Mounts}}"))
    return {
        "schema_version": 1,
        "artifact_type": "postgresql_baseline",
        "version_number": int(version_number),
        "version": version_text,
        "settings": settings,
        "storage_description": args.storage_description,
        "container_image_id": image_id,
        "container_mounts": mounts,
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
    args = parser.parse_args()
    result = capture(args)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
