# Historical implementation records

These fixed revisions preserve earlier plans, investigations and observations.
They may describe limits since removed or work since completed; for present
decisions use the [roadmap](../../PLAN.md), the
[compatibility contract](../postgresql-18-compatibility.md) and the
[benchmark evidence index](../../benchmarks/README.md). Offline, use
`git show REVISION:PATH`.

## Before the architecture review

Revision `27af086bff6a9e02f5c05bac1774b6bea50cc03c`:
[earlier detailed behavior narrative](https://github.com/e6qu/pos3ql/blob/27af086bff6a9e02f5c05bac1774b6bea50cc03c/docs/implemented-baseline.md).
Its implementation steps are not open roadmap items.

## Original implementation record

Revision `708d5b475845492bdaa29dd573a6f508770acc62` (PR #599), before the
first documentation consolidation:

| Record | Immutable snapshot |
|---|---|
| Detailed implementation roadmap and evolving conclusions | [PLAN.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/PLAN.md) |
| Resolved defect investigations | [BUGS.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/BUGS.md) |
| Completed implementation evidence | [Implemented baseline](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/docs/implemented-baseline.md) |
| Exploratory performance chronology | [Performance notes](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/docs/performance.md) |
| Former detailed project overview | [README.md](https://github.com/e6qu/pos3ql/blob/708d5b475845492bdaa29dd573a6f508770acc62/README.md) |

## Ownership preparation and benchmark catalog

Revision `51a21b6b7393022b92c4f6f86c889e9fa3394b93` (PR #605):

- [Roadmap and evolving conclusions](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/PLAN.md)
- [Resolved investigations](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/BUGS.md)
- [Detailed ownership summary](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/docs/implemented-baseline.md)
- [Complete dated benchmark catalog](https://github.com/e6qu/pos3ql/blob/51a21b6b7393022b92c4f6f86c889e9fa3394b93/benchmarks/README.md)

Related pull requests:

- [PR #604](https://github.com/e6qu/pos3ql/pull/604): routine-lookup profiling
  and row-version ownership investigation.
- [PR #605](https://github.com/e6qu/pos3ql/pull/605): issued-reader validation.
- [PR #606](https://github.com/e6qu/pos3ql/pull/606): heap ownership and
  compaction repair, with
  [focused validation](https://github.com/e6qu/pos3ql/actions/runs/37767694197).
- [PR #623](https://github.com/e6qu/pos3ql/pull/623): retained-reader
  qualification and fixes that keep metadata-only ALTER from rewriting rows
  while still enforcing uniqueness; its CI covers PRs #620–#622.

## Roadmap before the baseline-first reorder

Revision `44a6cdee6d7237e17b6858be233a1f41445e9d52` (PR #786), before the
roadmap put the deployment baseline first:
[PLAN.md](https://github.com/e6qu/pos3ql/blob/44a6cdee6d7237e17b6858be233a1f41445e9d52/PLAN.md).
Its checked items record finished work: full join merged keys (`975`), self-join removal
(`976`), single-table `LATERAL` subqueries with `Memoize` (`977`),
`max_join_relations` (`978`), batched hash join outer rows (`979`), bounded
`ORDER BY ... LIMIT` over large joins (`980`), and TOAST stored forms through
query values, PL/pgSQL variables and routine parameters (`966`-`992`).
