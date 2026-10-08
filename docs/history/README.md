# Historical implementation records

Current guidance is maintained as contracts and open completion gates. These
immutable snapshots preserve earlier progress journals and original observations;
they can describe limits since lifted or work since completed.

## Original implementation record

Revision `708d5b475845492bdaa29dd573a6f508770acc62` (PR #599) preserves the
implementation record before the first documentation consolidation.

| Record | Immutable snapshot |
|---|---|
| Detailed implementation roadmap and evolving conclusions | [PLAN.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/PLAN.md) |
| Resolved defect investigations | [BUGS.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/BUGS.md) |
| Completed implementation evidence | [Implemented baseline](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/docs/implemented-baseline.md) |
| Exploratory performance chronology | [Performance notes](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/docs/performance.md) |
| Former detailed project overview | [README.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/README.md) |

## Ownership preparation and benchmark catalog

Revision `51a21b6b7393022b92c4f6f86c889e9fa3394b93` (PR #605) preserves the
records before the current consolidation:

- [Roadmap and evolving conclusions](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/PLAN.md)
- [Resolved investigations](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/BUGS.md)
- [Detailed ownership summary](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/docs/implemented-baseline.md)
- [Complete dated benchmark catalog](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/benchmarks/README.md)

[PR #604](https://github.com/e6qu/pos3ql/pull/604) retains the routine-lookup
profiling and row-version ownership investigation.
[PR #605](https://github.com/e6qu/pos3ql/pull/605) retains issued-reader validation.
[PR #606](https://github.com/e6qu/pos3ql/pull/606) records heap ownership and
compaction repair, including [focused validation](https://github.com/e6qu/pos3ql/actions/runs/37767694197).

For an offline checkout, use `git show REVISION:PATH` with the corresponding
revision above. Use the current [roadmap](../../PLAN.md),
[compatibility contract](../postgresql-18-compatibility.md), and
[benchmark evidence index](../../benchmarks/README.md) for present decisions.

Original raw benchmark artifacts, generator provenance, and upstream licenses
remain in their existing locations. Do not regenerate them during an editorial
cleanup or reinterpret exploratory timings as production qualification.
