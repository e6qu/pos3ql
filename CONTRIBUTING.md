# Contributing to pos3ql

Read [README.md](README.md), [PLAN.md](PLAN.md), [AGENTS.md](AGENTS.md), and
[terminology](docs/terminology.md) before changing a boundary. License terms are
in [LICENSE](LICENSE).

## Choose and scope a change

Use the roadmap's remaining sequence and completion criteria. Reproduce a bug
or identify the missing behavior before implementing it. Batch related code,
regressions, recovery/capacity work, and documentation into one complete PR.
Fix incidental bugs you encounter. BUGS.md is reserved for repairs prevented
by a stated external blocker or genuine intractability.

Every PR updates PLAN.md and BUGS.md. Keep current behavior in its owning
[document](docs/README.md); put provenance and observations with their evidence.
Keep PLAN.md focused on open gates and acceptance criteria; BUGS.md contains
only blocked defects. Preserve completed investigations in PRs and the
[history index](docs/history/README.md), without appending progress journals.

## Write documentation people can use

Begin with what the reader can do, what happens, and what the limits mean.
Explain technical terms at first use and add a small example from the user's
point of view. Mark future behavior as a goal rather than implying it works.

For example, explain that “a confirmed order survives loss of the server's
disk in object-store mode” before detailing journal publication and
compare-and-swap. For a capacity limit, say which operation fails, whether the
limit is configurable, and when a change takes effect. Keep exact formats,
numbers, error codes, and source provenance in the appropriate technical guide.

Use the documentation index to avoid repeating the same contract across files.
Preserve dated measurements and upstream licenses; simplify current guidance
without rewriting historical evidence.

## Engineering boundaries

- Match vanilla PostgreSQL strictly at SQL, SQLSTATE, wire, and catalog inputs
  and outputs. Unsupported client-visible behavior must fail explicitly.
- Fix repeated causes through types, input validation, and common update paths.
  Avoid needless wrappers and fallbacks. Make consequential settings and arguments
  explicit; do not silently choose different behavior when a request fails.
- Charge runtime buffers and pools exactly at startup. No growing collections,
  heap allocation, or allocating sorts after the runtime memory freeze.
  Exhaustion names the capacity. Object-store TLS is isolated and budgeted.
- Keep durable data in object storage and cache state disposable. Provider
  behavior belongs in the shared object-store contract, not database logic.
- When changing concurrency, preserve which versions transactions see, lock
  order, durable success responses, safe retries, reader lifetimes, and bounded
  queues. The [glossary](docs/terminology.md) defines the technical terms.
- Backward compatibility with earlier pos3ql versions is not required.
  Keep the [format inventory](docs/durable-format.md) accurate and reject
  unsupported identities before interpreting their bytes. Format changes must
  preserve recovery of data written by the current version.

The runtime dependency policy is the standard library, libc, and the isolated
object-store TLS component and its dependencies. A new dependency requires an
explicit policy decision and memory-accounting review.

## Development and resource policy

Use the Rust toolchain exercised by CI. PostgreSQL 18 supplies reference results;
external suites also use Python 3, Docker, and the client tools installed by
the workflow that owns the check. The release target is Linux x86-64; the event
loop also contains macOS support. See `Cargo.toml`, `examples/dev.conf`, and
`.github/workflows/` for the actual build and environment definitions.

SQL test harnesses prepare their Python clients with
`tests/external/python_env.py VENV`. Dependencies and wheel hashes are pinned in
`tests/external/requirements.lock`, with links to their PyPI release metadata.
For example, an interrupted download retries up to three times with the same
15-second request timeout; a failed installation never marks the environment
ready. Verified wheels are cached and installed without network access. CI
shares that cache through `.github/actions/python-test-environment`.

Protect the interactive machine. Full builds, full test gates, and large
benchmark/evidence regeneration run on GitHub runners. Local work is serial,
low priority, and monitored: preserve at least 64 GiB free disk, keep generated
`target` data below 2 GiB, and keep each workload at or below 1 GiB sampled
aggregate RSS with a 180-second deadline. Use the workspace's provided resource
guard for local checks. If it refuses or a limit is reached,
move the workload to CI; do not bypass the guard or raise limits automatically.

On a suitable dedicated runner, a focused regression can be selected with:

```sh
cargo test --lib --locked regression_name -- --nocapture
```

Start a built development executable with `examples/dev.conf`; that configuration
uses local-only durability. Keep credentials in an owner-only file and never
include them in commits, PR bodies, logs, or retained evidence.

## Verification

