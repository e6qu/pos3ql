# Function lookup copy repair

The unchanged regression test took 486.20 seconds before the repair and 19.82
seconds after it in these two instrumented runner samples, a 24.5-fold difference.
The test inserts 1,100 rows through a function that also writes an audit table,
fetches from a 70,000-row cursor, checkpoints, deletes its local cache and verifies
both tables after recovery from the object-store simulator.

| Measurement | Before | After |
|---|---:|---:|
| Test harness elapsed seconds | 486.20 | 19.82 |
| Timed command elapsed seconds | 486.30 | 19.88 |
| User CPU seconds | 485.93 | 19.50 |
| System CPU seconds | 0.22 | 0.18 |
| Peak resident memory, KiB | 265,956 | 266,344 |

Five [earlier stack samples](https://github.com/e6qu/pos3ql/actions/runs/41234230450/job/124634602082)
found repeated full function-definition copies during lookup and call binding.
The repair uses input signatures for overload checks, borrows mapping metadata,
copies the selected visible definition once, binds that snapshot in place and
borrows it through execution. It preserves transaction-private replacements and
releases catalog locks before type coercion and argument effects.

These are single samples on different GitHub-hosted Ubuntu runners, each reporting
four logical CPUs and 15,982 MiB RAM. Both use Rust 1.99.1, cargo-llvm-cov 0.9.1,
release instrumentation, one test thread and the same fixture and memory settings.
The test reserves a 1 GiB engine budget. No rows, assertions, recovery checks,
allocation guards or job deadlines were removed. Neither sample swapped or had
major page faults. Build time is excluded from the timed test command.

This is an internal regression diagnosis, not a PostgreSQL performance comparison
or a production throughput, latency or minimum-memory claim. Prefix replay of
mutating function calls remains; retained execution still needs to remove that
repeated work. Coverage instrumentation and runner variation affect timings.

## Provenance and reproduction

Before revision: `fa702ae87a0dfbbb8e0514103b2318b62581e465`,
[passing measurement job](https://github.com/e6qu/pos3ql/actions/runs/41238034847/job/124646199622).
The same diagnostic workflow exposed compilation errors in the first repair;
those errors were fixed before the after sample.

After revision: `b0981ab8da792d7629b630af61264d107679d8d0`,
[passing measurement job](https://github.com/e6qu/pos3ql/actions/runs/41239162352/job/124650216057).
[All-target lint and 115 function regressions](https://github.com/e6qu/pos3ql/actions/runs/41239162352/job/124650216159)
also passed at that revision.

[measurements.json](measurements.json) records revisions, source jobs, downloaded
log hashes and the fixture checksum. [before.log](before.log) and
[after.log](after.log) retain verbatim excerpts from the revision/resource output
through the timed command's exit status, including compiler output and timings.
[SHA256SUMS](SHA256SUMS) checks these evidence files.

At each recorded revision, install the stated Rust toolchain, its
`llvm-tools-preview` component and cargo-llvm-cov version on a suitable runner,
then run:

```sh
coverage_environment=$(cargo llvm-cov show-env --sh)
eval "$coverage_environment"
cargo test --lib --release --locked --no-run
/usr/bin/time -v python3 tools/test_command.py \
  cargo test --lib --release --locked -- \
  --exact sql::tests::remaining_execution_widths_are_allocation_free_and_survive_cold_recovery \
  --test-threads=1
```

The test configuration and recovery checks are in `src/sql/tests.rs`; its SQL
fixture is recorded in the measurement manifest. The temporary diagnostic
workflow is removed after this comparison; full PR workflows remain mandatory.
