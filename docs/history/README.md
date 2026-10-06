# Historical implementation records

The 2026-10-06 documentation review replaced duplicated progress journals with
current contracts and an actionable roadmap. Original observations remain in
Git history at the last merged implementation revision:
`708d5b475845492bdaa29dd573a6f508770acc62` (PR #599).

| Record | Immutable snapshot |
|---|---|
| Detailed implementation roadmap and evolving conclusions | [PLAN.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/PLAN.md) |
| Resolved defect investigations | [BUGS.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/BUGS.md) |
| Completed implementation evidence | [Implemented baseline](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/docs/implemented-baseline.md) |
| Exploratory performance chronology | [Performance notes](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/docs/performance.md) |
| Former detailed project overview | [README.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/README.md) |

For an offline checkout, use `git show REVISION:PATH` with the revision above.
Historical statements describe their original implementation and measurement;
they can contain limits since lifted or work since completed. Use the current
[roadmap](../../PLAN.md), [compatibility contract](../postgresql-18-compatibility.md),
and [benchmark evidence index](../../benchmarks/README.md) for present decisions.

Original raw benchmark artifacts, generator provenance, and upstream licenses
remain in their existing locations. Do not regenerate them during an editorial
cleanup or reinterpret exploratory timings as production qualification.
