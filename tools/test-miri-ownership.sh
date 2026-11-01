#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: test-miri-ownership.sh LOG" >&2; exit 1; }
LOG=$1

filters=(
  shared_heap_append
  owned_write_inputs_scoped_arena
  arena_owned_list_
  arena_retained_handles_
  arena_retained_slices_
  cursor_owned_query_
  sequence::tests::statement_replay_
  pending_routine_contract_
  retained_input_ownership_
  shared_heap_relocation_heap_
  row_lock_owners_heap_
  stable_row_slots_
  storage::row_versions::tests::row_history_reservation_
  table_definition_cells_
  sliced_heap_cleanup_heap_
  statement_files_miri_
  index_file_owners_miri_
  enum_image_ownership_pool_
  storage::definition_pool::tests
  catalog_lifetime_schemas_share_bounded_images
  owned_ordered_source_tags
  stable_spill_generation_ranges
  stable_spill_generation_exhaustion
  type_publication_workspace_exact_startup_budget
  bound_type_origin_never_reuses_an_exhausted_identifier
  storage::catalog_sources::tests
  storage::catalog_history::tests
  sql::catalog_snapshot::tests
  catalog_snapshot_schema_cache_
  transaction_admission_does_not_wait_for_existing_queries_to_finish
  lock_wait_handoff_
  mem::mutex::tests
)
# All libtest filters share one interpreter startup.
cargo miri test --lib --locked -- "${filters[@]}" --test-threads=1 --color=never --format=pretty \
  2>&1 | tee "$LOG"
python3 - "$LOG" "${filters[@]}" <<'PY'
import pathlib
import re
import sys

log = pathlib.Path(sys.argv[1]).read_text()
passed = re.findall(r"^test (\S+)(?: - should panic)? \.\.\. ok$", log, re.MULTILINE)
results = re.findall(r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", log, re.MULTILINE)
if len(results) != 1 or results[0] != (str(len(passed)), "0", "0"):
    raise SystemExit("Miri did not report one complete passing test run")
missing = [pattern for pattern in sys.argv[2:]
           if not any(pattern in name for name in passed)]
if missing:
    raise SystemExit("Miri filters matched no passing tests: " + ", ".join(missing))
print(f"Miri ownership: {len(passed)} passed across {len(sys.argv) - 2} filters")
PY
