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


def print_postgresql(path, label):
    postgresql = json.loads(path.read_text(encoding="utf-8"))
    if postgresql.get("resource_profile") == "external":
        label = "PostgreSQL external baseline"
    settings = postgresql["settings"]
    image = postgresql["container_image_id"]
    image_text = f"; image `{image[:20]}…`" if image else ""
    limits = postgresql.get("resource_limits")
    if limits and limits["memory_bytes"]:
        resource_text = (
            f"; CPU quota={number(limits['cpu_quota'])}; "
            f"memory limit={number(limits['memory_bytes'] / 1048576)} MiB; "
            f"swap limit={number(limits['memory_swap_bytes'] / 1048576)} MiB"
        )
    elif postgresql.get("resource_profile") == "external":
        resource_text = "; external resource limits not verified"
    else:
        resource_text = "; no container CPU or memory limit"
    print(
        f"{label}: version `{postgresql['version_number']}`{image_text}; "
        f"storage: {postgresql['storage_description']}{resource_text}; "
        f"fsync={settings['fsync']['value']}, "
        f"full_page_writes={settings['full_page_writes']['value']}, "
        f"synchronous_commit={settings['synchronous_commit']['value']}.\n"
    )


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
    suite_mode = None
    if environment_path.exists():
        environment = json.loads(environment_path.read_text(encoding="utf-8"))
        suite = environment["suite"]
        suite_mode = suite.get("mode")
        cache = (
            f"{suite['disk_cache_mib']} MiB fixed disk cache, "
            if "disk_cache_mib" in suite else ""
        )
        timeout = (
            f"{suite['timeout_seconds']} s query timeout, "
            if "timeout_seconds" in suite else ""
        )
        latency = (
            f"{suite['object_latency_ms']} ms injected object latency"
            if suite.get("object_latency_injected", True)
            else "no artificial object latency"
        )
        print(
            f"Run identity: `{environment['git_commit']}`; "
            f"binary `{environment['pos3ql_binary_sha256'][:16]}…`; "
            f"{environment['machine']}, {environment['logical_cpu_count']} logical CPUs; "
            f"{suite['rows']} rows, {suite['clients']} clients, {cache}{timeout}{latency}.\n"
        )
        hardware = environment.get("hardware_description")
        cache_storage = suite.get("cache_storage_description")
        if hardware or cache_storage:
            print(
                f"Benchmark host: {hardware or 'unspecified'}; "
                f"cache storage: {cache_storage or 'unspecified'}.\n"
            )
        object_store = environment.get("pos3ql_object_store")
        if object_store:
            image = object_store.get("container_image")
            image_text = f"; image `{image}`" if image else ""
            image_id = object_store.get("container_image_id")
            image_id_text = f"; resolved `{image_id[:20]}…`" if image_id else ""
            metrics = (
                "exact request metrics available"
                if object_store.get("request_metrics_available", True)
                else "provider request metrics unavailable"
            )
            operation = (
                "independently operated"
                if object_store.get("independently_operated")
                else "operated on the benchmark host"
            )
            qualification = (
                ""
                if object_store.get("independently_operated")
                else " This local-host timing is exploratory."
            )
            location = ""
            if object_store.get("independently_operated"):
                prefix = object_store.get("prefix") or ""
                location = (
                    f"; endpoint `{object_store['endpoint']}`; "
                    f"bucket `{object_store['bucket']}`; prefix `{prefix}`; "
                    f"region `{object_store['region']}`; "
                    f"addressing={object_store['addressing']}; "
                    f"TLS={'on' if object_store['tls'] else 'off'}"
                )
                if object_store.get("tls_ca_sha256"):
                    location += f"; TLS CA `{object_store['tls_ca_sha256'][:20]}…`"
            print(
                f"pos3ql object store: {object_store['implementation']}{image_text}"
                f"{image_id_text}; backing: {object_store['backing']}; {operation}"
                f"{location}; network: {object_store.get('network_description', 'unspecified')}; "
                f"{metrics}.{qualification}\n"
            )
        duration = suite.get("checkpoint_duration_seconds", 0)
        if suite_mode == "checkpoint" and duration:
            print(
                f"Mixed workloads run for at least {duration} seconds and "
                f"{suite['operations_per_client']} operations per client; "
                "operation counts may differ across engines.\n"
            )
            if suite.get("checkpoint_settled_before_interference"):
                print(
                    "Each engine completes an unmeasured settling checkpoint before "
                    "the checkpoint-interference window.\n"
                )
    postgresql_path = args.directory / "postgresql-server.json"
    if postgresql_path.exists():
        print_postgresql(postgresql_path, "PostgreSQL host-available baseline")
    matched_postgresql_path = args.directory / "postgresql-matched-server.json"
    if matched_postgresql_path.exists():
        print_postgresql(matched_postgresql_path, "PostgreSQL resource-matched baseline")
    print("| Scenario | ops/s | p50 ms | p95 ms | p99 ms | max ms | max RSS MiB | index scans | seq scans | object req/op | errors |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
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
        access_path = measured.get("access_path") or {}
        print(
            f"| {label} | {number(measured['throughput_ops_per_second'])} | "
            f"{number(latency['p50'])} | {number(latency['p95'])} | "
            f"{number(latency['p99'])} | {number(latency['maximum'])} | "
            f"{number(rss / 1048576 if rss is not None else None)} | "
            f"{access_path.get('index_scans', '—')} | "
            f"{access_path.get('sequential_scans', '—')} | "
            f"{number(object_requests, 3)} | {len(measured['errors'])} |"
        )

    profile_path = args.directory / "checkpoint-profile.json"
    if profile_path.exists():
        profile = json.loads(profile_path.read_text(encoding="utf-8"))
        print("\n## Checkpoint phases\n")
        print("Profiled pos3ql build; totals cover explicit and automatic checkpoint work "
              "from the workload start through server stop.")
        print(
            "Times sum phase spans and are not query latency or a cross-system metric. "
            "The profile request window includes cleanup after the timed workload ends.\n"
        )
        print("| Phase | Events | Elapsed ms | Block GET | Block PUT | Object DELETE |")
        print("|---|---:|---:|---:|---:|---:|")
        for phase, totals in sorted(profile["totals"].items()):
            print(
                f"| {phase} | {totals['events']} | {number(totals['elapsed_us'] / 1000)} | "
                f"{totals['block_gets']} | {totals['block_puts']} | {totals['deleted']} |"
            )
        requests = profile["object_requests"]
        if requests is None:
            print("\nProvider request totals are unavailable for this profile window.")
        else:
            print(
                f"\nFull profile window: {requests['put']} object PUT, "
                f"{requests['delete']} object DELETE, {requests['list']} LIST."
            )
        overlap = profile.get("operation_overlap")
        if overlap:
            print("\n### Foreground overlap\n")
            print(
                "Operations are grouped when their client-observed interval intersects a "
                "phase on the common realtime axis. One operation can intersect multiple phases.\n"
            )
            print("| Phase | operations | reads | writes | intersection ms | p50 ms | p95 ms | p99 ms | max ms |")
            print("|---|---:|---:|---:|---:|---:|---:|---:|---:|")
            groups = {"no phase overlap": overlap["outside_phases"], **overlap["by_phase"]}
            for phase, measured in groups.items():
                print(
                    f"| {phase} | {measured['operations']} | {measured['reads']} | "
                    f"{measured['writes']} | "
                    f"{number(measured.get('operation_intersection_ms'))} | "
                    f"{number(measured['p50_ms'])} | "
                    f"{number(measured['p95_ms'])} | {number(measured['p99_ms'])} | "
                    f"{number(measured['maximum_ms'])} |"
                )
            print(
                f"\nProfiled phases intersected "
                f"{number(overlap['profiled_phase_intersection_ms'])} ms of "
                f"{number(overlap['total_operation_latency_ms'])} ms summed foreground latency."
            )

    if recoveries:
        print("\n## Recovery and cache-fill intervals\n")
        print("| Cache state | startup s | GET | ranged GET | LIST | response MiB |")
        print("|---|---:|---:|---:|---:|---:|")
        for label, value in recoveries.items():
            objects = value["object_store"]
            if objects is None:
                print(
                    f"| {label} | {number(value['elapsed_seconds'], 3)} | "
                    "— | — | — | — |"
                )
                continue
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
    pg_baseline = results.get("postgresql18-mixed-baseline")
    pg_interference = results.get("postgresql18-mixed-checkpoint-interference")
    pg_matched_baseline = results.get("postgresql18-matched-mixed-baseline")
    pg_matched_interference = results.get(
        "postgresql18-matched-mixed-checkpoint-interference"
    )
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
    elif suite_mode == "checkpoint":
        print("- This focused run measures checkpoint overlap; compare its raw workload with a matching run.")
    else:
        print("- The smoke suite does not run long-form comparison scenarios.")
    if pg_baseline and pg_interference and interference:
        print(
            "- Matched checkpoint commands completed: "
            f"pos3ql {interference['results']['maintenance_operations']}, "
            f"PostgreSQL 18 {pg_interference['results']['maintenance_operations']}. "
            "This short shared-host run compares SQL workloads on distinct persistence tiers; "
            "the p99 samples above do not establish production ratios."
        )
    if pg_matched_baseline and pg_matched_interference and interference:
        print(
            "- Resource-matched checkpoint commands completed: "
            f"pos3ql {interference['results']['maintenance_operations']}, "
            "PostgreSQL 18 "
            f"{pg_matched_interference['results']['maintenance_operations']}. "
            "The PostgreSQL container CPU and memory limits are recorded above."
        )

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