Use the existing fixtures and allocation guard for changed runtime behavior.
Verify the invariant or external contract, not a test that merely restates the
implementation. Boundary changes generally need accepted/rejected capacity,
rollback, failure cleanup, WAL/checkpoint, and empty-cache recovery coverage.
SQL and wire behavior needs comparison with actual PostgreSQL, including
SQLSTATEs, types, result shape, and values.
Synchronize concurrent catalog changes before comparing current permission
lookups. For example, after another connection grants access to `orders`,
`LOCK TABLE orders IN ACCESS SHARE MODE` acquires a new relation lock and makes
PostgreSQL process pending cache changes. A constant-only `pg_get_acl` query can
otherwise reuse cached information in repeatable read. Keep checking ordinary
catalog listings against the older transaction snapshot. See PostgreSQL 18.6's
[relation lock refresh](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/storage/lmgr/lmgr.c#L110-L130)
and [catalog snapshot invalidation](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/cache/inval.c#L768-L789).
Use the same relation kind on both sides when checking accepted commands. For
example, a PostgreSQL plain table cannot establish whether a command is accepted
on a foreign table; use a PostgreSQL foreign table for that boundary.

The object-store simulator models remote storage with growing fixture vectors.
Keep simulator uploads outside allocation-forbidden scopes when checking client
execution. For example, a guarded MERGE can read a saved source table; use the
external object-store checks for a VALUES source that writes temporary files.

Required PR workflows are:

| Workflow | Verification |
|---|---|
| CI | Architecture/bug/vendor guards, tool tests, all-target build, formatting, lint, library shards, object-store qualification, recovery/fault simulation, release install, and performance smoke |
| Differential | PostgreSQL comparisons, vendored SQLLogicTest/regression slices, generated queries, and external execution paths |
| Drivers | Actual client transcripts against PostgreSQL and pos3ql |
| coverage | Instrumented library/external shards and merged coverage gate |

The workflow files define exact commands and supported environments. A focused
runner validates iteration; it does not replace complete PR gates. Remove
temporary validation workflows before the final PR head. Fix failures on the
same PR branch and rerun the affected gates; merge only when every required
check passes on that head.

PostgreSQL comparison shards share one release binary built from their checkout.
Each shard verifies its recorded revision and checksum before use. The CI harness
takes an explicit executable path and never compiles during a comparison run.

Root-cause execution timeouts using step timings, logs, resource use, and the
smallest reproducer. Fix the slow work, deadlock, resource contention, or workload
partitioning; do not increase timeout durations. A job waiting without an assigned
runner is a scheduling delay, not evidence of an execution timeout.

Regenerate built-in catalog metadata from the pinned PostgreSQL 18.6 reference
with explicit container and output arguments:

```sh
tools/generate-pg18-catalog-metadata.sh pos3ql-pg18-ref src/sql/catalog_metadata.rs
```

The generator rejects other server versions and records the source version and
container image. It includes the input/output routine identities reported by
`pg_type`; user-defined domains, enums, arrays and row types derive those
identities from their type definitions.

The catalog comparison also checks built-in view SQL against the PostgreSQL
service. To regenerate its static definitions from retained `catalog-metadata.json`
evidence, supply the evidence URL and destination explicitly:

```sh
python3 tools/generate-pg18-view-definitions.py --mode write \
  --metadata /path/to/catalog-metadata.json --source https://example/evidence \
  --output src/sql/catalog_view_definitions.rs
```

The generator requires PostgreSQL 18.6, records the metadata checksum and source,
and retains the upstream license reference. Its `check` mode compares SQL while
preserving the provenance of the checked-in generation.

CI also exercises shared heap publication and saved row and index file ownership with
Miri, alongside allocation-forbidden concurrent regressions.
Pass the selected ownership filters together to share one interpreter startup;
[Rust's test harness accepts multiple filters](https://doc.rust-lang.org/src/test/cli.rs.html#161-163).
Every configured filter must produce passing tests, with none failed or ignored.
Coverage stops at a failed build or instrumentation setup and retains its
diagnostics; it must never run a cached executable after that failure.
Native test shards preserve failed exit codes and resolve allocation-abort
addresses with the runner's `addr2line`, so the log names the allocating code.
The wire suite stops at its first transport timeout and captures the live Linux
server stack and operational state before cleaning up the failed connection.
Stack arguments are omitted; protocol coverage installs its debugger explicitly.

PostgreSQL service and client containers use Docker's official image from
[Google's Docker Hub cache](https://docs.cloud.google.com/artifact-registry/docs/pull-cached-dockerhub-images),
pinned by manifest digest. Keep service images, container selectors, and client adapters on the
same digest. Verify updated manifests against Docker Hub's official image and
retain that comparison in the PR; do not switch registries silently on failure.
SeaweedFS qualification uses the upstream GHCR release image at the same digest
as its Docker Hub publication. Registry changes preserve image provenance.

For benchmarks, follow [performance.md](docs/performance.md). Keep raw artifacts,
revision/binary hashes, tool and service provenance, hardware, storage, network,
and workload settings. Compare against stock PostgreSQL with its ordinary local
durable tier. Report host-available and resource-matched controls separately;
shared-host runs are exploratory, and hosted-runner timing is not a production
performance gate. Preserve upstream corpora, licenses, pins, and checksums in
`vendor/`; changes must pass `tools/check-vendor.sh`.

## Pull requests and commits

Describe the behavior changed, why, verification performed, and material limits.
Distinguish passing focused tests from still-running complete gates. Open a
reviewable PR and resolve failures there rather than creating replacement branches.

Every commit message is exactly one line of at most 80 characters, with no body,
trailers, AI attribution, authored-by text, or co-authored-by text. Use ordinary
Git author metadata. PRs are squash-merged after passing CI. Supply the squash
subject and an explicitly empty body; match the checked head when merging:

```sh
gh pr merge PR_NUMBER --squash --match-head-commit FULL_HEAD_SHA \
  --subject 'One-line subject of at most 80 characters' --body ''
```

Follow the repository's authorization and review policy before merging. Verify
the resulting commit contains only the intended subject.
