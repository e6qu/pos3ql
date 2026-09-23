#!/usr/bin/env bash
# Run one workload against each supported local object store and PostgreSQL 18.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
MODE=${1:-checkpoint}
case "$MODE" in
  checkpoint|full) ;;
  *) echo "usage: $0 [checkpoint|full] [output-directory]" >&2; exit 2 ;;
esac
OUTPUT=${2:-"$ROOT/performance-results/matrix-$(date -u +%Y%m%dT%H%M%SZ)"}
mkdir -p "$OUTPUT"

for backend in fixture minio seaweedfs; do
  backend_output="$OUTPUT/$backend"
  if [ -e "$backend_output" ]; then
    echo "matrix output already exists: $backend_output" >&2
    exit 2
  fi
  if [ "$backend" = fixture ]; then
    POS3QL_BENCH_OBJECT_STORE="$backend" \
      "$ROOT/tools/run-performance.sh" "$MODE" "$backend_output"
  else
    POS3QL_BENCH_OBJECT_STORE="$backend" POS3QL_BENCH_OBJECT_LATENCY_MS=0 \
      "$ROOT/tools/run-performance.sh" "$MODE" "$backend_output"
  fi
done

python3 "$ROOT/tools/benchmark-matrix-report.py" "$OUTPUT" >"$OUTPUT/report.md"
echo "performance matrix results: $OUTPUT"
