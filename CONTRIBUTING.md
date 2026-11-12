# Contributing to pos3ql

Read [README.md](README.md), [architecture](docs/architecture.md),
[PLAN.md](PLAN.md), and [terminology](docs/terminology.md) before changing a
boundary. [AGENTS.md](AGENTS.md) holds the binding engineering and change rules
(PostgreSQL strictness, no silent fallbacks, fixed runtime memory, object
storage as the durable tier, commit and PR format, the Boyscout Rule); this
guide does not repeat them. License terms are in [LICENSE](LICENSE).

## Choose and scope a change

Pick work from the [milestones](PLAN.md#milestones). Reproduce a bug or identify
the missing behavior before implementing it. Keep current behavior in its
owning [document](docs/README.md), unfinished work in PLAN.md, and only blocked
defects in BUGS.md. Completed investigations live in PRs and the
[history index](docs/history/README.md); do not append progress journals.

When changing concurrency, preserve which versions transactions see, lock order,
durable success responses, safe retries, reader lifetimes and bounded queues.
Keep the [format inventory](docs/durable-format.md) accurate; format changes must
preserve recovery of data written by the current version. A new runtime
dependency requires an explicit policy decision and a memory-accounting review.

## Write documentation people can use

Start with what the reader can do, what happens, and what the limits mean. For
example, say that "a confirmed order survives loss of the server's disk in
object-store mode" before detailing journal publication and compare-and-swap.
For a capacity limit, say which operation fails, whether the limit is
configurable, and when a change takes effect.

- Distinguish hard encodings, startup reservations, configured examples,
  calculated ceilings and measured results. State throughput units and the
  workload, hardware and revision; never turn addressable bytes or same-host
  timings into a production capacity promise.
- Link historical benchmark evidence with immutable GitHub URLs. Release
  archives ship the benchmark index, so its links must not depend on local
  baseline files.
- Preserve dated measurements, upstream licenses, pins and checksums; corpora
  in `vendor/` must pass `tools/check-vendor.sh`.

## Development and resource policy

Use the Rust toolchain exercised by CI. PostgreSQL 18 supplies reference
results; external suites also need Python 3, Docker and the client tools
installed by the owning workflow. `Cargo.toml`, `examples/dev.conf` and
`.github/workflows/` define the actual build and environment.

Protect the interactive machine. Full builds, full test gates and large
benchmark or evidence regeneration run on GitHub runners. Local work is serial,
low priority and monitored through the workspace's resource guard: keep at
least 64 GiB free disk, generated `target` data below 2 GiB, and each workload
at or below 1 GiB sampled aggregate RSS with a 180-second deadline. If the guard
refuses or a limit is reached, move the work to CI; do not bypass the guard or
raise its limits.

A focused regression on a suitable runner:

```sh
cargo test --lib --locked regression_name -- --nocapture
```

`examples/dev.conf` starts a development executable with local-only
durability. Keep credentials in an owner-only file and out of commits, PR
bodies, logs and retained evidence.

SQL test harnesses build their Python clients with
`tests/external/python_env.py VENV`. Packages and wheel hashes are pinned in
`tests/external/requirements.lock` (with PyPI release links). Downloads retry up
to three times with the same 15-second timeout; a failed install never marks
the environment ready. CI shares the verified wheel cache through
`.github/actions/python-test-environment`.

When adding a required configuration setting, update file samples, generators
and embedded test configuration strings, including those outside `config.rs`.

### Regenerate PostgreSQL-derived tables

All generators pin PostgreSQL 18.6 (`REL_18_6`), reject other versions, and
record their source and checksums.

```sh
# Built-in catalog metadata, from a running reference container.
tools/generate-pg18-catalog-metadata.sh pos3ql-pg18-ref src/sql/catalog_metadata.rs

# Planner procedure attributes from upstream pg_proc.dat and pg_proc.h.
python3 tools/generate-pg18-planner-properties.py \
  --source /path/to/pg_proc.dat --header /path/to/pg_proc.h \
  --type-io-source src/sql/catalog_builtin_properties.rs \
  --output src/sql/catalog_planner_properties.rs

# Callable contracts: add OIDs (none re-verifies digests and the OID index).
python3 tools/generate-pg18-callable-signatures.py \
  --dsn 'host=127.0.0.1 port=5432 user=postgres' \
  src/sql/catalog_callable_signatures.rs 6381 6388

# Shipped pg_description text of the listed built-in routines (--mode check compares).
python3 tools/generate-pg18-routine-descriptions.py --mode write \
  --dsn 'host=127.0.0.1 port=5432 user=postgres' \
  --signatures src/sql/catalog_callable_signatures.rs \
  --output src/sql/catalog_routine_descriptions.rs

# Every pg_catalog pg_type row, all 32 columns (--mode check compares).
python3 tools/generate-pg18-type-rows.py --mode write \
  --dsn 'host=127.0.0.1 port=5432 user=postgres dbname=template1' \
  --output src/sql/catalog_type_rows.rs

# Built-in view SQL from retained catalog-metadata.json evidence (--mode check compares).
python3 tools/generate-pg18-view-definitions.py --mode write \
  --metadata /path/to/catalog-metadata.json --source https://example/evidence \
  --output src/sql/catalog_view_definitions.rs
```

`tools/generate-pg18-type-layouts.py` takes the same `--mode`, `--metadata`,
`--source` and `--output` arguments and writes `src/sql/type_layouts.rs`. Those
layouts describe PostgreSQL's catalog and wire contracts, not pos3ql's object
block layout. Planner sources are
[`pg_proc.dat`](https://github.com/postgres/postgres/blob/REL_18_6/src/include/catalog/pg_proc.dat)
and [`pg_proc.h`](https://github.com/postgres/postgres/blob/REL_18_6/src/include/catalog/pg_proc.h);
they add no callable functions.

## Verification

### Required PR workflows

| Workflow | File | Gates |
|---|---|---|
| CI | `ci.yml` | Formatting and type checks; architecture, bug, docs, vendor, timeout and duplicate guards; tool tests; Linux amd64 build and Clippy; 12 library shards (`tools/test-lib-shard.sh N-of-12`); native build, test (call contracts and boundaries in separate jobs) and lint on Linux arm64 and macOS arm64; macOS mutex ThreadSanitizer gate; Miri ownership (`memory-sql`, `sql-exec`, `storage`, `storage-definitions`); object-store qualification; storage VOPR; release install; performance smoke; embedded capacity |
| Differential | `differential.yml` | One release binary, then sharded PostgreSQL comparisons, vendored SQLLogicTest and regression slices, generated queries and external execution paths (`tests/external/ci_diff.sh`) |
| Drivers | `drivers.yml` | pgJDBC, pgx, node-postgres, psycopg2, and SQLAlchemy/Alembic with both psycopg2 and psycopg 3, against PostgreSQL and pos3ql (`tests/external/ci_drivers.sh`) |
| coverage | `coverage.yml` | Instrumented library and external shards (`tools/coverage.sh`), crash torture, merged 70% line floor (`tools/coverage-merge.py 70`) |

Each workflow first calls `formatting.yml`: `cargo fmt --check` plus
`rustfmt --check --edition 2024 src/storage/native_tests.rs` (Cargo does not
traverse that file), then `cargo clippy --locked --all-targets -- -Dwarnings`.
Build and test matrices start only after both pass, so one compiler error does
not launch dozens of failing builds. A formatting failure retains a
`formatting-<revision>` artifact with the correction patch; apply it before
pushing again.

A focused runner validates iteration; it does not replace the PR gates. Remove
temporary validation workflows before the final head.

### Time budget and sharding

- Every runner job declares exactly one `timeout-minutes`, at most 15
  (`tools/check-ci-timeouts.sh`). A shorter step timeout is extra protection,
  not a substitute.
- Keep existing budgets. When work outgrows a job, shard it instead of raising
  the timeout: coverage uses `COVERAGE_SHARD` (`lib:N-of-6`, `run:<groups>`,
  `runtest:<groups>`), and external harness groups use `POS3QL_RUN_GROUPS`.
  The exact-error corpus splits by file ordinal with `POS3QL_EXACT_SHARD` and
  `POS3QL_EXACT_SHARDS`, like the SQLLogicTest query slices.
  Coverage tracing and `runtest:` correctness shards stay in separate matrix
  entries.
- Bound compiler concurrency and debug data rather than adding time. Native
  arm64 jobs build every target once with the `native-ci` profile and
  `--jobs 1` (debug assertions, overflow checks and line backtraces, without
  full variable debug data); separate test jobs run that build's library
  executable with the full native filter lists. The artifact helper checks
  revision, target, length and SHA-256, and restores `RUST_MIN_STACK` from the
  checked-in Cargo configuration.
- Coverage builds and runs use the `coverage` profile (release with
  optimization level 1). Production release builds keep release settings.
- Preserve explicit profiles, assertions, native targets and test selections.

### Native targets

- Native Linux amd64, Linux arm64 and macOS arm64 builds, lint and boundary
  regressions must pass. Jobs verify their compiler host first. Use libc's
  declared C types at C boundaries (`c_char` signedness differs by target).
- Function-call contracts and broader native boundary cases run in separate
  jobs per target, so each has its own budget and a broken call contract is
  reported on its own.
- Native SQL and storage fixtures stay in native gates and never enter
  interpreter startup; Miri ownership cases live beside their runtime modules.
- The macOS mutex gate runs every `core_mutex_` ownership case and a direct
  Apple API control under ThreadSanitizer, rebuilding the standard library with
  the same instrumentation and no race suppressions.

### Miri ownership

`tools/test-miri-ownership.sh LOG GROUP` runs one of four groups:
`memory-sql`, `sql-exec`, `storage` and `storage-definitions`. A proof belongs
to the group of its longest matching module prefix in
`tools/miri-ownership-manifest.py`; move slow modules into their own group
when one approaches the 15-minute job budget. The `miri-ownership` Cargo feature omits unrelated
test modules under Miri only, so interpreter startup does not compile
irrelevant fixtures; native builds keep the full test set.

- List every case by its full Rust name in `tools/miri-ownership-tests.txt`;
  each belongs to exactly one group and its module stays enabled in ownership
  mode. The script fails on any missing, repeated, failed or ignored proof.
- Every group must complete. Reducing startup work must not drop proofs.
- Tests that build an anonymous invocation install an explicit SQL plan owner
  through `type_input_plans::with_owner`, as the command executor does.
- Keep deadline proofs independent of the host clock under Miri isolation.

Linux memory ownership is checked with Miri; Apple locks are checked natively
because the Miri model is wrong for them. Nightly rustc `5b7b37aef` emulates an
unlocked unfair lock by
[writing zero atomically](https://github.com/rust-lang/rust/blob/5b7b37aef/src/tools/miri/src/shims/unix/macos/sync.rs#L410)
and later [reading it non-atomically](https://github.com/rust-lang/rust/blob/5b7b37aef/src/tools/miri/src/shims/unix/macos/sync.rs#L127)
([failing run](https://github.com/e6qu/pos3ql/actions/runs/42578259095/job/129034044286)).
To compare the two:
`cargo miri test -p pos3ql-mutex-model --lib --target aarch64-apple-darwin -- core_mutex_direct_native_api`.

### Pinned external inputs

- pgJDBC 42.7.4 from
  [Maven Central](https://repo.maven.apache.org/maven2/org/postgresql/postgresql/42.7.4/),
  SHA-256 `188976721ead8e8627eb6d8389d500dccc0c9bebd885268a3047180274a6031e`
  (retrieved 2026-10-28; repository SHA-1
  `264310fd7b2cd76738787dc0b9f7ea2e3b11adc1`). The script verifies checksum and
  jar structure on download and cache restore. Change version, checksum, source
  and cache key together.
- PostgreSQL 18.6 containers come from
  [Google's Docker Hub cache](https://docs.cloud.google.com/artifact-registry/docs/pull-cached-dockerhub-images),
  pinned to the Linux amd64 manifest
  `sha256:885953109528ad3dfc90362b1a6f50a78620b5315be19f187753d267e484dc5b`
  from official index
  `sha256:74935e72241653ca55e0414067e6d8763aceb8a810eb51b452253ec3dcfc4336`.
  Pin the platform manifest, not the index (mirrored attestation descriptors
  can fail to pull). Keep services, selectors and client adapters on one
  digest; verify updates against Docker Hub and record the comparison in the
  PR.
- SeaweedFS qualification uses the upstream GHCR release image at the same
  digest as its Docker Hub publication.

### Writing regressions

Test the invariant or external contract, not a restatement of the
implementation. Boundary changes generally need accepted and rejected
capacity, rollback, failure cleanup, WAL/checkpoint and empty-cache recovery.

- **Compare with PostgreSQL.** SQL and wire behavior must match SQLSTATEs,
  types, result shape and values. Use the same relation kind on both sides (a
  plain table cannot show what a foreign table accepts). Before comparing
  permission lookups after another connection's grant, run
  `LOCK TABLE orders IN ACCESS SHARE MODE` so PostgreSQL processes pending
  cache changes ([lock refresh](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/storage/lmgr/lmgr.c#L110-L130),
  [snapshot invalidation](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/cache/inval.c#L768-L789)).
  Compare host-libm floating results with an explicit error bound in the
  fixture, keeping exact checks for zero/infinity, types and errors
  ([math contract](https://www.postgresql.org/docs/18/functions-math.html)).
- **Corpus harnesses** echo each command and keep stdout verbatim, matching
  [psql's echo and status output](https://github.com/postgres/postgres/blob/REL_18_6/src/bin/psql/common.c#L1088);
  stderr diagnostics are compared separately with file and line, and the
  normalizer selects compared fields. The port-based harness needs its
  reference at `127.0.0.1`; forward a remote reference explicitly.
- **Capacity.** Capacity tests must reach the intended limit and print the
  actual error if it differs; an earlier missing-setting error is not limit
  coverage. Run long and repeated work (nested loops, queries per iteration,
  error handlers) at unchanged capacity in both transaction modes: short loops
  cannot prove reclamation. Keep fixtures within catalog limits by releasing
  completed definitions, never by raising capacities or dropping assertions.
- **Ownership.** Saved values, diagnostics and execution positions must
  survive dropping their source arena, pool owner or parsing storage, and
  return every range on failure. Distinguish lexer buffer exhaustion (`54000`)
  from malformed SQL (`42601`).
- **Synchronization.** Hold fixture responses explicitly (for example, HTTP
  upload responses while inspecting an unfinished commit); a slow response is
  not a synchronization point. Set mock sockets' blocking mode explicitly and
  wait for readiness within the original deadline. Polling tests reclaim each
  snapshot's scratch space before reuse rather than relying on thread
  scheduling. Keep time-only timezone expectations independent of the day or
  season the test runs.
- **Simulator.** The object-store simulator grows fixture vectors; keep its
  uploads outside allocation-forbidden scopes and use the external
  object-store checks for paths that write temporary files.

### Diagnosing failures

- Root-cause timeouts from step timings, logs, resource use and the smallest
  reproducer; fix the slow work, deadlock, contention or partitioning. Do not
  extend timeouts. A job waiting for a runner is a scheduling delay, not an
  execution timeout.
- Keep compiler and per-test progress visible through cancellation. Native
  builds retain `native-build-progress-<target>-<revision>` (Cargo progress and
  timings); coverage library shards stream compiler output and each test's
  progress so the last running test is visible.
- Native crashes need thread backtraces; a retry or longer deadline is not a
  repair. Library coverage requires `COVERAGE_NATIVE_DEBUGGER=gdb` or `none`;
  CI uses `gdb` to print every thread's stack without retrying. Native shards
  keep failed exit codes and resolve allocation-abort addresses with
  `addr2line`. The wire suite stops at its first transport timeout and
  captures the live server stack. Keep the failing selection, parallelism,
  seed and capacity limits when fixing.
- Coverage stops at a failed build or instrumentation setup and never runs a
  cached executable afterwards.
- SQL-session harness assertions print the last query-memory error's source
  location and requested, remaining and capacity bytes, plus whether a
  temporary region was active, without allocating. Check for a temporary
  borrow before changing a limit. Sequence replay and definition-image retry
  failures print similar test-only diagnostics.
- Retained artifacts:

  | Artifact | Contents |
  |---|---|
  | `differential-failure-<shard>-<revision>` | Complete failed comparisons and server logs (7 days) |
  | `crash-failure-a`, `crash-failure-b` | Object-store fixture, server and SQL logs, `object-audit.jsonl` of writes, deletions and recovery roots (no credentials); job b repeats its seed at 0, 1 and 5 ms storage delay, one `latency-N` fixture each |
  | `ingest-failure` | Ingestion fixture, server logs and mutation audit |
  | `miri-ownership-<group>-<revision>` | Log and selected manifest |
  | `macos-mutex-<revision>` | ThreadSanitizer log |
  | `driver-evidence-<revision>` | Driver transcripts, package versions, server logs |

### Merge qualification

Fix failures on the same PR branch and rerun the affected gates. A cancelled
build, a skipped test or a still-running gate is incomplete qualification; a PR
merges only after every required check passes on its head.

For benchmarks, follow [performance.md](docs/performance.md); hosted-runner
timing is not a production performance gate.

## Pull requests and commits

Describe the behavior changed, why, verification performed and material
limits. Distinguish passing focused tests from still-running gates. Resolve
failures in the open PR rather than creating replacement branches. Commit and
squash format are in [AGENTS.md](AGENTS.md#change-rules); merge against the
checked head:

```sh
gh pr merge PR_NUMBER --squash --match-head-commit FULL_HEAD_SHA \
  --subject 'One-line subject of at most 80 characters' --body ''
```

Follow the repository's authorization and review policy, then verify the
resulting commit contains only the intended subject.
