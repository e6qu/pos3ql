#!/usr/bin/env python3
"""Collect equal-capacity embedded samples on a dedicated CI runner."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]


HASH_CHUNK_BYTES = 1024 * 1024


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(HASH_CHUNK_BYTES), b""):
            digest.update(chunk)
    return digest.hexdigest()


def rss_metric_contract(system):
    if system == "Linux":
        peak, current = "linux_proc_status_vmhwm", "linux_proc_status_vmrss"
    elif system == "Darwin":
        peak, current = "darwin_getrusage_ru_maxrss", None
    else:
        raise RuntimeError(f"RSS measurement is unsupported on {system}")
    return {"version": 2, "unit": "bytes", "peak_source": peak,
            "current_source": current, "getrusage_source": "getrusage_rusage_self_ru_maxrss"}


def validate_rss_metrics(sample, contract):
    for field, expected in [("rss_metric_version", contract["version"]),
                            ("rss_unit", contract["unit"]),
                            ("rss_peak_source", contract["peak_source"]),
                            ("rss_current_source", contract["current_source"]),
                            ("getrusage_maxrss_source", contract["getrusage_source"])]:
        if field not in sample or type(sample[field]) is not type(expected) or sample[field] != expected:
            raise RuntimeError(f"missing or incompatible RSS metric field: {field}")
    for stage in ["startup", "final"]:
        for suffix in ["peak_rss_bytes", "current_rss_bytes", "getrusage_maxrss_bytes"]:
            field = f"{stage}_{suffix}"
            if field not in sample:
                raise RuntimeError(f"missing RSS counter: {field}")
            value = sample[field]
            if suffix == "current_rss_bytes" and contract["current_source"] is None:
                if value is not None:
                    raise RuntimeError(f"unimplemented current RSS must be null: {field}")
            elif type(value) is not int or not 0 <= value <= (1 << 64) - 1:
                raise RuntimeError(f"invalid RSS byte counter: {field}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--budget-limit-bytes", type=int, required=True)
    args = parser.parse_args()
    if args.budget_limit_bytes <= 0:
        parser.error("budget-limit-bytes must be positive")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    commands = []

    def command(arguments, **kwargs):
        commands.append([str(value) for value in arguments])
        return subprocess.run(arguments, cwd=ROOT, check=True, text=True, **kwargs)

    try:
        revision = command(["git", "rev-parse", "HEAD"], capture_output=True).stdout.strip()
        status = command(["git", "status", "--porcelain"], capture_output=True).stdout
        if status:
            raise RuntimeError("measure committed source in a clean checkout")
        compiler = command(["rustc", "--version", "--verbose"], capture_output=True).stdout
        (output / "revision.txt").write_text(revision + "\n")
        (output / "compiler.txt").write_text(compiler)
        environment = {
            "platform": platform.platform(),
            "python": sys.version,
            "cpu_count": os.cpu_count(),
            "budget_limit_bytes": args.budget_limit_bytes,
            "build_profile": "debug",
            "rss_metrics": rss_metric_contract(platform.system()),
            "hash_chunk_bytes": HASH_CHUNK_BYTES,
            "table_counts": [1, 8, 32, 128],
            "workloads": ["empty", "hot", "even", "moving", "wide", "replacement", "statistics", "statistics-wide"],
            "schema_columns": {"narrow": 2, "wide": 64},
            "row_overlay_slots": 8192,
            "row_work_bytes": 8 * 1024**2,
            "row_work_leases": 128,
            "definition_image_slots": 256,
            "table_metadata_bytes": 64 * 1024**2,
            "table_metadata_leases": 8192,
            "statistics_bytes": 1024**2,
            "statistics_leases": 1024,
            "index_statistics_slots": 4096,
            "active_rows": 512,
            "local_only": True,
        }
        (output / "environment.json").write_text(json.dumps(environment, indent=2) + "\n")
        with (output / "metric-tests.stdout").open("w") as stdout, (output / "metric-tests.stderr").open("w") as stderr:
            command(["cargo", "test", "--locked", "--example", "embedded-memory"], stdout=stdout, stderr=stderr)
        with (output / "build.stdout").open("w") as stdout, (output / "build.stderr").open("w") as stderr:
            command(["cargo", "build", "--locked", "--example", "embedded-memory"], stdout=stdout, stderr=stderr)
        target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        if not target.is_absolute():
            target = ROOT / target
        binary = target / "debug" / "examples" / "embedded-memory"
        (output / "binary.sha256").write_text(sha256_file(binary) + "\n")
        samples = []
        draws = []
        for mode, history in [("mvcc", 65536), ("locking", 0)]:
            for tables in environment["table_counts"]:
                for workload in environment["workloads"]:
                    label = f"{mode}-{tables}-{workload}"
                    data = output / f"{label}.data"
                    arguments = [binary, "--tables", str(tables), "--row-overlay-slots", "8192",
                                 "--row-work-bytes", str(8 * 1024**2), "--row-work-leases", "128",
                                 "--definition-image-slots", "256", "--mode", mode,
                                 "--row-history-slots", str(history), "--workload", workload,
                                 "--rows", "0" if workload == "empty" else "512",
                                 "--data-dir", data, "--budget-limit-bytes", str(args.budget_limit_bytes)]
                    arguments.extend(["--table-metadata-bytes", str(environment["table_metadata_bytes"]),
                                      "--table-metadata-leases", str(environment["table_metadata_leases"])])
                    arguments.extend(["--statistics-bytes", str(environment["statistics_bytes"]),
                                      "--statistics-leases", str(environment["statistics_leases"]),
                                      "--index-statistics-slots", str(environment["index_statistics_slots"])])
                    try:
                        with (output / f"{label}.stdout").open("w") as stdout, (output / f"{label}.budget.log").open("w") as stderr:
                            command(arguments, stdout=stdout, stderr=stderr)
                    finally:
                        if data.is_dir():
                            shutil.rmtree(data)
                    sample = json.loads((output / f"{label}.stdout").read_text())
                    validate_rss_metrics(sample, environment["rss_metrics"])
                    if sample["planned_engine_bytes"] != sample["drawn_engine_bytes"]:
                        raise RuntimeError(f"{label}: startup budget plan/draw mismatch")
                    if sample["drawn_session_bytes"] > sample["planned_session_ceiling_bytes"]:
                        raise RuntimeError(f"{label}: session exceeded its startup ceiling")
                    samples.append(sample)
                    components = {}
                    for line in (output / f"{label}.budget.log").read_text().splitlines():
                        if not line.startswith("budget-draw\t"):
                            continue
                        _, scope, count, component = line.split("\t", 3)
                        key = f"{scope}:{component}"
                        components[key] = components.get(key, 0) + int(count)
                    if sum(count for key, count in components.items() if key.startswith("engine:")) != sample["drawn_engine_bytes"]:
                        raise RuntimeError(f"{label}: incomplete engine component evidence")
                    if sum(count for key, count in components.items() if key.startswith("session:")) != sample["drawn_session_bytes"]:
                        raise RuntimeError(f"{label}: incomplete session component evidence")
                    draws.append({"mode": mode, "max_tables": tables, "workload": workload, "components": components})
        fixed_components = ["engine:row_overlay_slots", "engine:row work pool", "engine:row work free ranges",
                            "engine:table definition image owner", "engine:table definition image occupancy", "engine:table definition images",
                            "engine:table metadata pool", "engine:table metadata free ranges",
                            "engine:statistics pool", "engine:statistics free ranges", "engine:index_cumulative_statistics"]
        for component in fixed_components:
            if any(component not in draw["components"] for draw in draws):
                raise RuntimeError(f"missing fixed-pool evidence: {component}")
            if len({draw["components"][component] for draw in draws}) != 1:
                raise RuntimeError(f"shared pool changed with table count, mode or workload: {component}")
        (output / "samples.jsonl").write_text("".join(json.dumps(sample, sort_keys=True) + "\n" for sample in samples))
        (output / "components.json").write_text(json.dumps(draws, indent=2, sort_keys=True) + "\n")
        report = ["# Embedded capacity measurements", "", f"Revision: `{revision}`.", "",
                  f"RSS metric version: {environment['rss_metrics']['version']}; bytes.",
                  f"Primary peak: `{environment['rss_metrics']['peak_source']}`; current RSS: `{environment['rss_metrics']['current_source']}`.",
                  f"Separate launcher-sensitive diagnostic: `{environment['rss_metrics']['getrusage_source']}`.", "",
                  "Linux primary peaks use the current address space's VmHWM. getrusage may",
                  "retain a launcher's pre-exec peak; its diagnostic is not engine-only RSS.",
                  "Do not compare unlabelled older getrusage peaks as the same metric.", "",
                  "Each sample owns a new process and the same global pool limits. Active samples",
                  "create all configured tables, verify row counts and sums, and forbid heap",
                  "allocation during SQL execution. Moving demand uses TRUNCATE before refilling.", "",
                  "Wide samples create 64-column tables; ordinary populated samples use two columns.",
                  "Replacement samples add columns, roll back a schema savepoint and replace a unique index,",
                  "then verify the original row counts and sums. Statistics samples analyze every table;",
                  "wide statistics samples also change a column type, analyze it and roll back the savepoint.",
                  "Retained-reader pressure is covered by",
                  "ownership/capacity regressions; this single-session matrix does not measure it.", "",
                  "Session reservation is a full configured connection ceiling; session draws report",
                  "the direct embedded client's actual buffers. These are local-only debug samples,",
                  "not a production memory minimum or an object-store throughput qualification.", "",
                  "| Mode | Tables | Workload | Engine reserved MiB | Session drawn MiB | Startup peak MiB | Final peak MiB | Final current MiB | Final getrusage diagnostic MiB |",
                  "|---|---:|---|---:|---:|---:|---:|---:|---:|"]
        for sample in samples:
            current = "—" if sample["final_current_rss_bytes"] is None else f"{sample['final_current_rss_bytes'] / 1024**2:.2f}"
            report.append(f"| {sample['mode']} | {sample['max_tables']} | {sample['workload']} | {sample['planned_engine_bytes'] / 1024**2:.2f} | {sample['drawn_session_bytes'] / 1024**2:.2f} | {sample['startup_peak_rss_bytes'] / 1024**2:.2f} | {sample['final_peak_rss_bytes'] / 1024**2:.2f} | {current} | {sample['final_getrusage_maxrss_bytes'] / 1024**2:.2f} |")
        report.extend(["", "## Remaining metadata costs", "", "Named engine startup reservations for empty samples, in bytes. This exposes",
                       "remaining per-table/catalog costs instead of attributing all changes to rows.", ""])
        components = sorted({key for draw in draws if draw["workload"] == "empty" for key in draw["components"]})
        report.extend(["| Component | MVCC 1 | MVCC 8 | MVCC 32 | MVCC 128 | Locking 1 | Locking 8 | Locking 32 | Locking 128 |", "|---|---:|---:|---:|---:|---:|---:|---:|---:|"])
        empty = [draw for draw in draws if draw["workload"] == "empty"]
        for component in components:
            report.append("| " + component.removeprefix("engine:") + " | " + " | ".join(str(draw["components"].get(component, 0)) for draw in empty) + " |")
        (output / "report.md").write_text("\n".join(report) + "\n")
    finally:
        (output / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
        files = sorted(path for path in output.iterdir() if path.is_file() and path.name != "SHA256SUMS")
        (output / "SHA256SUMS").write_text("".join(f"{sha256_file(path)}  {path.name}\n" for path in files))


if __name__ == "__main__":
    main()
