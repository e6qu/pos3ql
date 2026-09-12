#!/usr/bin/env python3
"""Render a compact report from benchmark.py JSON artifacts."""

import argparse
import json
import pathlib


def number(value, digits=2):
    return "—" if value is None else f"{value:.{digits}f}"


def ratio(numerator, denominator):
    if numerator is None or denominator in (None, 0):
        return None
    return numerator / denominator


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=pathlib.Path)
    args = parser.parse_args()
    results = {}
    recoveries = {}
    for path in sorted(args.directory.glob("*.json")):
        value = json.loads(path.read_text(encoding="utf-8"))
        if value.get("schema_version") != 1 or "label" not in value:
            continue
        if value.get("artifact_type") == "recovery":
            recoveries[value["label"]] = value
        elif "results" in value and "workload" in value:
            results[value["label"]] = value
    if not results:
        raise SystemExit("no benchmark schema-v1 JSON files found")

    print("# pos3ql performance report\n")
    print("Raw JSON files are the evidence; this report derives comparisons without hiding errors.\n")
    environment_path = args.directory / "environment.json"
    if environment_path.exists():
        environment = json.loads(environment_path.read_text(encoding="utf-8"))
        suite = environment["suite"]
        print(
            f"Run identity: `{environment['git_commit']}`; "
            f"binary `{environment['pos3ql_binary_sha256'][:16]}…`; "
            f"{environment['machine']}, {environment['logical_cpu_count']} logical CPUs; "
            f"{suite['rows']} rows, {suite['clients']} clients, "
            f"{suite['object_latency_ms']} ms injected object latency.\n"
        )
    print("| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | object req/op | errors |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|")
    for label, value in results.items():
        measured = value["results"]
        latency = measured["latency_ms"]
        objects = measured.get("object_store")
        object_requests = None
        if objects:
            object_requests = sum(objects["requests"].values()) / max(
                1, measured["completed_operations"]
            )
        rss = measured.get("maximum_rss_bytes")
        print(
            f"| {label} | {number(measured['throughput_ops_per_second'])} | "
            f"{number(latency['p50'])} | {number(latency['p95'])} | "
            f"{number(latency['p99'])} | {number(latency['maximum'])} | "
            f"{number(rss / 1048576 if rss is not None else None)} | "
            f"{number(object_requests, 3)} | {len(measured['errors'])} |"
        )

    if recoveries:
        print("\n## Recovery and cache-fill intervals\n")
        print("| Cache state | startup s | GET | ranged GET | LIST | response MiB |")
        print("|---|---:|---:|---:|---:|---:|")
        for label, value in recoveries.items():
            objects = value["object_store"]
            requests = objects["requests"]
            print(
                f"| {label} | {number(value['elapsed_seconds'], 3)} | "
                f"{requests['get']} | {requests['range_get']} | {requests['list']} | "
                f"{number(objects['response_body_bytes'] / 1048576, 3)} |"
            )

    print("\n## Derived comparisons\n")
    comparisons = []
    warm = results.get("warm-memory-point")
    disk = results.get("warm-disk-point")
    cold = results.get("cold-object-point")
    if warm and disk:
        comparisons.append((
            "warm-disk / warm-memory p99",
            ratio(disk["results"]["latency_ms"]["p99"], warm["results"]["latency_ms"]["p99"]),
        ))
    if warm and cold:
        comparisons.append((
            "cold-object / warm-memory p99",
            ratio(cold["results"]["latency_ms"]["p99"], warm["results"]["latency_ms"]["p99"]),
        ))
    baseline = results.get("mixed-baseline")
    interference = results.get("mixed-checkpoint-interference")
    if baseline and interference:
        comparisons.append((
            "checkpoint-overlap / baseline p99",
            ratio(
                interference["results"]["latency_ms"]["p99"],
                baseline["results"]["latency_ms"]["p99"],
            ),
        ))
    postgres = results.get("postgresql18-point")
    if postgres and warm:
        comparisons.append((
            "pos3ql / PostgreSQL 18 warm point throughput",
            ratio(
                warm["results"]["throughput_ops_per_second"],
                postgres["results"]["throughput_ops_per_second"],
            ),
        ))
    one = results.get("point-concurrency-1")
    if one and warm:
        comparisons.append((
            "pos3ql concurrent / single-client point throughput",
            ratio(
                warm["results"]["throughput_ops_per_second"],
                one["results"]["throughput_ops_per_second"],
            ),
        ))
    pg_one = results.get("postgresql18-point-concurrency-1")
    if pg_one and postgres:
        comparisons.append((
            "PostgreSQL 18 concurrent / single-client point throughput",
            ratio(
                postgres["results"]["throughput_ops_per_second"],
                pg_one["results"]["throughput_ops_per_second"],
            ),
        ))
    for pos3ql_label, postgres_label, description in (
        ("concurrent-insert", "postgresql18-insert", "pos3ql / PostgreSQL 18 insert throughput"),
        ("analytical-scan", "postgresql18-scan", "pos3ql / PostgreSQL 18 scan throughput"),
    ):
        pos3ql_result = results.get(pos3ql_label)
        postgres_result = results.get(postgres_label)
        if pos3ql_result and postgres_result:
            comparisons.append((
                description,
                ratio(
                    pos3ql_result["results"]["throughput_ops_per_second"],
                    postgres_result["results"]["throughput_ops_per_second"],
                ),
            ))
    if comparisons:
        for label, value in comparisons:
            print(f"- {label}: {number(value)}x")
    else:
        print("- The smoke suite does not run long-form comparison scenarios.")

    scaling = sorted(
        (
            int(label.rsplit("-", 1)[1]),
            value["results"]["throughput_ops_per_second"],
        )
        for label, value in results.items()
        if label.startswith("logical-replicas-")
    )
    if scaling:
        print("\n## Logical read-replica scaling\n")
        one_replica = scaling[0][1]
        for count, throughput in scaling:
            print(
                f"- {count} replica(s): {number(throughput)} ops/s "
                f"({number(ratio(throughput, one_replica))}x the one-replica run)"
            )
        freshness = []
        for path in sorted(args.directory.glob("logical-replica-*-freshness.json")):
            value = json.loads(path.read_text(encoding="utf-8"))
            freshness.append(value["elapsed_seconds"])
        if freshness:
            print(
                f"- observed apply freshness: max {number(max(freshness), 3)} s "
                f"across {len(freshness)} catch-up barriers"
            )

    identities = sorted({
        identity
        for value in results.values()
        for identity in value.get("target_identities", [value["database_identity"]])
    })
    print("\n## Recorded database identities\n")
    for identity in identities:
        print(f"- `{identity}`")


if __name__ == "__main__":
    main()
