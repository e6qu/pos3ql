# Query planning and EXPLAIN

Use this guide to answer "will this query run with PostgreSQL's plan, row
order and `EXPLAIN` output?". A *plan* is the tree of steps (scans, joins,
sorts, aggregates) PostgreSQL's planner chooses from its cost estimates; the
order of rows a query returns without `ORDER BY`, and every `EXPLAIN` figure,
follow from it.

pos3ql ports PostgreSQL 18's planner for the shapes listed here, from the
statistics `ANALYZE` gathers. For those shapes `EXPLAIN` prints the same plan,
costs and row estimates, and the query returns its rows in the same order.
A plan with costs that pos3ql cannot reproduce fails with `0A000` naming the
shape rather than printing invented figures. Shapes still missing are listed
in [PLAN.md](../PLAN.md#close-application-compatibility-gaps).

**User example:** after `ANALYZE orders`, `SELECT * FROM orders WHERE
customer = 17 OR id = 5` reads a bitmap heap scan over a `BitmapOr` of two
bitmap index scans and returns rows by heap position, as PostgreSQL does.

Fixture numbers below refer to `tests/external/differential_exact/`, which
compares output with PostgreSQL 18.6. PostgreSQL function names identify the
ported code.

## Scans of one table

A query reading one table, and the scan of an `UPDATE`'s or `DELETE`'s target,
choose a sequential, index, index-only or bitmap heap scan by PostgreSQL's
costs (`cost_index` with `btcostestimate`, `cost_bitmap_heap_scan` over
`choose_bitmap_and` and `generate_bitmap_or_paths`, `cost_seqscan`, `add_path`
with pathkeys, `cost_sort` and `cost_incremental_sort` under `ORDER BY`,
`create_limit_path` under `LIMIT`) and return rows in that scan's order: an
index scan by key (backward for a reverse order), a bitmap heap or sequential
scan by heap position (`691`, `692`, `720`-`723`). Constraint validation,
index builds, `VACUUM` and `ANALYZE` read rows in the same order.

The model covers:

- B-tree indexes of one or several columns, descending keys, `NULLS FIRST`,
  `INCLUDE` columns, boundary conditions and skip arrays.
- Expression indexes: `ANALYZE` gathers each index expression's statistics
  (`compute_index_stats`), `pg_stats` shows them under the index's name, and a
  condition or `ORDER BY` on the expression uses the index (`730`).
- Partial indexes whose predicate the conditions imply (`predicate_implied_by`
  for comparisons with constants, NULL tests, `IN` lists and their `AND` and
  `OR`).
- Comparisons with constants, `IN`/`NOT IN` lists, `op ANY (array)`, NULL
  tests, boolean columns tested alone, and `OR`s of these; `BitmapAnd` and
  `BitmapOr`.
- Pattern conditions under `C` (`752`): `LIKE` (its `ESCAPE` folded through
  `like_escape`), `ILIKE` (prefix up to a letter), regular expressions (`~`,
  `~*`, prefix from `pg_regprefix`) and `starts_with` bound an index by their
  prefix, non-ASCII too (`make_greater_string`), with `patternsel`'s
  selectivity. A `varchar` column cast to `text` is the column (`731`).
  Pattern classes (`text_pattern_ops`, `varchar_pattern_ops`,
  `bpchar_pattern_ops`) serve `=` and the `~<~` family only.
- Conditions on an expression no index holds take PostgreSQL's defaults
  (`753`): equality `1/DEFAULT_NUM_DISTINCT` (a boolean's 1/2), inequality
  `DEFAULT_INEQ_SEL`, two bounds `DEFAULT_RANGE_INEQ_SEL`, a NULL test
  `DEFAULT_UNK_SEL`, a boolean function a third. `X = X` becomes `X IS NOT
  NULL`, and `NOT` of a comparison, pattern, `IN` list, null test, `AND` or
  `OR` folds to its negation (`negate_clause`, `834`).
- Comparisons with an InitPlan's output (`var_eq_non_const`) or a stable
  expression estimated from its planning-time value
  (`estimate_expression_value`, charged once by `index_other_operands_eval_cost`,
  `783`).
- Cast costs: a built-in's argument of another type is charged its cast
  function unless binary coercible; integer cross-type and `real`/`double
  precision` operators cast nothing (`793`); `real` against an integer or
  `numeric` takes the `real`-`double precision` operator (`796`).
- The target list's width and evaluation cost
  (`apply_scanjoin_target_to_paths`).
- `min` and `max` of a column as `preprocess_minmax_aggregates` InitPlans over
  an ordered index scan (`791`, `795`, also in subqueries, `942`).
- Aggregation costed as `cost_agg` (`790`): transition and final functions from
  PostgreSQL 18.6's `pg_aggregate`, shared states (`find_compatible_trans`), a
  `GroupAggregate` over a sort weighed against a `HashAggregate` with its spill
  model (`hash_agg_entry_size`, `hash_agg_set_limits`, `Planned Partitions`),
  index-ordered groupings with keys reordered to the index
  (`get_useful_group_keys_orderings`, `905`), `HAVING` costs and default
  selectivities, and `LIMIT`/`OFFSET` via `adjust_limit_rows_costs` (`821`).
  A grouping whose keys all conditions fix is refused.
- Grouping sets arranged as PostgreSQL does (`extract_rollup_sets`,
  `reorder_grouping_sets`, `DiscreteKnapsack`, `MixedAggregate`), with
  PostgreSQL's costs over one table (`802`).
- A `LIMIT` over a sorted grouping stops at the first row of the group after
  the last one taken (`941`).

Statistics objects (`CREATE STATISTICS`) are built over a sample as
`statext_ndistinct_build`, `statext_dependencies_build` and
`statext_mcv_build` do (`801`): every row of a table up to `300 *` the
statistics target, else a reservoir sample; values wider than 1,024 bytes are
left out of MCV lists and dependencies. An MCV list holds at most 100 items,
each item's values at most 128 bytes of text (`54000` beyond); an expression's
distribution at most 16 KiB per statistic (a named error beyond). The
single-table planner applies them as `statext_clauselist_selectivity` does
(`896`, `897`, `899`-`901`). Join estimates do not use them yet.

SQL functions whose body is one expression are inlined before planning
(`inline_function`, `800`), under PostgreSQL's conditions, with errors
reported `during inlining`; variadic, polymorphic and subquery cases follow
(`890`-`895`). Set-returning SQL functions in `FROM` that are neither strict
nor volatile become subqueries (`inline_set_returning_function`). A `Function
Scan` costs as `cost_functionscan`, with `generate_series` and `unnest` rows
from their support functions.

Partitioned tables and inheritance trees read alone, or written by an
`UPDATE` or `DELETE` that joins nothing, are planned member by member
(`set_append_rel_pathlist`, `750`, `751`): an `Append`, an ordered `Append`
(`partitions_are_ordered`) or a `Merge Append`. Pruning follows `partprune.c`
and constraint exclusion; constants and custom plan arguments prune at
planning, generic plan arguments and stable functions at executor startup
(`Subplans Removed`), and execution prunes with every value it knows
(`600`-`604`, `610`-`614`). `ANALYZE` of a parent records inherited statistics
from a sample of the tree; a partitioned table's `reltuples` counts its
partitions and its `relpages` is -1. Foreign members are not sampled.

Maintenance operations (`CREATE INDEX`, index builds of `ALTER TABLE`,
`REINDEX`, `ANALYZE`, materialized view population) evaluate under the search
path `pg_catalog, pg_temp`, as PostgreSQL's `RestrictSearchPath` does (`754`).

### Page layouts the costs read

PostgreSQL's costs read page counts, so pos3ql emulates its layouts: rows
packed in physical order, a btree laid out as `_bt_buildadd` fills its pages
(an index grown by inserts uses the size estimate), and a visibility map whose
pages stay all-visible after `VACUUM` until an update, delete or insert. A
single-column btree's NULL keys form their own run at the end of the index
(`862`). The btree layout is computed from column statistics, so it can
differ by a page from PostgreSQL's real build.

The local reference used to verify costs runs on arm64, whose compiler fuses
multiply-adds, so a cost there can differ from x86-64's (and pos3ql's) in its
last printed digit.

## Joins

A `SELECT` of plain columns from analyzed local tables without row security
is prepared and planned as PostgreSQL 18 does: `reduce_outer_joins` (a left
join whose `WHERE` tests a compared column `IS NULL` becomes an anti join),
`deconstruct_jointree`, equivalence classes, `standard_join_search` with
`add_paths_to_joinrel`'s merge, nested-loop and hash paths, `costsize.c`'s
join costs and `eqjoinsel` over most common values. Execution runs the chosen
plan. Covered:

- Inner, cross, left, right, full, semi and anti joins; outer join order
  limits (`join_is_legal`); `eqjoinsel_semi`; join filters cheapest first;
  unmatched rows as `ExecNestLoop`, `ExecHashJoin` and `ExecMergeJoin` return
  them (`844`-`847`).
- Index and bitmap paths per relation, parameterized scans of join clauses
  (`consider_index_join_clauses`, `get_loop_count`), unique inner relations
  (`innerrel_is_unique`), `Memoize` (`cost_memoize_rescan`, hits, misses and
  memory as `ExecMemoize` counts them), star-schema parameterization, foreign
  key join selectivity (`844`, `849`, `850`).
- Constants propagated through classes (`845`, `853`, `872`, `876`, `877`);
  an equality of two columns of one relation joins their classes, as
  `process_equivalence` does, so the join clause a class derives names the
  members PostgreSQL's does, and a column compared with an expression of its
  own relation's columns (`b = a - 1`) takes the operator's default
  selectivity, as no side is a pseudoconstant (`976`);
  `remove_useless_joins` (`851`); `remove_useless_self_joins`, which merges
  two scans of one table joined on a unique key into one scan whose filter
  holds both scans' conditions and `IS NOT NULL` of each joined column not
  declared `NOT NULL` (`976`); `EXPLAIN` of such a join whose scans an
  expression class member or a subquery reads fails with `0A000`; inequality and `OR` join conditions
  (`neqjoinsel`, `clauselist_selectivity_or`, `852`);
  `extract_restriction_or_clauses` (`883`); cross-type integer and float keys
  (`882`).
- `EXISTS`, `NOT EXISTS`, `IN` and `ANY` subqueries pulled up as semi and anti
  joins, unique-ified (`create_unique_path`) or made inner joins
  (`reduce_unique_semijoins`, `847`, `873`).
- `USING` and `NATURAL` joins (`848`), whose merged column is PostgreSQL's join
  alias variable (`buildMergedJoinVar`: left column, a right join's right one,
  a full join's `COALESCE`, `972`); parenthesized and aliased joins read into
  one search (`884`, `934`, `937`, `946`, `947`, `951`, `975`).
- A full join's merged column (or a two-argument `COALESCE` of its compared
  columns in written order) equated with a constant equates each side instead
  (`reconsider_full_join_clause`, `888`, `975`); any other `COALESCE` stays a
  join filter.
- Expressions of one or several relations as class members: hashed, merged,
  `Memoize` keys, parameterized index scans, sort keys (`932`, `933`).
- `ORDER BY` and constant `LIMIT`/`OFFSET` brought into the search
  (`grouping_planner` pathkeys, `create_ordered_paths`, `870`); merge joins
  descending or with NULLs first (`874`); `Incremental Sort` batches and
  counters (`875`).
- Grouping, `DISTINCT` and aggregates over joins (`add_paths_to_grouping_rel`,
  `878`-`881`, `887`, `908`, `946`) and windows (`create_window_paths`, `886`,
  `935`).
- `EXPLAIN VERBOSE` target lists as `createplan.c` builds them (`930`).
- Row estimates as doubles clamped at `MAXIMUM_ROWCOUNT` (1e100), so GEQO
  finds PostgreSQL's plan for huge cross products (`970`, `971`).
- A parameterized scan's filter testing a subplan costs `cost_subplan` (`950`).

Hash joins build on the first outer row, return matches in bucket-chain order
(re-chained as `ExecHashIncreaseNumBuckets` does), keep a one-batch table
across rescans, and split a build larger than `work_mem` times
`hash_mem_multiplier` into batches with skew buckets as PostgreSQL does
(`842`, `843`, `854`). Batched keys hash with `hashint2`, `hashint4`,
`hashint8`, `hashtext` and `hashchar`; other key types (`numeric`) keep the
search out of such joins. Merge joins sort with PostgreSQL's quicksort and
replay a `Materialize`'s mark and restore (`910`); a merge join reads an index
scan in order as far as it fetches (`945`).

**Capacities.** The search plans up to 128 relations, 256 conditions, 64
equivalence classes of up to 255 members, 32 clauses of one join and 65,535
clauses, in the statement arena (`sql_arena_bytes`); past any of them a query
fails with `54000` naming the capacity. Paths take 288 bytes each. A
relation with a btree index holds 16 join parameters, so an indexed relation
equated with more than 16 others fails with `54000`. `EXPLAIN` of a
64-relation `USING` chain takes about 4 MB of the arena and a 128-relation
chain's GEQO search about 8 MB. Queries of more than 128 relations run
pos3ql's own join order.

**Outside the search** (never-analyzed tables, row locks, and the shapes
listed in [PLAN.md](../PLAN.md#close-application-compatibility-gaps)), pos3ql
runs its own join order: FROM order, or selectivity order for comma lists; two
items joined by equalities hash the second (`811`); later items join in
nested loops. `EXPLAIN` with costs of such a join fails with `0A000` naming the
join node (`810`); `COSTS OFF` prints the tree pos3ql runs. A query of
analyzed tables reading a shape the search cannot plan (a full join's merged
column, a `USING` join beside a parenthesized one) fails with `0A000` rather
than run that order.

An `UPDATE ... FROM` or `DELETE ... USING` of one plain analyzed FROM table
without an index on the columns its conditions read runs the hash join or
nested loop PostgreSQL's costs pick, changing rows in the join's order, so
triggers and `RETURNING` see PostgreSQL's order (`670`-`672`, `740`, `798`).
PostgreSQL estimates a table from its current size on disk, so a table that
rolled-back or deleted rows grew since its last `ANALYZE` can make it pick the
other order. Into a partitioned table it joins over the pruned `Append`
(`630`, `671`).

A `MERGE` joins its target and source as PostgreSQL plans relations with
statistics (`631`, `651`-`653`): a hash join building on the smaller relation,
or a nested loop over a `Materialize` without an equality; a join keeping both
sides without an equality fails as PostgreSQL's full join does (`0A000`).

## Sorting, hashing and memory figures

Every sort that orders rows the client sees runs one port of `tuplesort.c`
(`915`-`921`): tuples are charged at PostgreSQL's size as they arrive, a sort
past `work_mem` forms runs where `dumptuples` does and merges them as
`mergeruns` does, and a limit directly above bounds a heap. `EXPLAIN ANALYZE`
prints PostgreSQL's `Sort Method` (`quicksort`, `top-N heapsort`, `external
merge`), `Memory` and `Disk` (8,176-byte tape blocks, `906`; `476`, `820`).
Values are charged at PostgreSQL's sizes, including `jsonb`
(`convertToJsonb`), arrays, records, ranges, multiranges, `tsquery` and
`jsonpath`; `pg_column_size` reports the same (`919`). A value with no size
here fails with `0A000`. `jsonb` orders as `compareJsonbContainers` does.
With an object store attached, a sort fitting the external sorter's one
256 KiB chunk orders ties as PostgreSQL does; beyond it ties keep arrival
order.

A `HashAggregate` runs as `nodeAgg.c` does (`926`): past the memory or group
limit it spills new groups to a power of two of partitions, estimated by a
5-bit hyperloglog, read back last first. Partitions are a port of
`logtape.c`'s block allocation, run over PostgreSQL's tuple lengths (for
`Disk Usage`) and over pos3ql's rows in 8 kB blocks leased from
`plan_build_bytes` (`54000` when the pool is full). Groups come back in the
order `ExecAgg` walks its `simplehash` table, each key hashed by its type's
default hash function (`881`, `922`, `924`); an enum key hashes its `pg_enum`
OID as `hashenum` does, but those OIDs come from pos3ql's own identity band
(`925` sorts its groups). `Batches`, `Memory Usage` and `Disk Usage` follow
`hash_agg_update_metrics` for one to seven grouping columns.

The transition values count as PostgreSQL's transition functions allocate
them in `aset.c`'s chunks and freelists (`963`): `min` and `max` of `text`,
`character`, `bytea`, `inet`, `numeric`, `interval`, `time with time zone`
and `tid` keep a copy of the value they return; `sum`, `avg` and the
variances of `numeric` and `bigint` keep a 144-byte `NumericAggState` whose
digit buffers grow with the inputs' weights and scales; `string_agg` keeps
a 1kB buffer it doubles, `array_agg` an array of eight elements it doubles
and a copy of each element passed by reference; the integer and interval
sums allocate their state on their first call, so a group whose `FILTER`
rejects every row takes none. Other aggregates that allocate per group
(`json_agg`, `range_agg`, `array_agg` of arrays, `min` and `max` of arrays,
records and bit strings, ordered-set aggregates) leave the memory, and so a
hashed grouping's order, unmodelled: `0A000` where the order shows.

A one-table `GROUP BY` or `DISTINCT` of expressions plans as PostgreSQL's
does (`929`); a `DISTINCT` takes its `ORDER BY` items first
(`transformDistinctClause`) and `EXPLAIN` shows the `HashAggregate` over the
computed keys under the `Sort` (`961`). The select list is read after
`ExpandRowReference` and `eval_const_expressions`: `(t).*` of a whole-row
reference is the table's columns, and a field of a row constructor is its
argument, so ordinals count expanded columns (`960`). Arrays, records,
ranges, multiranges and `jsonb` hash as `hash_array`, `hash_record`,
`hash_range`, `hash_multirange` and `jsonb_hash`; a named composite key, or
an array of one, hashes when every field type hashes (`962`); `money`, bit
strings and anonymous records have no hash operator class and sort. A
`Sort` over a hashed grouping whose `ORDER BY` names only some keys runs
the `tuplesort` model over the groups in hash table order, so tied groups
keep PostgreSQL's quicksort order. A grouping the model leaves out, or a
hashed one whose transition states have no modelled size, fails with
`0A000` unless its order cannot show: its `ORDER BY` names every key or
output column, every key is fixed by a constant, it forms at most one group,
it is an `IN`, `ANY` or `ALL` subquery without `LIMIT`, or it is a derived
table whose sole reader orders by every shown column of a type whose equal
values print alike, or reduces it to `count`, `min` or `max` (`960`). With
a spill store attached, such a `DISTINCT` fails once a second distinct row
arrives, as in memory.

`aset.c` reuses freed default-sized contexts without recomputing their
chunk limit, so after a hash aggregate under `work_mem` below 1MB
(`CreateWorkExprContext`: 1,024 bytes at `64kB`, 2,048 at `256kB`, 4,096 at
`512kB`) a later aggregate's meta context may get the lower limit at any
`work_mem` (on PostgreSQL 18.6 a later `string_agg` grouping reported other
figures at 1MB, 4MB and 16MB). pos3ql records that per session and fails a
later hash aggregate whose bucket array falls between the lower limit and
8 kB with `0A000` (`926`, `928`). The transition values' context is itself
a work context: a fresh one takes `work_mem`'s limit, one recycled after a
statement that freed many contexts (on PostgreSQL 18.6, a five-way join)
keeps 8 kB. A transition value between the lower limit and 8 kB therefore
fails with `0A000` under `work_mem` below 1MB, or after such an aggregate.

A hash join's `Hash` node reports the table PostgreSQL builds: at least 1,024
buckets, doubled while rows outnumber them, and `Memory Usage` from each row's
minimal tuple plus 8 bytes per bucket (a 1,500-row build reports `Buckets:
2048  Batches: 1`). A `Materialize` reports its tuplestore (`Storage: Memory
Maximum Storage: 18kB` for 40 small rows, mostly its initial array of 2,048
row pointers). A `CTE Scan`, `Recursive Union` and nested loop `Materialize`
report `tuplestore_get_stats` figures (`534`, `536`, `537`, `561`, `820`).

Windows: each clause plans its own `WindowAgg` in PostgreSQL's evaluation
order (`w1`, `w2`, ...) over a `Sort` or `Incremental Sort`; `EXPLAIN ANALYZE`
replays PostgreSQL's partition tuplestore for the default and `ROWS UNBOUNDED
PRECEDING` frames and for `row_number`, `rank`, `dense_rank` and plain
aggregates (`472`, `564`, `907`, `909`, `911`, `912`, `935`); other frames and
functions fail with `0A000`. An integer in a window's `PARTITION BY` or
`ORDER BY` is a constant, never an output position (`914`).

## Subplans, InitPlans and `WITH` queries

An uncorrelated scalar, `EXISTS` or `ARRAY` subquery is an `InitPlan` under
the query's top node, charged as `SS_charge_for_initplans` charges it (`741`);
a correlated one is a `SubPlan` numbered as PostgreSQL numbers it, with its
relations renamed (`u_1`). A `WITH` query PostgreSQL does not inline
(`SS_process_ctes`: `MATERIALIZED`, read twice, volatile, recursive or
data-modifying) is a `CTE` InitPlan read by `CTE Scan` nodes; a recursive one
is a `Recursive Union` over a `WorkTable Scan` that runs an iteration at a time
as readers pull rows. A `WITH` query run once is read lazily when its reader
is the query's only `FROM` item (`840`). An `INSERT` whose source PostgreSQL
pulls up runs the source's InitPlans in its `Insert` node (`943`).

Conditions run in `order_qual_clauses` order: by per-row cost, stably, a
routine or user operator costing its declared `COST` times an operator, so
`f(id) AND id < 4` runs `id < 4` first. Under row security each policy
condition runs first, except leakproof conjuncts cheaper than ten operators.
`UPDATE` and `DELETE` whose conditions call a user routine or volatile
built-in qualify each candidate row in the change loop, as PostgreSQL's
executor does, so the condition's notices interleave with `SET` expressions,
triggers and `RETURNING`.

The planner folds constants as `eval_const_expressions` does before a
`SELECT`, `INSERT ... VALUES`, `UPDATE` or `DELETE` reads a row, so
`''::tsquery` in a three-row query sends its notice once, and a constant in an
arm no row reaches still folds and reports its error. `EXPLAIN` prints folded
conditions (`Filter: (a > 2)`), a `Result` with a false `One-Time Filter` for
a condition folded to false, and an `IN` list of constants as one array.

## `EXPLAIN` output

- `ANALYZE` counts loops, rows, rows removed by `Filter` and `Join Filter`,
  `(never executed)` nodes, `Index Searches`, `Heap Fetches` and `Heap
  Blocks`, in all four formats. Times are measured per loop; in pos3ql's
  push-based executor a node's time includes the nodes reading its rows. A
  rewritten statement runs once, each query in its own plan, its time and I/O
  on the last query.
- `BUFFERS` is on by default under `ANALYZE`, as in PostgreSQL 18 (`936`,
  `939`). pos3ql has no 8 kB buffer pool, so it counts its own pages: the heap
  pages its rows would fill and its modeled btree pages, the same pages
  `pg_class.relpages` uses. A page is a `hit` unless reaching it needed a block
  the RAM cache missed (`read`); temporary tables count `local`, spilled sorts
  and stores `temp`. A sequential scan of a table filling 22 pages prints
  `Buffers: shared hit=22`; inserting one row into a table with a two-level
  primary key prints `shared hit=3`. Existing pages count as already dirty, so
  only new pages are `dirtied`. Planning reads no pages. A write into a table
  with an index other than a plain column btree, or into a foreign table,
  fails with `0A000` under `BUFFERS`; `EXPLAIN (ANALYZE, BUFFERS OFF)` runs
  it.
- `INSERT ... ON CONFLICT` prints its resolution, arbiter indexes (every
  unique index `infer_arbiter_indexes` picks) and, analyzed, its inserted and
  conflicting tuples (`940`, `948`).
- `VERBOSE` prints target lists as `show_plan_tlist` does; a list the
  deparser does not cover fails with `0A000` rather than print a wrong
  `Output:`. A constant `OFFSET 0` or `LIMIT NULL` adds no `Limit`, and a
  trivial `Subquery Scan` is removed.
- Error and notice positions follow PostgreSQL's parser for syntax errors,
  unknown names, failed constant input and the cases in `812`, `814`,
  `830`-`832`, `860`, `861`.

## Planner settings

PostgreSQL's planner settings (every `pg_settings` row in a "Query Tuning"
category) steer how a query is planned; they never change its result. pos3ql
accepts all 52 with PostgreSQL 18's values, units, ranges, errors, `SHOW`
text, `pg_settings` row, `SET LOCAL`, routine `SET` clause and `EXPLAIN
(SETTINGS)` line. Where pos3ql's planner has the same choice, the setting
decides it:

- `enable_seqscan`, `enable_indexscan`, `enable_indexonlyscan`,
  `enable_bitmapscan` and the page and tuple cost settings act on the scans
  above: `SET enable_indexscan = off` makes `SELECT * FROM t WHERE id = 5` use
  a bitmap heap scan, as PostgreSQL does.
- `enable_hashjoin`, `enable_mergejoin`, `enable_nestloop`, `enable_material`
  and `enable_memoize` act on joins the search plans.
- `geqo` and its parameters steer the genetic query optimizer (GEQO), which
  PostgreSQL uses instead of trying every join order once a join problem has
  `geqo_threshold` (12) relations or more. pos3ql runs the same algorithm with
  the same random numbers, so at a given `geqo_seed` a query gets PostgreSQL's
  plan (`952`-`959`). Its pool holds at most 500 join orders (`50 *
  geqo_effort` at most); a larger `geqo_pool_size` fails with `54000` when a
  join is planned.
- `join_collapse_limit` and `from_collapse_limit` split join problems as
  PostgreSQL does (`955`, `956`).
- `hash_mem_multiplier` sizes hash joins and hash aggregates.
- When every way to run a step is disabled, the step still runs and `EXPLAIN`
  marks it `Disabled: true`, as PostgreSQL 18 does.

pos3ql plans no parallel query, TID scan or partitionwise join.
`max_parallel_workers_per_gather` keeps PostgreSQL's range and default (2):
PostgreSQL gives a relation smaller than `min_parallel_table_scan_size` no
workers, so those plans match, and with the setting above 0 an `EXPLAIN` of a
query scanning a larger relation fails with `0A000`, since PostgreSQL may
choose a `Gather` there (writes and row locks are never parallel); at 0 every
plan is exact (`931`). pos3ql behaves as a PostgreSQL built without LLVM: `jit`
is on and `jit_provider` (a server-start setting, so `SET jit_provider` fails
with `55P02`) names `llvmjit`, `pg_jit_available()` is false and no `EXPLAIN`
prints a `JIT` section; `jit_debugging_support` and `jit_profiling_support`
are set only at connection start and `jit_dump_bitcode` only by a superuser;
`pg_settings_get_flags` reports `guc_tables.c`'s flags (`973`).

A prepared statement takes custom plans for its first five executions and its
generic plan after them, as `choose_custom_plan` does under `plan_cache_mode =
auto`, without PostgreSQL's cost comparison. A generic plan chooses paths
without its arguments' values.

## Statistics

`ANALYZE` gathers PostgreSQL's column statistics (`compute_scalar_stats`: most
common values, histogram bounds, correlation and `n_distinct`) from the sample
PostgreSQL reads (`690`); a table larger than 300 rows per unit of the
statistics target is sampled at random, so its figures vary between runs, as
in PostgreSQL. `pg_stats` shows them as an `anyarray` of the column's type.
Range, multirange and `tsvector` columns count every value distinct, and a
type without equality leaves `n_distinct` unknown. Array columns keep most
common elements and the distinct-element histogram (`compute_array_stats`,
`903`); `tsvector` columns keep most common lexemes (`904`). When more elements
pass the cutoff than the target allows, PostgreSQL keeps them in its
`dynahash` iteration order, which cannot be reproduced without porting
`dynahash`; pos3ql keeps ties in first-seen order.

## postgres_fdw plans

The [compatibility guide](postgresql-18-compatibility.md#extensions)
describes what a query of one foreign table sends to the server. Its plans
also follow postgres_fdw: grouped queries whose keys and `WHERE` ship become
one `Foreign Scan` of the grouped relation with `count`, `sum`, `avg`, `min`,
`max`, `bool_and`, `bool_or`, `every` and `string_agg` shipped (`507`, `509`,
`580`, `582`, `583`); windows and local aggregates over a foreign scan
(`581`); `RETURNING` reading the remote `ctid` (`584`); direct `UPDATE` and
`DELETE` (`504`, `506`); InitPlan values and prepared parameters shipped as
`$n::type` (`621`, `622`); `COLLATE` printing (`623`); `ORDER BY` keys fixed by
a `WHERE` equality (`624`); and the remote order shipped only when
postgres_fdw's sorted path costs no more than a local sort (`625`). Values
sent to the server use postgres_fdw's transmission modes (ISO dates,
`extra_float_digits = 3`).
