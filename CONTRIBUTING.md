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

## Engineering boundaries

- Match vanilla PostgreSQL strictly at SQL, SQLSTATE, wire, and catalog inputs
  and outputs. Unsupported client-visible behavior must fail explicitly.
- Fix classes of bugs through types, parse boundaries, and shared publication
  paths. Do not accept and ignore behavior or introduce silent fallbacks.
- Charge runtime buffers and pools exactly at startup. No growing collections,
  heap allocation, or allocating sorts after the runtime memory freeze.
  Exhaustion names the capacity. Object-store TLS is isolated and budgeted.
- Keep durable data in object storage and cache state disposable. Provider
  behavior belongs in the shared object-store contract, not database logic.
- Preserve MVCC, lock ordering, durable acknowledgement, retry ownership,
  reader lifetimes, and backpressure when changing concurrency.
- A format change follows the [migration contract](docs/durable-format.md).
  Preserve old readers and provenance until a verified migration permits removal.

The runtime dependency policy is the standard library, libc, and the isolated
object-store TLS component and its dependencies. A new dependency requires an
explicit policy decision and memory-accounting review.

## Development and resource policy

Use the Rust toolchain exercised by CI. PostgreSQL 18 supplies the SQL oracle;
external suites also use Python 3, Docker, and the client tools installed by
the workflow that owns the check. The release target is Linux x86-64; the event
loop also contains macOS support. See `Cargo.toml`, `examples/dev.conf`, and
`.github/workflows/` for the actual build and environment definitions.

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

PostgreSQL service and client containers use Docker's official image from
[ECR Public](https://gallery.ecr.aws/docker/library/postgres), pinned by manifest
digest. Keep service images, container selectors, and client adapters on the
same digest. Verify updated manifests against Docker Hub's official image and
retain that comparison in the PR; do not switch registries silently on failure.

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
