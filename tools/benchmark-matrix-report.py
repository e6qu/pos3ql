#!/usr/bin/env python3
"""Render paired pos3ql/object-store and PostgreSQL benchmark runs."""

import argparse
import json
import pathlib


BACKENDS = ("fixture", "minio", "seaweedfs")
CHECKPOINT_PAIRS = (
    ("point", "point-concurrency-1", "postgresql18-point-concurrency-1"),
    ("mixed baseline", "mixed-baseline", "postgresql18-mixed-baseline"),
    (
        "mixed with checkpoints",
        "mixed-checkpoint-interference",
        "postgresql18-mixed-checkpoint-interference",
    ),
)
FULL_PAIRS = (
    ("point", "warm-memory-point", "postgresql18-point"),
    ("insert", "concurrent-insert", "postgresql18-insert"),
    ("scan", "analytical-scan", "postgresql18-scan"),
    ("mixed", "mixed-baseline", "postgresql18-mixed"),
)
SUITE_IDENTITY_FIELDS = (
    "mode",
    "rows",
    "table_capacity",
    "operations_per_client",
    "clients",
    "logical_replicas",
    "disk_cache_mib",
    "timeout_seconds",
    "checkpoint_duration_seconds",
)


def number(value, digits=2):
    return "—" if value is None else f"{value:.{digits}f}"


def load_run(directory):
    environment = json.loads((directory / "environment.json").read_text(encoding="utf-8"))
    results = {}
    for path in directory.glob("*.json"):
        value = json.loads(path.read_text(encoding="utf-8"))
        if "workload" in value and "results" in value:
            results[value["label"]] = value["results"]
    return environment, results


def request_rate(result):
    metrics = result.get("object_store")
    if metrics is None or result["completed_operations"] == 0:
        return None
    return sum(metrics["requests"].values()) / result["completed_operations"]


def ratio(numerator, denominator):
    if denominator == 0:
        return None
    return numerator / denominator


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=pathlib.Path)
    args = parser.parse_args()

    runs = {backend: load_run(args.directory / backend) for backend in BACKENDS}
    reference_environment = runs[BACKENDS[0]][0]
    reference_suite = reference_environment["suite"]
    for backend, (environment, _) in runs.items():
        if environment["git_commit"] != reference_environment["git_commit"]:
            raise SystemExit(f"{backend} used a different git commit")
        if environment["pos3ql_binary_sha256"] != reference_environment["pos3ql_binary_sha256"]:
            raise SystemExit(f"{backend} used a different pos3ql binary")
        for field in SUITE_IDENTITY_FIELDS:
            if environment["suite"].get(field) != reference_suite.get(field):
                raise SystemExit(f"{backend} used a different suite {field}")

    pairs = CHECKPOINT_PAIRS if reference_suite["mode"] == "checkpoint" else FULL_PAIRS
    print("# Object-store performance matrix\n")
    print(
        "Each backend run uses the same pos3ql binary and workload shape and includes a "
        "contemporaneous vanilla PostgreSQL 18 baseline. PostgreSQL uses its recorded local "
        "durable tier; pos3ql uses the object store named below.\n"
    )
    print(
        f"Run identity: `{reference_environment['git_commit']}`; binary "
        f"`{reference_environment['pos3ql_binary_sha256'][:16]}…`; "
        f"{reference_suite['rows']} rows, {reference_suite['clients']} clients.\n"
    )
    print("## Object stores\n")
    print("| Backend | Implementation | Backing | Artificial latency | Request metrics |")
    print("|---|---|---|---:|---|")
    for backend in BACKENDS:
        environment = runs[backend][0]
        store = environment["pos3ql_object_store"]
        metrics = "yes" if store["request_metrics_available"] else "no"
        suite = environment["suite"]
        latency = (
            f"{number(suite['object_latency_ms'])} ms"
            if suite.get("object_latency_injected", True)
            else "none"
        )
        print(
            f"| {backend} | {store['implementation']} | {store['backing']} | "
            f"{latency} | {metrics} |"
        )

    print("\n## Paired results\n")
    print(
        "| Backend | Engine | Scenario | completed ops | ops/s | p99 ms | max ms | "
        "object req/op | errors |"
    )
    print("|---|---|---|---:|---:|---:|---:|---:|---:|")
    for backend in BACKENDS:
        results = runs[backend][1]
        for scenario, pos3ql_label, postgres_label in pairs:
            for engine, label in (("pos3ql", pos3ql_label), ("PostgreSQL 18", postgres_label)):
                if label not in results:
                    raise SystemExit(f"{backend} is missing {label}")
                measured = results[label]
                print(
                    f"| {backend} | {engine} | {scenario} | "
                    f"{measured['completed_operations']} | "
                    f"{number(measured['throughput_ops_per_second'])} | "
                    f"{number(measured['latency_ms']['p99'])} | "
                    f"{number(measured['latency_ms']['maximum'])} | "
                    f"{number(request_rate(measured), 3)} | {len(measured['errors'])} |"
                )

    print("\n## Ratios\n")
    print("| Backend | Scenario | pos3ql / PostgreSQL throughput | pos3ql / PostgreSQL p99 |")
    print("|---|---|---:|---:|")
    for backend in BACKENDS:
        results = runs[backend][1]
        for scenario, pos3ql_label, postgres_label in pairs:
            pos3ql = results[pos3ql_label]
            postgres = results[postgres_label]
            print(
                f"| {backend} | {scenario} | "
                f"{number(ratio(pos3ql['throughput_ops_per_second'], postgres['throughput_ops_per_second']))}x | "
                f"{number(ratio(pos3ql['latency_ms']['p99'], postgres['latency_ms']['p99']))}x |"
            )
    print(
        "\nRatios compare the stated end-to-end setups. PostgreSQL has no corresponding "
        "object-request measure. Duration-bound operation counts and resulting checkpoint "
        "generation shapes can differ, and sequential local shared-host samples do not establish "
        "production ratios or rank object-store implementations."
    )


if __name__ == "__main__":
    main()
