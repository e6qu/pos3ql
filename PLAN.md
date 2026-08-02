# pos3ql roadmap

Architecture summary lives in [README.md](README.md); known bugs and divergences
in [BUGS.md](BUGS.md); standing directives in [AGENTS.md](AGENTS.md); the glossary
and naming rules in [docs/terminology.md](docs/terminology.md). Decisions fixed with the
project owner: hand-rolled everything (no tokio / pgwire / sqlparser-rs / AWS
SDK; `std` + `libc` only), strict no-alloc-after-init, row-oriented storage on
object storage, own Viewstamped Replication for 1..N replicas (future phase),
deterministic core.

Compatibility requirement: **general PostgreSQL clients must work**, both at
the wire level (simple and extended query protocol, newest protocol version
3.2) and at the SQL-dialect level. Verified continuously by the external
conformance suite (`tests/external/run.sh`) against psql 18.4, psycopg 3, and
the latest MinIO.

Storage end state: **the durable database lives in object storage**. The
storage engine depends on one small, provider-neutral object-store contract
rather than naming or branching on AWS, MinIO, Google Cloud Storage, Azure
Blob Storage, or any other provider. Provider protocol/authentication adapters
must implement that contract below the storage engine; checkpointing, WAL,
compaction, recovery, and query execution must contain no provider-specific
paths. RAM is the first cache and local disk is the second cache. Both are
bounded, rebuildable, and disposable: losing either or both must lose no
acknowledged data in durable mode.

## Phases

| # | Phase | Milestone | Status |
|---|-------|-----------|--------|
| P0 | Scaffolding & memory core | Fixed-budget allocation w/ loud exhaustion, alloc guard, PCG32, config | **done** |
| P1 | Event loop | kqueue reactor, fixed event buffers, no-alloc waits | **done** (simulator driver deferred to the VOPR phase) |
| P2 | PG wire, minimal | psql connects; protocol 3.0 **and 3.2**, SSL/GSSENC probes, NegotiateProtocolVersion | **done** |
| P3 | SQL front + in-memory engine | Lexer/parser/eval with PG semantics; CREATE/INSERT/SELECT/UPDATE/DELETE, ORDER BY w/ PG null ordering, LIMIT | **done** |
| P4 | WAL / journal + recovery | Single preallocated journal, CRC-32C + monotonic LSNs, F_FULLFSYNC, kill -9 survives | **done** |
| P5 | Object-storage client | Hand-rolled SHA-256/HMAC/SigV4 (official AWS test-suite vectors) + HTTP/1.1; verified against MinIO | **done** |
| P6 | Driver compatibility | Extended query protocol incl. binary parameters, named statements/portals; functions & aggregates; psycopg 3 suite passes | **done** (`pg_catalog` tables themselves still absent) |
| P7 | Object storage is the database | CHECKPOINT + auto-checkpoint snapshot SSTs + CAS'd manifest; cold start from a wiped disk; WAL truncation; heap compaction; object GC | **done** (snapshot model — see "Deviations") |
| P8 | External conformance suite | psql 18 golden tests (dialect, SQLSTATEs, extended), raw wire probes, psycopg, durability + cold-start scenarios; 12/12 pass | **done** |
| P9 | Transactions | BEGIN/COMMIT/ROLLBACK, READ COMMITTED, REPEATABLE READ, and SERIALIZABLE snapshots, READ ONLY enforcement, commit-LSN-keyed row versions in resident staging and object SSTs, waitable writer/catalog/row/table locks with deadlock detection, WAL-batch-at-commit, transactional DDL | **done** |
| P10 | VSR multi-replica | Sans-io Replica state machine (normal op + view change), TCP transport (`vsr::cluster`), wire codec; 3-node cluster replicates and fails over | **done** (live psql write-routing into the cluster is the remaining productionization step) |
| P11 | VOPR hardening | Deterministic whole-cluster simulator (`sim`) with loss/reorder/dup/delay/crash/partition; found and fixed two real consensus bugs (B-009, B-010); reproducible from a seed | **done** |
| P12 | Compatibility polish | SCRAM-SHA-256 + cleartext auth, GROUP BY/HAVING/joins/subqueries, `pg_catalog` + `information_schema`, binary result format, portal max_rows, NOTICE, more types (date/timestamp/uuid/bytea), differential suite vs real PostgreSQL 18. | **done** (the TLS decision, deferred here, was later resolved in Stage G: isolated rustls to the object store) |
| P13 | Full PostgreSQL fidelity | Strict differential/sqllogictest fidelity (no papering over gaps): arbitrary-precision **NUMERIC** (base-10000, PG numeric.c representation, exact division scale), plan-time semantic type analysis (42883 before scanning), **correlated subqueries + EXISTS/NOT EXISTS** (scalar/IN/EXISTS, streaming + materialized paths), **subqueries in FROM-less SELECT** and `SELECT *` single-column subqueries, **INSERT ... SELECT** (materialize-then-insert, self-insert safe), exact **`x IN (subquery)`** empty/all-NULL/operand-type semantics, and **DISTINCT aggregates** (`count/sum/avg/min/max/string_agg(DISTINCT ...)`), **`string_agg`**, **date arithmetic** (`date ± int`, `date - date`), **derived tables** (`FROM (SELECT ...) alias`, materialized; compose with WHERE/aggregates/ORDER BY/joins), **non-recursive CTEs** (`WITH`, expanded into derived tables), **GROUP BY/aggregates as a row source** (derived tables, CTEs, set-op branches, and INSERT ... SELECT over grouped queries), **aggregate ORDER BY** (`string_agg(x ORDER BY k)`), **durable CREATE VIEW/DROP VIEW** (registry + WAL + manifest; expanded as a derived table at query time; columns validated at creation), and **durable CREATE INDEX/DROP INDEX** (catalog + WAL + manifest, composite UNIQUE enforcement via 23505; provider-neutral persistent key generations for equality and range probes, plus bounded RAM/disk acceleration; DROP TABLE cascades to indexes), and **DML on auto-updatable views** (rewritten onto the base table). The current differential and SQLLogicTest corpora have zero accepted divergences in supported blocks; unsupported SQL is capped by a CI ratchet rather than accepted as an unmeasured green result. That is a regression floor, not a claim that PostgreSQL's full SQL, catalog, wire, locking, replication, and tooling surface is complete. The compatibility wave and structural storage gaps below are the live completion list. In durable mode, top-level ORDER BY/DISTINCT/DISTINCT ON, non-lateral derived/CTE/view rows, set-operation multisets, scalar-subquery cardinality state, run-probed IN subqueries, recursive CTE all/work tables, lateral subqueries/functions, and RIGHT/FULL match maps now use bounded immutable external runs over the provider-neutral block store, with RAM and disk serving only as caches. Grouped-aggregate group-key sorts, ARRAY subqueries, set-subquery forms carrying their own final ORDER/LIMIT, and windows (partition-at-a-time over an external key sort, values restitched from an ordinal-stable win run) all run through the provider-neutral run stack in durable mode, closing B-006; a single window partition larger than the shared arena still errors 54000 rather than truncating. | **in progress** |

| P14 | Client/tooling & datatype fidelity | Driver- and tool-facing fidelity from a fresh audit of the wire/SQL/CLI/SDK surface: accept the common session GUCs real drivers set (`extra_float_digits`, `client_min_messages`, `bytea_output`, `lock_timeout`, `row_security`, zero-valued `statement_timeout`/`idle_*`), casts with type modifiers (`x::varchar(10)`, `CAST(x AS numeric(8,2))` — truncation applied), `SET`/`SHOW TRANSACTION ISOLATION LEVEL` and `SHOW ALL`, distinct **smallint/real/varchar/char** types (own OIDs/names/typmod, `varchar(n)`/`char(n)` length enforcement, 22003/22P02 surfaced), **SERIAL/bigserial/smallserial** (owned named sequences), **INSERT ... ON CONFLICT** (`DO NOTHING`/`DO UPDATE`, `excluded.*`), `JOIN ... USING`, and **named/DST time zones** (`SET timezone='America/New_York'` etc.; per-timestamp offset+abbrev via POSIX DST rules; ~25 IANA zones + `Etc/GMT±n` + bare numeric offsets; JDBC/psql now connect and introspect from any zone), and **table constraints** (multi-column PRIMARY KEY / UNIQUE, CHECK, and FOREIGN KEY — durable in the table catalog, enforced on INSERT/UPDATE/DELETE with PG-matching SQLSTATEs; parent-side NO ACTION/RESTRICT, with CASCADE/SET-actions rejected loudly pending a follow-up — see B-029). and **join/DML breadth** (RIGHT and FULL OUTER JOIN; `UPDATE ... FROM` and `DELETE ... USING`; NATURAL JOIN and multi-join RIGHT/FULL rejected loudly pending follow-up — see B-030). and **subtransactions** (SAVEPOINT / RELEASE / ROLLBACK TO SAVEPOINT — the transaction undo log records every row write with its prior image so nested rollback is byte-exact vs PostgreSQL; see B-031). and **window functions** (row_number/rank/dense_rank, lag/lead, and aggregate windows with PARTITION BY / ORDER BY and the default frame — running-with-peers or whole-partition; see B-032). and the **`time`** and **`interval`** types (time-of-day; and interval with months/days/micros fields, verbose parse, PG-exact output, and date/timestamp/interval arithmetic with calendar-month clamping — B-034). and **`json`/`jsonb`** (json verbatim; jsonb parsed and canonicalized — sorted/deduped keys, canonical numbers; `->`/`->>` accessors — B-034). and **one-dimensional arrays** (`ARRAY[...]`/`'{...}'::elem[]`, subscripting, `= ANY/ALL`, `array_length`/`cardinality`, element-wise ordering — B-034, done). and **range types** (`int4range`/`int8range`/`numrange`/`daterange`/`tsrange`/`tstzrange` — canonical-text `Datum::Range`, constructors, text cast, `lower`/`upper`/`isempty`/`lower_inc`/`upper_inc`, the `@>`/`<@`/`&&` predicate operators, value-based comparison/ordering `= <> < <= > >=` including `ORDER BY`/`GROUP BY`/`DISTINCT`, the set operators `* + -`, the positional predicates `<< >> &< &> -|-`, and the functions `range_merge`/`lower_inf`/`upper_inf`; storage/WAL/wire I/O; B-047, done). and **interval/symbolic-date arithmetic** (`interval * / number` with PostgreSQL's fractional-month/day spill, `justify_hours`/`justify_days`/`justify_interval`, and `age(a[, b])` — the symbolic calendar interval; B-049, done). and **FROM-item column-alias lists** (`(subquery) AS v(c1, c2, …)` and `func(args) AS g(c)`, so `VALUES (…) AS v(cols)` works in `FROM`; B-050, done). and **aggregate `FILTER (WHERE …)`** (plain/grouped/DISTINCT/windowed; B-051, done). and **value/positional window functions, `AT TIME ZONE`, `SIMILAR TO`, `LIKE ANY/ALL`, ordered-set aggregates (`percentile_cont`/`percentile_disc`/`mode`), `make_interval` with named arguments, and `GROUPING SETS`/`ROLLUP`/`CUBE` + `GROUPING()`** (B-053..B-060, done). and **bit-string types** (`bit(n)`/`varbit`, `B'…'`/`X'…'`, operators/casts — B-061, done). and **multirange types** (B-062, done). and **`regexp_matches`** (capture-group tracking in the regex engine, SRF in SELECT and FROM — B-063, done). and **`WITH RECURSIVE`** (fixpoint materialization), **full regex quantifiers** (bounded `{m,n}`, non-greedy, PostgreSQL length-preference), **`regexp_replace` backreferences `\1`–`\9`**, and **`string_agg(DISTINCT x … ORDER BY x)`** (B-064, done). and the **B-066 fidelity sweep** (transactional view/index DDL visibility — B-016; NATURAL JOIN + real USING merge semantics; qualified star `t.*`; ORDER BY ordinals through stars; WITH before set operations + views in set-op leaves; GROUP BY/HAVING/DISTINCT in subqueries and EXISTS; DISTINCT over grouped output; FROM-less aggregates; WHERE-level correlated subqueries in grouped/window queries; ANY/ALL/SOME over subqueries; the full to_char numeric code set; fractional-second typmods; FK referential actions CASCADE/SET NULL/SET DEFAULT; explicit window frames ROWS/RANGE/GROUPS; window functions over grouped queries and with DISTINCT — residual loud-error gaps tracked as B-067, closed by the **B-068 sweep**: RIGHT/FULL JOIN in any chain position, set-returning functions composed with aggregates/grouping/DISTINCT/ORDER BY/LIMIT, frame EXCLUDE, windows over GROUPING SETS, `count(t.*)`, correlated subqueries in every clause position of grouped queries (including inner queries that group over the outer reference), parenthesized set-op branches with ORDER BY/LIMIT, and `bytea_output = escape` — remaining loud-error gaps in B-069, then closed by the **B-070 sweep**: records as first-class values (`t.*`, `ROW(...)`, `row_to_json`/`to_jsonb`, record comparison), correlated subqueries in window select lists / ORDER BY / derived-table columns, `DISTINCT ON`, `array_agg`, and the array set operators `@> <@ &&`, plus latent fixes to UNION ALL ordering and untyped-NULL set/VALUES type unification, then the **B-072 sweep**: the single-column JSON set-returning functions (`json_object_keys`/`jsonb_object_keys`, `json_array_elements`/`jsonb_array_elements`, `json_array_elements_text`/`jsonb_array_elements_text`) in both the select list and the FROM clause — the `json` variants preserving the input's key order, duplicates, and whitespace, the `jsonb` variants normalized; jsonb object-key ordering corrected to length-then-bytewise (PostgreSQL's storage order); JSON text-escape decoding in `->>`/`#>>`/`*_text`; scalar function-scan whole rows (`SELECT x FROM json_array_elements(j) x` yields the scalar, not a one-field record); and a silent output-truncation bug class — fixed-size 40-/64-/256-byte render buffers that silently cut long json/array/range/record values on output and `::text` casts, replaced with unbounded `Arena::alloc_str_display` and length-counted Display streaming — remaining unimplemented features in B-071), then the **B-073 sweep**: the two-column `json_each`/`jsonb_each`/`json_each_text`/`jsonb_each_text` family (FROM-clause table functions with a two-column `(key, value)` `TableDef` and positional aliases; a `(key, value)` record per member in the select list), the `KEY` keyword corrected to non-reserved (usable as a column name, as in PostgreSQL), and the `json_to_text` jsonb re-serializer switched off a fixed 16 KiB buffer onto unbounded arena rendering (same silent-truncation class as B-072) — remaining unimplemented features (schemas, the full IANA time-zone database, and general composite-value field access / expansion `(record).field` and `(record).*`) in B-071), then the **B-074 sweep**: composite-value field access `(record).field` (resolving a field of a `ROW(...)`, a table whole row, or a `json_each` record — with static field-type inference via a new `table_columns` resolver hook) and expansion `(record).*` (a new `SelectItem::RecordStar` wired through every projection/describe/count/FROM-less path and composing with set-returning functions, so `SELECT (json_each(j)).*` works), plus PostgreSQL's exact `XX000` error for selecting a field of a `ROW(...)` containing a bare unknown literal — leaving field access on a scalar-subquery record and record-typed derived-table columns as loud-error follow-ups, and a fuzzer-found projection-postponement/div-by-zero cost-model divergence tracked as B-075, then the **B-076 sweep**: the array-manipulation function family (`array_append`/`array_prepend`/`array_cat`/`array_remove`/`array_replace`/`array_ndims`/`array_dims`/`trim_array`/`array_to_json`) with PostgreSQL's polymorphic element-type promotion (a wider new element widens the whole array), plus two pre-existing array gaps the family exposed — array-to-array casts with a different element type (`ARRAY[1,2]::int8[]`) and `pg_typeof` reporting the real `integer[]`/`numeric[]` name instead of a bare `array`, then the **B-077 sweep**: the regular-expression string-function family — the regex forms of `substring` (`substring(str FROM posix_pattern)` and the SQL-regex `substring(str FROM sql_pattern FOR escape)` with `#"..."#` capture extraction), `regexp_like`, and `regexp_split_to_array` (`regexp_substr`/`count`/`instr` were already present), then the **B-078 sweep**: `regexp_split_to_table` and `generate_subscripts` (as both select-list SRFs and FROM-clause table functions) and `WITH ORDINALITY` on any table function (appends a 1-based bigint column, composing with the multi-column `json_each` family and positional column aliases), then the **B-079 sweep**: temporal `generate_series` (over timestamp/timestamptz/date, iterated by calendar addition, in both SRF and FROM positions and with `WITH ORDINALITY`), `date_bin`, and the scalar temporal functions `make_timestamptz`, `clock_timestamp`, and `isfinite`, then the **B-080 sweep**: multiple set-returning functions in one select list now expand in lockstep to the longest (shorter ones NULL-pad), and a bare-string-literal `ARRAY[...]` now infers `text[]` instead of `int4[]` at describe time, then the **B-081 sweep**: the encoding/hashing/bytea/quoting function family (`encode`/`decode` base64·hex·escape, `sha224`/`sha256`/`sha384`/`sha512` with a new FIPS-validated SHA-512, `get`/`set_byte`/`_bit`, `bit_count`, `convert_to`/`from`, `length(bytea)`, `quote_ident`/`literal`/`nullable`, `parse_ident`), the `OVERLAPS` period operator, and a root-cause fix letting `bytea` input accept the `escape` text form (`'abc'::bytea`) not just `\x` hex, then the **B-082 sweep**: the jsonb manipulation family (`jsonb_set`, `jsonb_insert`, `jsonb_strip_nulls`, `jsonb_pretty`, and the `-` / `#-` delete operators, with `||` coercing an unknown text literal to jsonb) plus a fuzzer-found `to_char(float8)` overflow fractional fix, then the **B-083 sweep**: the statistical-aggregate family (`var_pop`/`var_samp`/`variance`/`stddev_pop`/`stddev_samp`/`stddev` and the two-argument `corr`/`covar_pop`/`covar_samp`/`regr_*`, all also usable as window functions), with variance/stddev over integer/numeric inputs returning an **exact numeric** result via a new `numeric::var_stddev` mirroring PostgreSQL's `numeric_stddev_internal` (float8 inputs and the two-argument family fold in f64), the `percent_rank`/`cume_dist` window functions, `pg_size_pretty(bigint|numeric)`, the array form of `width_bucket(operand, thresholds[])`, and an incidental `pg_typeof`-of-NULL fix (it now recovers the argument's static type instead of reporting `unknown`, including aggregates over an empty group via a schema-only projection lookup), then the **B-084 sweep**: `EXTRACT`/`date_part` on intervals (PostgreSQL's `interval2tm` field decomposition, with `epoch` scaling a year by 365.25 days and a residual month by 30), interval comparison operators (`= <> < <= > >=`, and thereby `ORDER BY`/`GROUP BY`/`DISTINCT`/`min`/`max`, via the canonical `interval_cmp_value` microseconds), the `bit_and`/`bit_or`/`bit_xor` aggregates over integers and bit strings (also as window functions), and the scalars `num_nonnulls`/`num_nulls`, `array_fill`, `array_positions`, and the `isoyear` field of `EXTRACT`, then the **B-085 sweep**: implicit row constructors `(a, b, …)` (previously only an `OVERLAPS` period pair) parsed as `ROW(...)` outside an `OVERLAPS`, with PostgreSQL's three-valued, short-circuiting **row comparison** (`= <> < <= > >=`, NULL-propagating, distinct from the total order `ORDER BY` uses) and the **row null-test** (`(...) IS NULL` iff every field is null, `IS NOT NULL` iff every field is non-null), plus `substring(str FOR len)`. then the **B-086 sweep**: **named window definitions** (`WINDOW w AS (...)` with `OVER name`, the parenthesized copy form inheriting PARTITION BY and adding a missing ORDER BY, definitions referencing earlier ones, and PostgreSQL's 42704/42P20 restrictions — resolved entirely in the parser by a bounded lookahead, so the AST and executor see only inlined specs; `window` is now reserved except after `AS`), together with four pre-existing window bugs its verification exposed: window functions in a derived table / CTE / set-operation leaf were counted as plain aggregates and took the grouped path (wrong results, e.g. a `UNION` leaf returning the aggregate instead of the windowed rows) — fixed by distinguishing an aggregate *use* (no `OVER`) from an aggregate *name* via a new `Expr::is_aggregate_use`; a window function only in `ORDER BY` was never dispatched to the window path; a window function in a scalar/IN/EXISTS subquery was not routed to the row-source executor; and a correlated subquery whose body computes a window could not resolve its outer row (previously masked, returning a silently wrong value), plus an incomplete `Chained` column lookup that implemented four of `ColumnLookup`'s five methods and so rendered a single-column table function as a record. Remaining: full psql `\d <table>` (\dt works; \d table needs more pg_class/pg_attribute — B-033), and then the **B-088 sweep**: keyword classification — one flat `is_reserved` list had been standing in for PostgreSQL's four keyword categories, so identifiers were both over-restricted (`insert`, `values`, `set` rejected as column names, `insert` wrongly quoted by `quote_ident`) and under-restricted (`all`, `array`, `null`, `authorization` accepted as column names, and not quoted); replaced with a `keyword_category` table generated from `pg_get_keywords()` and applied at the 30 `ColId` positions, leaving the positions PostgreSQL keeps permissive (`t.col` and select-list aliases are `ColLabel`) alone — verified by sweeping all 494 keywords through `CREATE TABLE t(<kw> int)` against real PostgreSQL (same 101 rejected, same 393 accepted) and all 494 through `quote_ident` (exact match), closing B-087. then the **B-090 sweep**: `CREATE TABLE t (LIKE source [INCLUDING ...])` — columns spliced in at the position the element was written (so `(z int, LIKE src, w text)` keeps PostgreSQL's order), always carrying name/type/NOT NULL, with `DEFAULTS`, `CONSTRAINTS` (CHECK), `INDEXES` (PRIMARY KEY, UNIQUE and secondary indexes), `IDENTITY`/`GENERATED` and `ALL` each adding a group and `EXCLUDING` removing one; foreign keys are never copied and copied constraint/index names are regenerated from the new table, both as PostgreSQL does; the four options describing properties this engine does not model are rejected with 0A000 rather than silently dropped. Closes B-089, and with it all 494 PostgreSQL keywords now behave identically in a `ColId` position, then the **B-091 fix**: the projection-postponement cost model ignored the implicit casts `GREATEST`/`LEAST`/`COALESCE` place on their arguments, so near PostgreSQL's 10-operator threshold pos3ql decided the opposite way and surfaced a division-by-zero or overflow for a row that sorts past the LIMIT and that PostgreSQL never evaluates; the per-operator costs were read straight out of `EXPLAIN` rather than guessed, closing B-075 and bringing the fuzzer to its first fully clean 40-seed sweep (16,000 statements, zero divergences), then the **B-094 fix**: fractional-second precision above 6 was clamped silently because the parser had no channel to the responder — it now records parse-time warnings that the engine drains and emits before the statement runs, so `timestamp(7)` and its siblings report PostgreSQL's `precision reduced to maximum allowed, 6` (closing the last open B-071 item). then the **B-095 fix**: a column's one-byte stored type code was ambiguous between the multirange and array families, so an `int4[]` or `bool[]` column replayed from the journal (or the checkpoint) came back as a multirange with its values gone — silent data loss on any restart; the families are rebased clear of each other and of every retired code, so older data fails loudly instead, guarded by a round-trip/collision unit test and a durability check that both fail on the parent commit. Remaining: schemas and the full IANA time-zone database (B-071), the repeated DDL warning PostgreSQL emits twice (B-093), then the **B-096 sweep**: the `timetz` type (instant-then-zone ordering as `timetz_cmp`, session-zone resolution for a zoneless source, casts, `± interval`, `extract`, typmod, storage/wire round-trip), which uncovered three pre-existing bugs — a **server crash** on `SELECT DISTINCT` over a time, interval, json, range, multirange, bit string, uuid or numeric (the sort path kept its own stale copy of the projected encoding's tag table and hit `unreachable!()`), `'12:00:00-05'::time` rejected because only `+`/`Z` suffixes were stripped, and `WHERE time_col > 'literal'` failing for want of a `coerce_unknown` arm, then **B-097**: the parenthesis-less SQL-standard functions (`current_date`, `current_timestamp`, `current_user`, …) had become syntax errors — a regression B-088 shipped, since every one of them is a reserved word and the new reserved-word test ran before the list that recognizes them, with no corpus probe naming any of them to catch it — fixed by ordering the test last, and completed with `current_time`/`localtime`, the optional precision argument, and a session-zone-aware `localtimestamp`, then the **B-104 surface sweep**: after that regression shipped unnoticed, the expression and statement surface was enumerated from the routers and compared form by form against PostgreSQL, fixing the bare `user` keyword, `LIMIT ALL`, `INSERT ... DEFAULT VALUES`, `POSITION`'s output-column label, and `COALESCE`'s result type (it took its first argument's type, so `coalesce(NULL, 1)` described as text) — and keeping the sweep as a fifteenth corpus, `14_surface.sql`, which fails on the commit before it. Gaps it found that need their own work are tracked: `TRUNCATE` (B-098, blocked on a persistent identity high-water mark), the `INTERVAL '1' DAY` qualifier (B-099), window functions in a FROM-less SELECT (B-100), statement-stable `now()` (B-101), the `case` column label on desugared `IS` forms (B-102), and the `name` type (B-103). The `query.rs` split continues: `WITH` expansion, recursive-CTE materialization and the AST substitution they rest on move to `query/cte.rs`, and FROM-clause scope resolution to `query/scope.rs`, and source-row enumeration to `query/scan.rs`, leaving `query/mod.rs` at 5731 lines, down from 10787 across seven extractions (set operations, window functions, aggregates, CTEs, scope resolution, row scanning) — no file in the crate now exceeds 5731 lines. Then **B-105**: the timestamp family all read the wall clock afresh, so two `now()`s in one statement could differ and none meant what PostgreSQL means; they now anchor as PostgreSQL anchors them — `now`/`current_timestamp`/`transaction_timestamp` and the `current_*` family to the transaction, `statement_timestamp` to the statement, only `clock_timestamp` live — closing B-101 and restoring the corpus probe that had to be withdrawn for flaking, and **B-106**: a window function in a FROM-less SELECT is no longer rejected — such a query *is* one row, so it is rewritten to select from a one-row derived table and handed to the ordinary scanning path, which needs neither a synthetic scope nor a second copy of the window family's semantics (closing B-100), then a **type × operation sweep** — every supported type through fifteen operations (cast, equality, ORDER BY, DISTINCT, GROUP BY, min/max, count, UNION, coalesce, CASE, array_agg, IS NULL, nullif, IN, pg_typeof), 434 probes against real PostgreSQL — which found that **`array_agg` was returning integers** for every element type arrays cannot carry (a `.unwrap_or(Int4)` standing in for an unrepresentable value: `array_agg` over a `time` gave `{250327040}`), now a loud 0A000 (B-107, remaining element types B-108), and that an array element needing quotes was written bare unless it was text, so a timestamp array printed a literal PostgreSQL would read back as two elements (B-109). The split continues: row materialization for GROUP BY / DISTINCT / ORDER BY moves to `query/materialize.rs`, leaving `query/mod.rs` at 5307 lines — down from 10787 across eight extractions, and every module in `query/` now under 1300 lines but the root. Then **B-111**: `json` compared equal where PostgreSQL has no operator at all — it declines because two documents differing only in whitespace or key order are the same value but not the same text — so the operator now declines too, `jsonb` still comparing; the `DISTINCT`/`GROUP BY`/`ORDER BY` forms that sort by the encoding rather than the operator remain (B-112), and `min`/`max` over `json`, `jsonb`, bit strings, ranges and multiranges now decline as PostgreSQL does (B-113 — whose first recording also blamed `boolean` and `uuid`, which already worked; the sweep's report had slipped a row against its probe names, and every claim was re-checked one type at a time before the fix). `exec.rs` is now a module directory too: constraint enforcement — uniqueness, NOT NULL, CHECK, and both sides of a foreign key including the referential actions that re-enter DML — moves to `exec/constraints.rs`, leaving `exec/mod.rs` at 4285 lines. Also **B-110**: an array constructor kept its `array` column label through a cast. `sql/mod.rs` follows: its 2632-line inline test module becomes `sql/tests.rs`, leaving the engine itself at 1468 lines — the file was never mostly engine. Then `parser.rs` becomes a directory too, its data-definition statements — `CREATE TABLE` with its constraints and `LIKE` clauses, `CREATE INDEX`, `CREATE VIEW` and the `DROP` family — moving to `parser/ddl.rs` as a second `impl Parser` block; probing those paths afterwards found that a `DROP` reported `relation` where PostgreSQL names the kind, and that `DROP INDEX` raised 42P01 where PostgreSQL raises 42704 (B-114). `eval/mod.rs` follows, its casting machinery — one arm per target plus the parsers the harder ones need (bit strings, uuid, bytea's two input forms) — moving to `eval/cast.rs`; round-tripping every type through a cast afterwards found two gaps, both recorded: a range does not quote a bound carrying a space (B-115, the same rule as the array elements fixed in B-109, but decided when the canonical stored text is built rather than when it prints) and `char(n)` keeps its blank padding through a cast to text (B-116). `exec/mod.rs` gives up its table-definition building too — column metadata, PRIMARY KEY/UNIQUE, CHECK reference validation and FOREIGN KEY resolution — to `exec/ddl.rs`, leaving it at 3788 lines. Across the session the largest file has gone from 10787 to 4444 and nothing exceeds it. `query/mod.rs` then gives up its subquery machinery — the uncorrelated evaluation done once up front, the correlated re-evaluation per outer row, and the scalar/IN/EXISTS/ARRAY forms — to `query/subquery.rs`. Sweeping that surface against PostgreSQL turned up a NULL inside a row being invisible to `IN` (B-117) and row-constructor `IN (subquery)` being rejected outright (B-118), both fixed, plus a golden expectation left stale by an earlier fix (B-119). Grouped execution (`GROUP BY`, grouping sets, `HAVING`) and qualification planning (conjunct order, pushdown, canonicalization) then follow into `query/group.rs` and `query/plan.rs`, leaving `query/mod.rs` at 3147 — down from 10787 where the session started. Sweeping the grouping surface found `GROUP BY <n>` not reading as a select-list position (B-120) and the ungrouped-column error not naming its column (B-121), both fixed, and three more recorded open (B-122, B-123, B-124). A sweep for doc comments stranded by the earlier splits — a moved function's doc silently reattaching to whatever followed it — returned eleven, each moved back to what it describes or dropped where the subject was already documented accurately. `exec/mod.rs` then gives up static type analysis (what a query's columns are before a row exists) to `exec/describe.rs` and the self-describing row encoding to `exec/projected.rs`, leaving it at 2220. Sweeping that surface found a non-boolean being accepted — and returned — wherever a boolean belonged (B-125), fixed, plus two recorded open (B-126, B-127). `parser/mod.rs` then gives up expression parsing (precedence climbing and the prefix forms) to `parser/expr.rs` and the window clause to `parser/window.rs`, leaving it at 2152. Sweeping the expression grammar found `BETWEEN SYMMETRIC` unsupported (B-128), fixed, plus two more recorded open (B-129, B-130). `eval/mod.rs` then gives up scalar-argument reading and text building to `eval/args.rs` and the LIKE / SIMILAR TO / regex family to `eval/pattern.rs`, leaving it at 2555. Sweeping that surface found the `ESCAPE` clause of LIKE and SIMILAR TO unparsed (B-131), fixed, plus two recorded open (B-132, B-133). `query/mod.rs` finally gives up the set-returning functions — the ones written in the select list and the ones written in FROM — to `query/srf.rs`, leaving it at 2298. The sweep of that surface found `string_to_table` missing entirely (B-134), recorded rather than added. No file in the tree now exceeds 2555 lines, against 10787 at the start of the session, and the four files that began it — query, exec, parser, eval — are all now within the same band as the modules extracted from them. With the file sizes settled, the quality gates get the same treatment: dead-code detection, which `lib.rs`'s fictional public API had disabled crate-wide, is turned back on (B-136), and coverage is measured for the first time — across both test layers, since instrumenting only the in-process tests reports 59% and the wire protocol at 6% (B-137). Three open bugs close alongside: `string_to_table` (B-134) and the `|/`, `||/` and `@` prefix operators (B-130). A further batch then closes four more: an untyped literal now takes the type of an array operand it faces (B-129), the desugarings of `SIMILAR TO` and `OVERLAPS` no longer leak into the function router (B-132), a bare row constructor is no longer a field-access target (B-135), and an undefined operator is reported under the operator that was written (B-127). Two entries were re-examined rather than fixed and now say what is actually wrong: `smallint` has no runtime representation at all rather than merely widening under arithmetic (B-126), and undefined-function errors omit their argument types (B-138). Later, range bound quoting (B-115) is fixed at the canonicalization choke point: a bound is normalized to its element type's output form and quoted when that text needs it, so a timestamp range gains its time-of-day and round-trips PostgreSQL's own quoted output — surfacing a separate error-wording gap recorded as B-141. The typmod family of bugs (B-139, B-140, B-116's blocker) is then retired as a
class: `TypeMod` in `types.rs` is the decoded view of an `atttypmod`, with the
one `decode`/`encode` pair as the only place the integer encodings exist —
round-trip-tested against PostgreSQL's exact values — and every consumer
pattern-matches on meaning, so a site can no longer subtract a header the value
does not carry. The same change made `interval hour to minute`'s unspecified
precision (`0xFFFF`) an `Option::None` instead of a number a clamp would have
silently rounded to 6. And the differential's error-wording blind spot is
closed: `tests/external/differential_exact/` corpora compare the full ERROR
line (SQLSTATE and message), in both the local harness and CI's sharded run —
its first execution caught two real bugs (B-147). The widened pass then went after three more classes. Every SQLSTATE is now a
named constant — 199 inline five-character literals across 46 files became
`sqlstate::` constants covering all 40 conditions in use, and a source gate
(`tests/sqlstate_gate.rs`, proven to fire on a planted typo) keeps a raw
literal from compiling back in, so a typo'd code is no longer representable.
RowDescription now reports real atttypmods (B-149) — `ColDesc` carries the
modifier the `TypeMod` work made trustworthy, filled by PostgreSQL's rule
(table column: declared; cast: target's; computed: none) and guarded in the
psycopg driver suite, the one harness that can see the wire. And the
coverage-guided function sweep is now a corpus: every dispatch-table function
no prior corpus called, one canonical call each — its first run found four
divergences (B-148), all fixed. B-138 is then fixed in the next batch alongside `substring(x SIMILAR p ESCAPE e)` (B-133), which turned out to be a parser gap alone — the extraction already existed under SQL:1999's `FROM p FOR e` spelling, so the two syntaxes now reach one implementation. `json` is then refused as a DISTINCT, GROUP BY or ORDER BY key (B-112), at one site rather than the three the entry expected. Two entries were re-examined against the server and found wider than recorded — range bound quoting is missing on input as well as output, so a range literal copied from PostgreSQL does not load (B-115), and `char(n)`'s padding lives in the value, making `length` and equality wrong rather than only a cast (B-116) — and the second turned up a third: `format_type` ignored its modifier argument, so every column read back as an unconstrained type (B-139) — the entry that recorded it blamed the catalog, which turned out to report `atttypmod` correctly, so checking the claim first is what kept a wide `ColDesc` change from being built on a false premise. Fixing it exposed B-140: the temporal types encode that modifier with a 4-byte header PostgreSQL does not use. B-116 then closes structurally: a `Datum::Bpchar` variant carries the padded text, so PostgreSQL's split falls out of the type — output functions, `LIKE`/regex and `octet_length` see the raw padded value while casts, comparisons and text-taking functions see it stripped — with the storage format unchanged and the behavior pinned by corpus `32_bpchar` (112 divergent lines against its parent commit) and psycopg text+binary wire assertions; bare `char` now means char(1), `character varying` parses, over-length all-space excess truncates silently on the column write path, and DISTINCT/GROUP BY dedup bpchar keys by stripped text. The grouping cluster follows: grouping keys now match by resolved column identity (`a` and `t.a` are one key; stars expand into grouped selects; the 42803 rule reaches HAVING and ORDER BY), aggregates in ORDER BY fold with the group (B-122, B-123), the DDL precision-clamp warning is duplicated as PostgreSQL duplicates it (B-093), `::regtype` resolves names and OIDs to canonical SQL type names (B-146), and the `name` type exists (`ColType::Name`, OID 19, 63-byte truncation, identifier functions infer it; B-103) — with `pg_typeof` preferring the static type whenever it is consistent with the runtime value. B-124 (grouping-set tie order) is re-recorded as unmatchable by design: PostgreSQL's order is hash-table emission under an unstable sort. The sqlstate gate was found blind to rustfmt's multi-line `sql_err!` layout — 56 raw codes had slipped through; all are constants now and the gate catches the bare-code-on-its-own-line form (proven on a plant). The types cluster completes with `Datum::Int2` (B-126: smallint is a real runtime type — narrow arithmetic with its own 22003 bounds, honest OID 21 / 2-byte binary wire, silent-truncating shifts, 42725 for the genuinely ambiguous int2 overloads, and the unary-minus-vs-cast precedence fix `-32768::int2` exposed) and eleven new array element types (B-108: int2/time/timetz/interval/uuid/bytea/json/jsonb/varchar/bpchar/name — `array_agg` reports the static element type, and the two duplicated per-element name tables collapse into one). TRUNCATE lands with the durability it was blocked on (B-098): serial columns are real sequences (`Table.serial_last` — advanced only by default assignment, never rewound, rollback-surviving), journaled as absolute-position WAL records, checkpointed as additive manifest lines, floored against stored rows at startup; TRUNCATE removes rows transactionally, closes over foreign keys (0A000 / CASCADE with NOTICEs), and RESTART IDENTITY resets sequences through the DDL-undo machinery. Sequence survival across kill -9 with an empty table is a run.sh assertion.| in progress |
| P15 | Differential CI at scale | Wire the existing differential + fuzz machinery into CI as its own workflow (`differential.yml` → `tests/external/ci_diff.sh`): a real PostgreSQL 18 service (C collation, **UTF8** encoding to match pos3ql and vanilla PG) is the oracle; the suite replays the vendored sqllogictest corpus and the generative fuzzer against both engines and diffs rows + SQLSTATEs. Hardened so a pathological query can never wedge CI: **predicate pushdown** removes the O(Nᵏ) multi-way-equi-join blowup that hung the run for 45+ min (B-037, `select5` now seconds, divergence 0); the cross-join order uses predicate-shape selectivity so an equality-connected component is established before independent broad filters multiply it (B-214); a per-statement `statement_timeout` guard is set where the engine honors it (B-038); and the job carries a hard `timeout-minutes` ceiling. The comparator decodes text-returned-as-bytes losslessly, and both sessions pin `TimeZone='UTC'`, so neither a server-encoding nor a host-timezone quirk can masquerade as a data divergence. The sqllogictest replay and generative fuzzer each run against a freshly-restarted pos3ql: the curated corpora leave catalog objects alive, while a sqllogictest file consumes all 64 configured table and value-index slots, so phase isolation is required for the bounded catalog rather than treating the resulting setup failures as unsupported. Fuzz failures remain loud, and its error-timing/semantic divergences were driven to **zero** (B-039 → B-065: projection postponement, qual-ordering and plan-time-simplification fidelity, correctly-rounded numeric→float8, float8 to_char) — `FUZZ_BUDGET=0`, 9 seeds all clean. CI is deduplicated (one run per ref) and caches the Rust build. | **done** |

Phase discipline: fine-grained commit per task; PLAN.md and BUGS.md updated in
the same commit series as the phase they describe; no phase numbers or bug IDs
in code or code comments (the "why" goes in commit messages).

## Object-storage LSM roadmap (realizing the original vision)

The phases above built a **RAM-resident** database: every live row is in one
fixed in-memory heap (`storage::RowHeap`, `memtable_bytes`), durability is the
local WAL, and object storage holds **full/delta checkpoint snapshots** behind a
CAS'd manifest (see *Deviations* below). Three things still separate this from
the founding vision of *object storage is the database, with a local disk/memory
cache in front of it*:

1. **The working set must fit RAM.** A full memtable fails loudly
   (`storage/mod.rs`: *"flush to object storage is not implemented yet"*).
2. **There is no read-through cache.** `block_cache_bytes` / `disk_cache_bytes`
   are declared in `config` but wired to nothing.
3. **The bucket is a snapshot target, not a block-addressable backing store.**
   SSTs are read whole on cold start, never a block at a time on the query path.

This roadmap closes that gap. Structural cues are taken from **Loki** (object
storage as the system of record; immutable content-addressed chunks; a small
cacheable index shipped to the bucket; multi-tier read caches; ingester
WAL-then-flush; a compactor for retention/GC) and **TigerBeetle** (a fixed-size
checksummed *block grid*; a superblock / manifest-log root updated by CAS; a
statically-allocated block cache; amortized "paced" compaction; deterministic
fault-injected simulation).

**Invariants every stage keeps** (unchanged from the founding discipline):
static memory (no heap after startup; pool exhaustion is a loud error); no
silent fallback or no-op; **PostgreSQL fidelity is frozen** — the differential +
sqllogictest + fuzzer stay green through every stage (storage is invisible to
SQL semantics); all storage I/O behind the `io` traits so the simulator can
drive it; every block and object is checksummed and a mismatch is fatal; one
runtime dependency (`libc`), TLS the single flagged exception (resolved in
Stage G as isolated rustls behind the budgeted guard scope).

The hard dependency chain is **A → B → C → D → E → F**; **G** and **H** run in
parallel once **A** exists; **I** (object-storage-adaptive execution) builds on
the block-granular read path (**C**) and the snapshot read path (**F**). See
[BUGS.md](BUGS.md) B-075 for the one open correctness caveat in the current
executor (evaluation-order of error-raising expressions vs sort/limit),
independent of this work.

### Stage 0 — codebase organization & detection tooling (prerequisite, parallelizable)

Two hygiene tracks that make room for the storage subsystems. Neither hard-blocks
the stages (which land in new directories), but both run alongside the early ones.

**Module structure.** `src/sql/` is ~75% of the tree in 23 flat files, four of them
4k–11k lines (`query.rs`, `eval.rs`, `exec.rs`, `mod.rs`; the `call()` dispatch
`match` alone is ~3.3k lines). New subsystems each get their own directory
(`src/store/`, `src/cache/`, `src/lsm/`, `src/sched/`) — the flat `sql/` layout is
not repeated. The existing monsters are split incrementally, one file per PR,
diff-gated (the differential + fuzzer + tests are the guardrail): `query.rs →
query/{scope,scan,join,cte,setop,aggregate,window,group,project,view_dml,select}`,
`eval.rs → eval/{hooks,core,call/ (by category),operators,cast,like,series}`
(splitting `call()` also removes its debug-build stack-frame risk), `exec.rs →
exec/{ddl,infer,row,record}`.

**Detection tooling — established tools, not hand-rolled** (checked against the Rust
ecosystem rather than reinvented):
- *Duplicates:* **jscpd** v5 (Rust-tokenizing Rabin-Karp copy-paste detector),
  gated in CI via `tools/check-dups.sh` against `.jscpd.json` (a ratchet threshold).
  The tree is ~0.2% duplicated (three ~25–47 line clones, all fidelity-critical hot
  paths — the INSERT/UPDATE row-fill in `exec.rs`, the grouping-set scan closure in
  `query.rs`, and the sign/currency match arms in `to_char.rs` — baselined, not yet
  extracted). A fourth, the byte-identical WAL/checkpoint on-disk type-code map, was
  unified into a single `ColType::code`/`from_code` (a single-source-of-truth fix).
- *Dead code:* rustc's own `dead_code` + `#![warn(unreachable_pub)]` are the precise
  long-term path, but are blind to the library's `pub` surface until it is curated
  down to what `main.rs`/tests use (a ~2000-edit `pub → pub(crate)` pass —
  mechanical, incremental, and an encapsulation win). Until then,
  **cargo-workspace-unused-pub** (rust-analyzer SCIP index; semantic, so it catches
  `pub` *methods*, not just free items) is the audit tool: `rust-analyzer scip . &&
  cargo workspace-unused-pub`.
- Both tools **surface candidates for judgment** — zero consumers can mean cruft OR
  intentional public API / protocol-and-format documentation / a companion
  accessor, and each carries its own false positives (`#[test]` entry points, trait
  impls, doc-comment references). A candidate is resolved by **wiring it in,
  removing it, or recording why it stays** — never deleted by consumer count alone.

**Stage 0 is done.** The four monster files are split — `query.rs` into
`query/{scope,scan,cte,setops,aggregate,window,group,plan,materialize,subquery,srf}`,
`eval.rs` into `eval/{cast,args,pattern,operators,funcs/*}`, `exec.rs` into
`exec/{ddl,constraints,describe,projected}`, and `sql/mod.rs`'s engine tests into
`sql/tests.rs` — taking the largest file in the tree from 10787 lines to 2555 and
leaving the four originals in the same band as the modules extracted from them.
Every split was diff-gated by the differential, the fuzzer and the tests, and each
turned up fidelity bugs in the code it moved (B-086 through B-140).

The dead-code path this stage called "precise but blind until the `pub` surface is
curated down" is now taken: `lib.rs` exported 14 `pub mod` for an API whose only
consumers are `main.rs` and two integration tests, which made rustc treat the whole
crate as reachable and disabled `dead_code` everywhere. Seven modules are now
`pub(crate)`, `#![warn(unreachable_pub)]` keeps the surface from drifting back open,
and the lint found and removed a genuinely dead accessor. `cargo-workspace-unused-pub`
is no longer needed as a substitute. Coverage was added alongside it
(`tools/coverage.sh`, ~78% line across both test layers, gated in CI) — measuring
only the in-process tests reports 59% and the wire protocol at 6%, because the
corpora and sqllogictest blocks drive the server binary as a subprocess.

Earlier: jscpd is gated in CI (`tools/check-dups.sh` + `.jscpd.json`), and the
dead-code audit's 15 candidates were adjudicated — nine genuinely-dead items removed
(superseded `storage` rollback/drop helpers, `is_frozen`, `FixedMap::contains_key`,
`Pool::iter_handles`, `Responder::with_render`, `sigv4::write_signed_headers`), the
rest being tool false positives. Remaining: extract the three baselined clones as
they're touched, run the dead-code audit periodically (its false positives make it a
poor hard gate), and split the four monster files incrementally.

### Stage A — the block grid: a checksummed, content-addressed block store

Introduce the one abstraction everything stands on: a fixed-size, self-describing,
checksummed **block** (`header { checksum, block_type, block_id, lsn, len }` +
payload; start at 256–512 KiB), and a `BlockStore` seam with a local backend and
an object-storage backend. Blocks are **immutable and content-addressed** (key =
content hash, Loki-chunk style), so writes are idempotent, retries are safe, and
only the root needs CAS. SST data/index/filter blocks, the manifest log, and WAL
segments all become blocks in the grid (TigerBeetle *Grid*), verified on read and
used in place. Work: `Block` layout + `BlockId`; `trait BlockStore`; a static free
set / ref-map for the local grid; re-express the current SST writer/reader in
terms of blocks, behavior-preserving. **Milestone:** existing checkpoint/cold-start
round-trip passes with every persisted byte a verified block; a flipped byte fails
loudly. **Risk:** block size (latency vs. amplification) and object-per-block vs.
pack-many (S3 request cost) — start object-per-block.

**Started.** `src/store/` holds the block format and the `BlockStore` seam: a
256 KiB block carrying `checksum | block_type | lsn | len | block_id` and a
payload, identified by the SHA-256 of that payload. Content-addressing is what
makes a re-written block the same block, so a retry after an ambiguous failure
costs nothing and only the root needs CAS. Both a CRC-32C and the identity hash
are kept — the CRC catches damage cheaply on every read, the hash is what stops a
bucket returning a *different* valid block from being believed, which the tests
demonstrate by re-checksumming a substituted payload. Encoding writes into a
caller's buffer and decoding borrows from one, so a block lives in the pool its
owner reserved.

Both backends now sit behind the trait. `store/object.rs` keeps one object per
block under a key prefix and writes with no precondition — the key *is* the
content, so a conditional create would turn a harmless retry into an error a
caller would have to interpret. It verifies a read against the name it was
fetched under rather than only against the block's own header, which is the case
a checksum cannot cover: being handed a different, intact block. `contains`
fetches the header alone, so asking whether a block exists does not cost what
reading it would. `store/memory.rs` is the RAM tier and the test double: a
reserved slab plus a `FixedMap` from identity to extent, where a full store
raises and keeps everything it holds rather than reclaiming space by dropping a
block — that distinction between a store and a cache is what tells a caller
whether it still owes the bucket an upload, and reclaiming belongs to Stage B in
front of this.

Stage A closed out since: the SST writer/reader is expressed in blocks
(`store/sst.rs` — data, chain, filter, sparse index, and roster blocks named
by identity through `BlockStore`), and the free set / ref-map turned out to be
the wrong tool for this design rather than a missing piece. Blocks are
immutable and content-addressed, so nothing needs per-block refcounting: the
published manifest is the single root, and mark-and-sweep GC reclaims whatever
it cannot reach (`collect_garbage`), while the local RAM and disk tiers are
pure caches that reclaim by CLOCK eviction — a lost local block costs a
re-fetch, never data.

### Stage B — the tiered read cache (RAM block cache + local disk cache)

Build the missing cache and make `block_cache_bytes` / `disk_cache_bytes` real —
the piece the founding "ClickHouse/Loki-style local cache" names. Two
statically-allocated tiers behind `BlockStore`: a **RAM block cache** (fixed
frames, CLOCK/CLOCK-Pro eviction — TigerBeetle's grid cache) and a **local disk
cache** (fixed-budget files with an in-RAM `FixedMap<BlockId, DiskSlot>` index and
CLOCK/LRU eviction — Loki's chunk cache + boltdb-shipper local store). Read path
becomes **RAM cache → disk cache → object-storage ranged GET**, with
hit/miss/evict counters surfaced. The disk cache is pure cache (always re-fetchable
from the bucket), so a torn disk-cache write is a miss, never data loss. **Milestone:**
a dataset whose hot set fits RAM but whose whole set does not is served mostly from
cache; the config knobs finally do something; hit ratio is visible.

**RAM tier started.** `store/cache.rs` wraps any `BlockStore` in a fixed set of
frames, drawn from the budget at startup, with CLOCK eviction — one referenced
bit per frame and a hand that clears bits until it meets one untouched since the
last pass. It approximates LRU closely enough here and costs a bit and a pointer,
where true LRU costs a list maintained on every hit. Writes go *through*: the
store decides first and the cache only remembers what the store accepted, so a
block the store rejected is never served. Frames hold payloads rather than framed
blocks, since the block was verified on the way in. `hits`/`misses`/`evictions`/
`insertions` are counted and readable, because a cache whose hit ratio cannot be
seen is one nobody can size. The disk tier is now built too. `store/disk.rs` is a preallocated cache file of
fixed slots with an in-RAM identity-to-slot index and the same CLOCK eviction,
one tier down — the RAM cache in front of it, the store or bucket behind. It is
sized once at startup like the WAL journal, so a slot write is only ever an
overwrite. Being pure cache is what lets it skip fsync: a slot torn by a crash
or rotted on the platter reads back as something other than the block the index
named, and that is a *miss* — the slot is dropped and the block re-fetched from
the store, so the caller never sees the damage. Identity, not the checksum,
catches a stale block a torn write left behind, since that block passes its own
checksum. A previous run's file is discarded on open rather than trusted.
Corrupt-slot reads are counted apart from misses, because a rising count is a
sick disk rather than a cold one.

The two tiers now stack. `store/tiered.rs` assembles RAM frames over the disk
file over a base store, sizing each tier from `block_cache_bytes` and
`disk_cache_bytes` — a `StackPlan::resolve` turns each byte budget into whole
units first, so a budget too small to hold one block is reported as undersized
(a likely typo the caller can refuse) rather than built as a cache that misses
on everything, and a budget of zero drops the tier entirely. Both tiers dropped
leaves the base store answering directly, which is exactly the RAM-only database
the earlier phases were, reached through the same seam. The base store is a type
parameter, so the identical stack sits over the object backend in the server and
over the memory backend under test; the assembled whole is still a `BlockStore`,
so a caller never learns how many tiers answered. The layering is an enum, not a
boxed trait, so no allocation or dynamic dispatch enters the read path.

This closes Stage B's structure: the knobs size real tiers and the read path is
RAM → disk → store. What remains before the stack is load-bearing is Stage A's
other half — re-expressing the SST writer/reader in terms of blocks — and then
routing the checkpoint/cold-start paths through `store::build` instead of the
whole-object SST reader/writer they use today. That last step is where storage
stops being additive and touches durability, and wants a session that can hold
the checkpoint path, the cold-start path and `tests/external/run.sh`'s
durability scenarios in view together.

### Stage C — a real SST: sorted data blocks + sparse index + bloom filter

Replace the whole-table SST with a **block-granular** SST so a read fetches only the
blocks it needs, decoupling dataset size from RAM. LevelDB/TigerBeetle shape, all
grid blocks: sorted **data blocks** + a sparse **index block** (first-key → data
block) + a **filter block** (bloom, to skip SSTs that cannot hold a key — Loki's
bloom tier). Point lookup = bloom → index → one data block, each pulled through the
Stage-B cache; range scan streams the covering blocks. **Milestone:** cold start no
longer rehydrates whole tables into RAM; a point lookup touches O(1) blocks
(verified by fetch counters).

**Started: the sorted data blocks and the sparse index.** `store/sst.rs` writes a
table's rows, in key order, into `SstData` blocks packed until each is full, then a
single `SstIndex` block recording the first key and identity of each data block.
That index block is the SST's root: given its identity a reader finds any key. The
lookup is the O(1) one the milestone names — binary-search the sparse index for the
one block a key could be in, read that block, scan it — and a test proves it
touches exactly two blocks (index + data) whatever the row count, using the memory
store's read counter. Keys are row identities, so this re-expresses the current
checkpoint SST's format in blocks (Stage A's other half) rather than a new key
space, and the writer refuses out-of-order keys and rows too large for a block. The
bloom filter block and a multi-block index (the single index block currently bounds
an SST at ~6.5k data blocks, a bound that is checked and raised, not overrun) are
what remain of Stage C. The range-scan reader is now built: `SstReader::scan`
locates the block a range's low key falls in through the sparse index, then
reads consecutive data blocks and emits their in-range rows in key order,
stopping at the first block that runs past the high key — so a range fetches the
index plus only the data blocks it covers, not the whole SST, which a test holds
to by reading a narrow window near the end of a three-thousand-row SST in four
block reads. The `get` lookup was refactored onto the same index-navigation
helpers and a shared data-block iterator in the process, so the point and range
paths cannot drift apart. The bloom filter is now built too. `store/bloom.rs` is a one-block filter over
the row identities, filled as the writer appends and written as an `SstFilter`
block; `finish` returns an `SstHandle` naming both the index and the filter. A
reader checks the filter first — a key it rejects returns without the index or a
data block being read, which is the whole point of a filter (skipping an SST
that cannot hold a key), and a test shows an absent key costing one block read
where a present one costs three. The filter has no false negatives, the one
property correctness needs: an inserted key is never reported absent, and an
empty or all-zero filter admits everything rather than claiming absence.
Membership is double-hashing over a splitmix64 finalizer, seven bits per key.
The filter is a fixed 128 KiB block — good to about a hundred thousand keys
under one percent false positives, degrading gracefully beyond, never to a false
negative — so a sized or per-block filter and the multi-block index (still
bounding an SST at ~6.5k data blocks) are what remain of Stage C, both
refinements rather than correctness gaps.

With that, Stage C's read path is complete: a point lookup is filter → index →
one data block, a range scan streams the covering blocks, and both are proven to
touch only the blocks they must. **The SST is now load-bearing.** The checkpoint
writes every table through `SstWriter` into content-addressed blocks under
`blocks/` — data, sparse index, bloom filter, and a *roster* block listing every
identity the SST comprises, so the garbage sweeper enumerates an SST by one read
instead of walking its data. Cold start scans each SST block-wise through the
tiered stack (`block_cache_bytes` RAM frames over the `disk_cache_bytes` slot
file over the bucket — the two config knobs finally wired, sized in `StackPlan`
and refused when under one block), and the write path populates the tiers on the
way out. Rows larger than one block's payload chain through overflow blocks
(head entry carries the chain identities; bounded at ~4 MiB per row, loudly).
The manifest names each SST by its root identities (`bsst` lines); manifests
from before the block grid still load, their whole-object SSTs rewritten as
block SSTs by the next checkpoint and swept. The full external harness — kill
-9 recovery, object-WAL rebuild, checkpointed cold start from a wiped disk —
passes over the block path, and the fidelity suites are untouched by
construction. What remained of Stage C proper — the multi-block index and the sized
filter — closed with maturity-roadmap step 4's format follow-ups
(2026-07-25), alongside LZ4 data-block compression.

### Stage D — memtable flush + the manifest log (continuous ingest)

Kill the "flush not implemented" wall: ingest becomes bounded by flush *rate*, not
RAM *size*. **The core is built: rows spill to the bucket and the wall is
gone.** The row map is now a bounded working-set overlay rather than the
authoritative dataset index: pending changes, heap-resident rows, deletion
markers, and hot entries stay in RAM; cold committed state exists only in the
SST forest and is synthesized through bloom-gated point probes or merged walks.
Committed row bytes have two homes (`RowHome::Heap | Spilled`). The
auto-checkpoint at 65% heap already wrote every
committed row into the table's block SST; under memory pressure (heap still past
50% after compaction) the map entries flip to `Spilled`, a second compaction
drops the bytes from RAM, and reads fetch them back through the cache tiers —
`Storage::row_bytes` (into the statement arena, for values that outlive a row
step) and `Storage::with_row_bytes` (consume-in-place, for the constraint scans
that visit every row; two scratch sets so one fetch may nest inside another).
Cold start now installs spilled entries directly — the manifest scan warms the
cache tiers but the heap stays small, so a node restarts into a dataset larger
than its RAM. The external harness proves the milestone: 1.5× `memtable_bytes`
ingested through a 16 MiB heap with zero memtable-full errors, point reads and a
full count(*) of spilled rows, and the kill -9 / object-WAL / cold-start checks
all green over the spill machinery. Below the pressure threshold nothing spills
and reads stay heap-fast — the fidelity suites are untouched by construction.

**Deviations, stated:** (1) the monolithic text manifest remains (it is
kilobytes at this scale; the append-only manifest log + superblock come with
flush *frequency*, i.e. compaction pressure in Stage E). (2) A full scan of
spilled rows stages each row in the statement work arena — bounded and loud
(`work_arena_bytes`), never wrong; the streaming read path that lifts it is
Stage I's object-storage-adaptive execution. (3) A single transaction's
working set stays bounded by `table_rows`; total persisted row count does not.
**Stage E's first half is
now in:** a dirty table with spilled SSTs flushes a *delta* — its heap-resident
committed rows plus tombstones for every rowid removed since the last
checkpoint (recorded at the committed-removal choke points, including the
update-then-delete case where the latest version was heap-resident but an older
SST still holds one) — appended to the table's SST list (`dsst` manifest lines,
capped at eight members) instead of rewriting everything; a full rewrite runs
only when the list is full or the tombstone buffer overflowed, collapsing the
list to one and remapping the spilled entries. Cold start applies the list in
order — later members' rows shadow earlier ones's, tombstones remove — and the
external harness proves delete/update/cold-start end to end (rows deleted after
spilling stay deleted; an update wins over its older SST version). Storage
state (list installs, entry remaps, tombstone clearing) applies only after the
manifest CAS lands, so a lost publish leaves memory consistent with the
still-current manifest and the orphaned blocks sweep as garbage. DML WHERE
scans consume spilled rows in place (the two-slot spill scratch), so a DELETE
over thousands of spilled rows no longer stages every candidate in the
statement arena. (Paced background compaction has since landed — see Stage E:
the merge is a background job crossing beats, not a checkpoint rider.)
**Crux invariant** (kept): an SST is referenced by the published manifest
before the WAL resets — the checkpoint orders it so.

**Stage H's spirit arrives early as a crash-torture differential**
(`tests/external/torture_diff.py`, a run.sh step): seeded random DML against
pos3ql *and* a hermetic real PostgreSQL, with random kill -9 restarts and
wiped-disk cold starts between acknowledged batches — the reference database
is the model, so the spill / delta / tombstone / WAL / manifest machinery is
checked against PostgreSQL itself after every recovery. Deterministic from its
seed. Its first run caught a real bug class: the standard library's *stable*
`sort_by` draws merge scratch from the heap above a size threshold, so five
query-path sorts (ORDER BY materialization, set operations, ordered/DISTINCT
aggregates, jsonb key canonicalization) violated the post-startup allocation
guard only once a sort exceeded ~tens of thousands of rows — below every
suite's radar until the torture sorted 20k. All five now run on
`arena::stable_sort_via`, an allocation-free stable sort (index permutation in
the statement arena, original position as the tiebreak, applied by cycles),
property-tested against the standard sort across the threshold. The guard
itself now prints an alloc-free backtrace (`backtrace_symbols_fd`) when it
fires, so the next violation names its call site. The full IANA time-zone database follows (the larger half of B-071's remainder): TZif files parsed per RFC 8536 into fixed thread-local pools — a zone-name catalog walked at startup before the allocator freezes, zones loaded on demand (64-slot cache, loud when full), transition history binary-searched, the POSIX TZ footer rule (its own parser) covering the far future — with the embedded rule set kept as the no-zoneinfo fallback. Corpus 36 pins Moscow's +04 era, Caracas's -04:30, Lord Howe's half-hour DST, 1968 US rules, Chatham's +12:45, case-insensitive names, zone names in timestamp literals (resolved at the literal's instant), and session-zone interpretation of bare timestamptz literals — the last two being fidelity bugs the work surfaced and fixed. B-071's remaining item was schemas, closed next (B-150): table identity
becomes `(schema, name)` end to end — a `QualName` through the AST, a schema
registry with catalog MVCC in storage, and every lookup routed through the
per-statement search-path context — with `CREATE`/`DROP SCHEMA` (CASCADE
severing inbound foreign keys via a definition-only WAL record, RESTRICT
reporting dependents with PostgreSQL's DETAIL/HINT), a real `search_path` GUC
(quote-aware list, `"$user"` from the startup packet's user, which now also
backs `current_user`), `ALTER TABLE ... SET SCHEMA`, multi-name DROP, views
bound to their creation path, schema-aware catalogs, and additive WAL/manifest
persistence proven across kill -9 replay and wiped-disk cold starts. The
record-access half of B-071 turned out mostly stale; its real divergences from
PostgreSQL's static-type binding closed as B-151. The remainder closed next:
record-typed derived-table columns (B-152 — a structural tail on the projected
encoding's record tag plus a statement-scoped shape registry standing in for
PostgreSQL's composite-type catalog) and three-part column references (B-153 —
`schema.table.column` binding only to an unaliased FROM entry of that schema,
42P01 otherwise). Recorded gaps: same-named tables from two schemas in one
FROM, and `schema.table.*` (B-154).

### Stage E — leveled compaction (background, paced, allocation-free)

Keep read amplification and object count bounded under sustained update/delete load
(the old P7 milestone, object-native). Leveled compaction **paced like TigerBeetle**:
work amortized across operations ("beats") so it never spikes tail latency and never
allocates — a merge iterator over a fixed number of input blocks into a fixed number
of output blocks per beat. **Tombstones** become first-class SST entries, dropped when
they fall below the oldest live snapshot (co-designed with Stage F's watermark). GC is
Loki's compactor + retention: after new SSTs are committed to the manifest log, orphan
blocks are swept (the existing bounded `collect_garbage`). **Milestone:** a sustained
insert/update/delete workload holds steady-state read-amp and object count; latency
histograms show no compaction spikes. **Note:** secondary indexes are a *forest* of
LSM trees (one per index, TigerBeetle-style) reusing Stages A–E; introduce here or
defer.

**Status (2026-07-24): paced merges landed.** A table's spill list at the merge
trigger (4) gets its two oldest SSTs merged during the checkpoint — one bounded
merge per table per cycle, rows streamed in rowid order through the block cache
into a fresh SST (newer member wins duplicates, its tombstones consume the older
member's rows, and nothing is older than member 0, so no tombstone survives the
merge). The in-memory spill indexes remap only after the manifest CAS lands,
like every other install, and the filled-list full rewrite remains the safety
net (also the fallback when a pair exceeds the merge id scratch). Exercised in
run.sh (seven checkpointed cycles with interleaved deletes and updates, then a
wiped-disk cold start over the merged lists) and adversarially by the crash
torture's random checkpoint/kill schedule. Level-aware pair
selection followed (2026-07-24): the merge picks the adjacent pair with the
smallest combined entry count — least write amplification now, big settled
members left to accrete — and a pair away from the list head keeps its
surviving tombstones (they still shadow earlier members at cold start), only
the head merge dropping them. **Beat pacing landed (2026-07-24, maturity-roadmap step 3):** the merge left
the checkpoint entirely and became a background *job* crossing beats — its
id schedule built a few block reads per beat, its output streamed a few
block writes per beat, alternating fairly with sweep work, surviving
publishes (a delta only appends at the tail, so the pair's positions hold),
cancelled without loss when a collapse supersedes its pair, and its
half-written SST kept alive in the garbage sweep's keep-set until the
result publishes. The `SstWriter` now owns its state (buffers + cursors, no
arena borrow) precisely so a half-written SST can persist between beats.
Stage E's one open idea — a manifest *log* (append deltas instead of
rewriting the whole manifest each checkpoint) — is a deliberate non-goal at
today's scale rather than a deferral: at the configured table counts a full
manifest is a single ≤256 KiB PUT per checkpoint, so a second persistence
format over the single most critical object (the manifest root) buys nothing
and risks the one object that must never be wrong. Revisit only if table
counts grow to make the rewrite itself the bottleneck.

### Stage F — MVCC snapshot reads over object-resident data

Preserve snapshot isolation once the working set spills to the bucket. Every
committed row version carries its final WAL LSN; a read at snapshot `S` sees the
newest version whose `commit_lsn ≤ S`. REPEATABLE READ pins that snapshot across
statements, READ ONLY is enforced recursively through prepared statements, and a
fixed active-snapshot registry exposes the oldest retention watermark. Checkpoints
encode immutable entries by `(rowid, commit_lsn)`, publish while readers are pinned,
and release their bounded resident staging histories after the manifest makes those
versions object-resident. Point probes and merged walks use the same snapshot rule.
Paced compaction keeps every version newer than the oldest live snapshot plus its
one visible baseline, and may discard older versions only after that watermark
advances. A second writer parks on the bounded wait graph, then re-evaluates at
READ COMMITTED after the owner commits or rolls back; a cycle selects a 40P01
deadlock victim.

**Milestone:** concurrent sessions show identical snapshot semantics whether data
is in RAM or on the bucket; update churn and snapshot duration consume immutable
object capacity rather than the bounded resident history; the full differential
suite stays green with `memtable_bytes` shrunk tiny.

**Status (2026-07-24): the forced-spill differential mode landed early** — the
single-session half of the milestone needs no MVCC change, so it now runs as a
standing run.sh step: the whole suite (43 corpora, the exact-error corpus, all
3205 sqllogictest blocks) against a pos3ql with a 256 KiB memtable over MinIO,
every query continuously spilling, checkpointing (paced merges included), and
reading back through the cache tiers — green, with the bucket showing hundreds
of content-addressed blocks written during the run. What remains of Stage F is
the real multi-session prerequisite: LSN-keyed row versions and the
snapshot-aware merge read.

**Status (2026-07-28): the transactional versioning foundation landed.**
Rows retain up to eight command-ID-keyed pending images, and table definitions
retain up to eight transaction-owned shape/layout versions in a
startup-budgeted slab. ALTER TABLE transforms rows without hiding the committed
shape from other sessions, and commit/rollback/savepoints publish or discard
definition plus row layout atomically. This closes data-modifying-CTE command
snapshots and pg_restore's explicit-transaction ALTER surface. It deliberately
does not claim Stage F: the one committed image still needs to become an
LSN-keyed history with an oldest-live-snapshot retention watermark.

**Status (2026-07-29): commit-LSN identity reached the live row transition.**
Commit promotion stamps every inserted, updated, and deleted row with the
transaction's final WAL LSN; WAL replay reconstructs the same stamp; and
`visible_at_lsn` combines own-command visibility with the durable snapshot
cutoff. This establishes one structural visibility choke point and tests the
too-new-image branch. It is deliberately not historical MVCC yet: checkpoint
SST entries still reload as legacy pre-versioned images, the map retains only
one committed image, and there is no active-snapshot registry or compaction
retention watermark.

**Status (2026-07-29): historical snapshots are live.** Up to eight committed
images per resident row are retained by commit LSN; REPEATABLE READ and READ ONLY
transaction characteristics work in combined PostgreSQL spellings; every scan,
constraint check, catalog lookup, and subquery uses the statement/transaction
snapshot; and ACCESS SHARE table locks protect pg_dump's schema view. A deterministic
object-store test pins an old reader, publishes the newer generation through
CHECKPOINT, proves the reader still sees the old image, wipes both cache tiers, and
cold-starts the latest image from the same bucket. This closes the pg_dump
consistency prerequisite without changing the durable hierarchy: the provider-neutral
object store is authoritative, and RAM/local disk remain disposable caches.

**Status (2026-07-29): historical versions are object-resident.** The block SST
format now sorts immutable live values and tombstones by
`(rowid, commit_lsn DESC)`, while retaining read compatibility with manifests
that name legacy rowid-only SSTs. One storage visibility choke point selects
across pending command versions, resident committed versions, and every SST
member. Delta checkpoints flush newly resident versions, then release their RAM
side chains; paced pair merges compact physical version streams against the
oldest active snapshot instead of stopping when a reader is pinned. Tombstones
participate in bloom filters, so a certain-negative filter result can never
resurrect an older member. The deterministic provider-neutral regression holds
one old snapshot through 24 updates, a delete, 25 publications, repeated pair
merges, and a cold start with both cache tiers wiped. The storage VOPR also
proved commit publication independent of object-store reads during injected
outages by removing old uniqueness-index entries by row identity.

**Stage F core is complete for the current materialized execution model.**
Stage I's future suspendable row source must register its longer-lived read in
this same snapshot registry and unregister it only when the portal is exhausted
or closed; it must not introduce a second retention mechanism. PostgreSQL
block-and-wait writer behavior, row/table lock modes, deadlock detection, and a
bounded relation-predicate SERIALIZABLE validator are now live in the
concurrency slice below. Finer predicate/range dependency tracking remains
concurrency-fidelity work, not a storage-authority exception.

### Stage G — object-store client hardening & multi-provider reach

Make the client production-shaped and reach real clouds, not just MinIO. The
core boundary is a minimal **provider-neutral object-store contract**:
immutable PUT, whole/ranged GET, LIST, DELETE, and conditional compare-and-swap
of the manifest/root pointer. Checkpointing, WAL, compaction, recovery, and the
cache hierarchy depend only on those semantics. S3, MinIO, Google Cloud
Storage, Azure Blob Storage, and future equivalents are selected by
configuration and implemented below that boundary; provider-specific signing,
authentication, and transport details must never leak into storage logic.
Keep the hand-rolled, static-memory discipline. TLS is isolated in rustls and
chunked-transfer decoding is complete. The remaining transport work is
**multipart upload** if object producers grow beyond single-PUT limits and **streaming
(non-buffer-bound) response reads** so a large-block GET is not capped by a
fixed buffer (today's `ResponseTooLarge`). **Milestone:** the full
flush/compaction/cold-start pipeline runs through the same contract against
MinIO and representative hosted object stores, with no provider branches above
the adapter boundary. **Risk:** TLS is the single dependency-policy exception;
keep it isolated below the contract so the core stays `libc`-only.

**Status (2026-07-24): chunked decoding and streaming WAL-segment replay
landed.** Chunked-transfer responses (hex-framed chunks with extensions and
trailers) decode into the bounded response buffer, refusing loudly on
overflow; and WAL-segment replay streams in ranged windows, closing a latent
unrecoverability — a committed batch larger than
`object_store_response_bytes` uploaded fine but could never be replayed at
cold start. run.sh proves the round trip
with the response buffer shrunk below one batch.

**Status (2026-07-24, later): TLS landed — the deferred decision resolved as
isolated rustls.** rustls (with compiled-in Mozilla roots) is the single
whitelisted dependency exception, and `mem::guard::tls_scope` is its only
door: the client configuration is built pre-freeze, and every runtime call —
handshakes, record I/O, teardown — runs inside a scope whose allocations are
charged against `tls_pool_bytes` and abort loudly past it, so the
static-memory discipline holds everywhere else. `object_store_tls = on` turns
it on; `object_store_tls_ca_file` adds PEM roots for self-signed endpoints (parsed by a
hand-rolled PEM/base64 reader at startup — no new parsing dependency). Proven
two ways: an in-process rustls server round trip in the unit tests (with a
checked-in certificate whose provenance and the `CA:FALSE` lesson live in
tests/data/README.md), and a run.sh durability cycle against MinIO serving
HTTPS — commit, checkpoint, kill -9, wiped disk, cold start entirely over
TLS. Object-store I/O errors now carry the source error's words, so a certificate
rejection names itself instead of flattening to `InvalidData`.

**Scope correction (2026-07-28) — transport hardening is done; the
provider-neutral boundary remains required.** The existing S3-compatible
client is the first adapter and MinIO remains the always-on conformance target,
but the S3 wire API is not the storage engine's architectural interface.
Hosted S3, MinIO, Google Cloud Storage, Azure Blob Storage, and equivalents
must all satisfy the same object semantics without conditionals in WAL,
checkpoint, LSM, cache, or query code. A native adapter or a separately
deployed compatibility gateway may translate a provider's wire/auth protocol;
that choice is below the contract and cannot change database behavior.
Multipart upload stays demand-driven until an object producer exceeds a
single PUT, but the contract must not preclude it.

**Status (2026-07-29): the provider-neutral boundary landed.** WAL upload,
manifest CAS, recovery, checkpointing, garbage collection, and the
content-addressed block store now depend on `object_store::Client` and its
provider-neutral request/result/error types. Adapter selection, credentials,
endpoint identity, and AWS environment fallbacks terminate below that module;
the deterministic bucket implements the same contract and a conformance test
pins PUT/create-only/CAS, whole and ranged GET, LIST, DELETE, and status
semantics. User configuration is now spelled `object_store*`; the old `s3*`
keys are strict compatibility aliases and mixing the two spellings is rejected
as a duplicate setting. Shared SHA/HMAC primitives moved out of the S3 adapter,
so block identities, SCRAM, and SQL digests no longer depend on a provider
module. The S3-compatible adapter remains the first transport and MinIO the
always-on target. Remaining here is hosted-provider qualification (native
adapter or compatibility gateway) and multipart only when an object producer
actually exceeds single-PUT constraints.

### Stage H — deterministic storage simulation (VOPR for the whole stack)

Prove the above correct under adversarial faults — the TigerBeetle VOPR discipline,
extended from consensus (`vsr`/`sim`) to storage. A **virtual object store + virtual
grid disk** implementing the same `io` traits, PCG-driven, injecting latency,
partial/torn writes, bit-rot, misdirected/duplicated I/O, and S3 outage/slowness/eventual-
consistency edges. Invariant checkers assert, from seeds: no committed write is ever
lost across crash-restart storms; the superblock/manifest CAS is never violated;
every block read verifies its checksum (and repairs where redundancy exists);
**cold-start state == pre-crash committed state**, including mid-flush and
mid-compaction crashes (Stage D's ordering invariant). **Milestone:** long seeded runs
clean; every failure reproduces from its seed — the gate that lets the object-storage
tier be trusted the way the SQL layer is today. Stand this up as soon as **A** lands so
every later stage is born simulation-tested, not retrofitted.

**Status (2026-07-24): the storage VOPR is standing** — maturity-roadmap step 1.
The seam is `object_store::Client` (an enum over the S3-compatible adapter and
`s3::sim`'s *virtual bucket*, selected by `object_store = sim`, which the real server binary
refuses): a deterministic in-process object store whose faults all draw from
one PCG stream — transient failures, ambiguous PUTs (applied, response
lost), one flipped bit on a GET body, and outages that begin mid-sequence
and end in a crash. The bucket also enforces the key discipline itself:
an unconditional overwrite that changes an object's bytes is a recorded
invariant violation (blocks content-addressed, manifest CAS-only, WAL
segments grow-only under their first-LSN key). The harness
(`sim::storage`) drives the real `Engine` — DML bursts, transactions,
checkpoints, auto-checkpoints, fault storms, corrupted disk-cache slots,
warm restarts, wiped-disk cold starts — against a model database with
certain/uncertain outcome tracking (an errored commit is *unknown*: the
engine must later show the before or the intended image, never a third).
Runs as `cargo test` (4 seeds) and scales by environment
(`POS3QL_STORAGE_VOPR_SEED0/SEEDS/STEPS`).

Its first session caught two real engine bugs (B-156, B-157, both fixed):
a commit whose synchronous WAL upload failed was left unpromoted — locally
durable but invisible until a restart resurrected it (client-observable
time-travel) — and a failed upload *retry* poisoned whatever innocent
statement (even ROLLBACK) happened to trigger it. The harness now scribbles
the WAL journal between incarnations alongside the cache file
(`corrupt_local_files`) — an extension that found three real bugs in one
session: a segment-recovery floor that lost records after a disk-wipe
restart, value-index roster write-idempotency, and B-283 (an errored commit's
records observable while durable only in the journal, so a torn journal could
take them after a read). All three are fixed — recovery merges journal and
segments by LSN, the roster writes a stable lsn, and an errored commit now
becomes bucket-durable eagerly (statement-start retry) and at startup
(reconciliation of the journaled tail). What remains of Stage H: folding the
mid-flush/mid-compaction crash invariants into longer standing runs.

### Stage I — object-storage-adaptive execution (the four pillars)

The storage stages make data *reachable* from the bucket; this stage makes queries
*adapt* to the bucket's latency/bandwidth/request-cost profile. It is the execution-
side counterpart to A–H and the concrete answer to "rearrange queries smartly for
object storage" — a **planner + scheduler + executor** concern, deliberately *not* a
bytecode VM (see *Considered and deferred* below). Four pillars, all behind frozen SQL
semantics (the differential + fuzzer are the guardrail — nothing here changes a result):

1. **Storage-aware cost model.** Give the planner an object-storage cost vector — per-
   request *latency*, request *count* (money + per-prefix rate limits), *bandwidth*, and
   **cache residency** (a block likely in RAM/disk cache is ~free; a cold block pays an S3
   GET). It then prefers one sequential, prefetchable scan over a nested-loop of cold point
   lookups; picks hash/semi-joins that touch each side's blocks once; and pushes
   predicates/aggregates/projection down into **block pruning** (zone-maps / min-max +
   bloom from Stage C's index+filter blocks) so fewer objects are fetched at all. Extends
   the existing predicate pushdown (B-037). *Depends on:* Stage C metadata.

2. **Async batching I/O scheduler.** A runtime layer between the executor and `BlockStore`
   that turns a plan's block demands into efficient traffic: **coalesce** adjacent/needed
   reads into fewer, fatter ranged GETs; issue them **in parallel** up to a fixed in-flight
   bound; **prefetch** ahead of the scan cursor; and **hedge** (a duplicate GET past a p95
   deadline, take the first) to cut the S3 tail. A fixed in-flight I/O pool, no per-request
   allocation (TigerBeetle's fixed grid I/O; Loki chunk prefetch). *Depends on:* Stage B
   cache + the suspendable row-source (Stage F).

3. **Block-at-a-time (vectorized) execution.** Operators process a whole fetched block's
   worth of rows per step, not one row per recursive call — a **push-based, batched**
   pipeline (Volcano-with-batching / DuckDB / MonetDB-X100), so a block fetched from S3 is
   consumed as a batch, amortizing per-operator overhead and matching the bandwidth-bound
   profile. This is the throughput lever, and the specific reason *not* to copy SQLite's
   row-at-a-time VDBE. An executor refactor of the `sql/query.rs` scan/exec path, done
   incrementally (vectorize the scan → filter → project hot path first) with the
   differential suite as the guardrail. *Depends on:* Stage C.

4. **Late materialization.** Fetch and carry only the key + the columns a stage needs;
   assemble full rows only for the rows that survive filters/joins/LIMIT — so cold blocks
   for unneeded columns are never fetched and full-row decode is paid only for survivors
   (C-Store/Vertica late materialization; Parquet column projection). Enabled by the
   **PAX / row-group within-block layout** noted in *Data structures & performance strategy*
   (columns clustered inside a block, so a projection reads only its columns' sub-ranges).
   *Depends on:* the PAX block refinement (Stage C) + Pillar 3.

**Milestone.** A selective query over a bucket-resident dataset fetches O(surviving blocks),
not O(table) — verified by GET counters — and the full differential suite is green in the
Stage-F forced-spill mode (fidelity unchanged while the whole path is rewired for object
storage). **Risk:** Pillar 3 is the largest refactor; keep it incremental and diff-gated so
fidelity never regresses.

**Adaptive-planning slice (2026-07-30).** The first half of pillar 1 and its
PostgreSQL-facing observability are now concrete. `ANALYZE` computes
MVCC-visible row count, average row width, null fraction, and bounded
HyperLogLog distinct estimates; targeted analysis preserves untouched column
statistics, and empty targets disappear from `pg_stats` as they do in
PostgreSQL. The manifest carries the statistics through a total local-cache
loss, and `pg_class.reltuples`/`relpages` plus `pg_stats` expose them to
clients. Column statistics use a startup-sized transaction-version slab and
ordinary savepoint/rollback visibility; relation estimates update in place,
matching PostgreSQL's deliberate `pg_statistic`/`pg_class` split. Both images
are WAL-recoverable before their next manifest checkpoint. The planner consumes
those estimates for predicate selectivity,
cardinality-aware join ordering, and durable single-column index scans. One
provider-neutral telemetry vector counts RAM hits/misses, disk hits/misses,
and object GET/PUT/contains operations at the `BlockStore` boundary; no
planner code knows an S3, MinIO, Google Cloud Storage, or Azure implementation.

`EXPLAIN` now prints the real bounded plan for SELECT, set operations, and
data modification in text, JSON, XML, or YAML. `ANALYZE`, `VERBOSE`, `COSTS`,
`BUFFERS`, `WAL`, `TIMING`, `SUMMARY`, `MEMORY`, `SERIALIZE`, `SETTINGS`, and
`GENERIC_PLAN` are parsed with PostgreSQL 18's invalid-combination checks.
Execution metrics come from the ordinary executor, cache/object counters,
wire serializer, and production WAL codec; planning itself performs no block
read. A forced object-resident regression wipes RAM and disk state, proves
EXPLAIN causes zero GETs, and proves a primary-key equality plan fetches fewer
durable objects than a full scan. The durable authority is unchanged: rows,
indexes, statistics, and WAL publication remain behind the common object-store
interface, while RAM and local disk only accelerate reads.

Pillar 1 still needs richer multi-column and distribution statistics, more
access paths, and cost calibration against real provider traces. The first new
access path landed: a **hash join** for two-table inner/cross equi-joins over
base tables — single-column and multi-column keys (composite FKs, natural
keys) — the inner side is built into an arena hash table keyed by the join
column(s), the outer side probes it, so both tables are scanned once (O(N+M)
reads, the right shape for cold object storage) instead of the nested loop's
O(N·M). The build uses the planner's `reltuples` estimate as its cost and
arena-capacity gate, avoiding a preliminary visibility scan; a stale
underestimate fails loudly at the fixed hash-table boundary. The equi-condition
only generates candidates; the full ON and WHERE still run at the leaf. The
planner selects the nested-loop plan up front when the hash path is inapplicable
or exceeds its fixed capacity (verified against PostgreSQL 18:
duplicates, NULL keys, unmatched keys, cross joins, residual conjuncts,
aggregates, LIMIT early-stop, int-width-mixed keys, and multi-column keys).
Pillar 2's first reactor slice now drives an in-flight durable block GET without
blocking the PostgreSQL socket loop: a miss preserves its block identity and
response state in the startup-bounded object store, the server registers that
one descriptor through its platform reactor, and the parked statement retries
only after the exact response completes or reports its terminal storage error.
Checkpoint publication waits for registered reads before it takes synchronous
ownership of the same bounded client. The block stack now has a configured,
startup-bounded pool of independently connected GET slots; each holds its own
request identity, response buffer, and terminal state until its parked caller
consumes it, so concurrent cold reads neither overwrite nor re-fetch one
another. The manifest and WAL clients remain synchronous so their statement-atomic
durability contract does not change.

**Pillar 2 scan-prefetch slice (2026-08-02).** Sequential SST readers,
external-run cursors, compaction scans, and the spilled-table member cursor now
resolve the next data-block identity from the already-loaded index leaf and
schedule it before consuming the current block. Prefetch is an explicit
`BlockStore` operation: only reactor-owned GET stacks schedule it, a completed
body remains owned by its request slot until the demand read consumes it, and
RAM/disk tiers pass the request through without inventing a buffer or a second
GET. The one-block lookahead is bounded by the configured GET-slot pool; a
full pool simply cannot schedule another optional lookahead, while provider
errors surface through the normal demand read. Cross-leaf lookahead now keeps
the same ownership contract: a cursor schedules the next index leaf, takes
only that completed speculative body, then schedules the leaf's first data
block before crossing the boundary. Ranged-GET coalescing and p95 hedging
remain separate scheduler work. A configured hedge is now available too:
`object_store_hedge_after_ms` starts one duplicate of a still-pending GET on a
spare fixed slot after that deadline (zero disables it); the first verified
body wins and releases its sibling. The remaining Pillars 2–4 work is
coalescing, the block-at-a-time executor, and PAX
late materialization described above.

**Pillar 3 scan slice (2026-08-02).** An unshadowed cold table scan carries
each selected row out of the resident merged-SST data block before the cursor
advances, rather than re-probing that row through the same SST. The copied row
lives only for the recycling callback (or in the ordinary statement arena for
a retaining caller); the block context is released before nested execution can
issue reads. The planner uses the same metadata-only request estimate for
sequential and single-column index paths, so a fragmented point probe is not
selected when the block cursor is cheaper. Repository formatting is now part
of CI (`cargo fmt --check`), and the object-store regression bounds cold
full-scan GETs while checking the selected plan is no more expensive.

The deterministic storage-VOPR keeps its 16-seed, 300-step endurance sweep,
but distributes independent seeds over four bounded workers. The merge gate
therefore targets five minutes rather than serially multiplying every
checkpoint/restart/verification cost, with a 15-minute hard ceiling for
runner variance.

Crash recovery binds its TCP listener with `SO_REUSEADDR` before the address
is claimed, and its pre-restart harness probe uses the identical bind
contract. A killed predecessor can therefore be replaced after active
connections close without weakening exclusive listener ownership.

The async object-read regression fixtures now use explicit socket and response
ownership handshakes, so their fixed-slot assertions are scheduler-independent
on every CI runner.

**External-execution slice (2026-07-30).** Physical scans now have a recycling
mode: every join depth retains its bound outer rows while reusing row-local
decode/evaluation space after the recursive callback returns. This separates
source-row lifetime from retained output lifetime without weakening the
ordinary arena-backed row-source API. A startup-bounded external sorter writes
ordinary immutable SST runs through `BlockStore`, performs stable eight-way
merges with fixed reader/writer buffers, and streams the final run. Top-level
`ORDER BY`, `DISTINCT`, and `DISTINCT ON` use it whenever durable object storage
is attached, including OFFSET/LIMIT, postponed projections, and FETCH WITH
TIES. A cold-cache regression sorts a result larger than its 512 KiB work arena
after deleting both local cache tiers and observes object GETs and PUTs.

Immutable runs now also have paired, startup-budgeted producer and reader
pools. A nested materializer retains an independent producer, while each
cursor borrows the provider-neutral block stack only to start or advance and
releases it before invoking query execution. Finalized output is copied into
reader-owned staging before evaluator scratch is recycled, then handed to a
higher-ranked callback; CTAS/INSERT-style consumers can retain their own copy
without borrowing cursor storage. Non-lateral derived tables, including CTE
and view expansions, spool through that interface. The cold-cache regression
composes a 1.5 MiB child materialization with a parent external sort and a
retaining CREATE TABLE AS consumer under a 512 KiB work arena.

Set-operation multisets now use the same encoded-run seam. UNION ALL retains
left-to-right append order when no final ordering is requested; UNION,
INTERSECT, and EXCEPT merge sorted equal runs with their DISTINCT/ALL
multiplicities; a trailing ORDER BY/LIMIT/OFFSET/WITH TIES streams a final run.
INSERT, CTAS, and other row-source consumers receive the run through a
higher-ranked callback rather than rebuilding it in the statement arena. A
small object-store simulator regression covers all three combining families
and observes both GETs and PUTs.

Scalar subqueries now spool through the same seam and stop at the second row
with PostgreSQL's cardinality error instead of first retaining the whole
result. `IN (subquery)` keeps a statement-arena capability to an immutable run
and probes it through the equality operator, preserving empty-set, NULL, row,
and DML semantics without borrowing mutable database state. The capability
owns no provider identity: it reaches only the tiered `BlockStore` and its
startup-sized reader pool.

Recursive CTEs carry immutable all/work runs in their materialized binding;
iteration-local substituted AST and decode scratch rewind after each run is
published. Lateral subqueries and ordinary set-returning functions spool per
outer row, while top-level and retaining consumers recycle the cursor row
before advancing. RIGHT/FULL joins append `(join depth, row ordinal)` matches
to a sorted run and merge matches produced by later outer-join post-passes,
replacing the arena-sized bitmap. A cold-cache regression exercises all four
families under a 256 KiB work-arena cap and observes provider-neutral object
traffic.

The durable authority remains the provider-neutral object contract: query code
does not know whether its blocks reach S3, MinIO, Google Cloud Storage, Azure
Blob Storage, or another adapter. RAM and disk cache those same content-addressed
blocks and may disappear at any point between statements. The second
externalization wave added `ARRAY(subquery)` results, set-subquery forms
carrying their own final ORDER BY/LIMIT/OFFSET (including set-operation
bodies), and the grouped-aggregate group-key sort through the same run stack.
The final wave closed B-006: windows now use the partition-at-a-time
representation — the source rows external-sort by (PARTITION BY keys, ORDER BY
keys, row position), partitions stream back one at a time through the
statement work arena, and each row's window values land in one
ordinal-stable win run that the projection re-scan joins back by position
before the final output sort. Pillar 2 must then make the cursor suspendable
around a fixed in-flight GET pool; pillars 3–4 batch expression evaluation
and introduce PAX column-range late materialization.

**Concurrency-fidelity slice (2026-07-30).** Execution now has a first-class
blocked result instead of converting contention to 40001 or blocking the
single-threaded reactor. Simple-query messages retain their frontend bytes and
completed-statement index; extended Execute retains its portal/message state.
A startup-bounded lock registry and wait-for graph carry stable
`(table slot, rowid)` identities through joins and hidden columns in both arena
and object-backed query runs. The four row lock strengths, `OF`, NOWAIT, and
SKIP LOCKED implement PostgreSQL's compatibility and paging behavior. UPDATE,
DELETE, pending unique-key writers, DDL/schema waits, and all eight `LOCK TABLE`
modes use the same graph. Ordinary scans, DML, COPY, TRUNCATE, VACUUM, and
ANALYZE acquire their PostgreSQL relation modes before observable work.
Per-mode acquisition stamps preserve incomparable table modes and let
`ROLLBACK TO SAVEPOINT` release exactly the table and row locks its
subtransaction acquired. Transaction end advances a generation and retries
parked peers, while a cycle raises 40P01 and immediately releases the victim.
Nonzero `lock_timeout` values are active deadlines, not merely observable
session text: the nearest parked deadline participates in the reactor poll,
and expiry replays the retained message once before returning PostgreSQL's
55P03 and rewinding the failed statement's locks and state. This timer lives in
fixed per-connection protocol state and does not add a thread, heap allocation,
or storage-provider dependency.

Every execution captures a statement undo mark before it can wait. A wait
rewinds row images, catalog/statistics undo, WAL staging, notifications, and
LISTEN changes while retaining transaction locks, so replay cannot duplicate a
partially executed multi-row statement. Locking SELECT performs its complete
row-lock pass before serializing rows, and every SELECT preflights relation
locks before RowDescription, preventing a late wait from leaking a partial
result. SERIALIZABLE is now a pinned-snapshot mode with a
startup-bounded relation-predicate validation registry; the write-skew
regression aborts with 40001 at commit. More granular predicate/range
dependencies remain part of the final PostgreSQL-fidelity work, not an excuse
to move durable state outside the common block store.

### First slice (de-risks the whole plan)

Land **A + a minimal B + the Stage-F forced-spill test harness** together, then run the
existing differential suite with `memtable_bytes` shrunk to a few MiB and watch data
page in and out of the bucket through the cache with fidelity intact. If that stays
green, the architecture is sound and the rest is incremental. Bring up **H**'s virtual
object store alongside **A**.

### Data structures & performance strategy (why the stages are shaped this way)

A row-oriented database that is *performant on object storage* is, above all, a
machine for **hiding per-request latency**. Every structural choice below follows
from the object-storage performance model, and the stages above exist to realize
them.

**The object-storage performance model (the constraints that dictate everything).**
Object storage (S3/GCS/Azure Blob) inverts the assumptions a local-disk engine is
built on:

- **Per-request latency is high and tail-heavy** — a GET/PUT is ~10–100 ms (p99
  far worse), versus microseconds for NVMe. Aggregate *throughput* is effectively
  unlimited; *latency* is the enemy. So the read path must minimize the number of
  **serial** round-trips and hide the rest behind cache, batching, prefetch, and
  concurrency.
- **Objects are immutable** — no in-place update, no append (bar multipart). A
  write publishes a whole new object. This suits **append-only, content-addressed**
  structures and an LSM (never update-in-place) perfectly.
- **Ranged GET** lets a byte range be read out of a large object, so many logical
  **blocks pack into one object** and are read individually — the escape from
  "one tiny object per row" (which per-request overhead and rate limits forbid).
- **Per-request cost and per-prefix rate limits** (money; ~3.5k PUT / 5.5k GET per
  prefix/s, scaled by spreading keys across prefixes) push toward **fewer, larger
  objects** and **key-prefix hashing**.
- **Strong single-key read-after-write and CAS** (`If-Match`/`If-None-Match`) exist
  now (pos3ql already relies on CAS for the manifest), but **LIST is only
  eventually consistent** — so the design must never depend on LIST for
  correctness, only address data by content hash reachable from the CAS'd root.

**On-object row layout (Stage C).** Rows are sorted by key and grouped into
compressed **data blocks** (target ~a few tens to low hundreds of KiB before
compression), many blocks per SST object. Each SST carries a **sparse index block**
(first-key of each data block → offset) and a **bloom filter block** (skip an SST
that cannot hold a key). Within a block, LevelDB-style **restart points + prefix
key compression** shrink keys, and per-block **compression** (lz4/zstd class) trades
CPU for the bytes/cost that dominate on object storage. A point lookup is then
**bloom → index → one ranged GET of one data block → decode the row**; a scan
streams the covering blocks. The sparse index and filters are *small* and stay in
RAM — you never pay an object-store round-trip to learn *where* data is, only
to fetch it.

**The in-RAM root and index (Stages A, D).** The manifest log + superblock (the
only CAS'd object) names every live SST, its key range, and its level — small
enough to hold in RAM and shipped to the bucket for bootstrap (Loki's index
shipping). Query planning consults RAM, never the bucket. Because non-root objects
are **content-addressed and immutable**, the cache is trivially correct (a block's
bytes never change under its id) and a stale LIST can never mislead a reader.

**MVCC on immutable objects (Stage F).** Immutability makes multi-version storage
natural: versions are keyed by `(key, commit_lsn)` and appended, never overwritten;
a snapshot read at LSN `S` takes the newest version with `commit_lsn ≤ S`; compaction
drops versions once they fall below the **oldest live snapshot** watermark. This is
exactly Neon's page-versioning model (below) at row granularity, and it is a real
change from today's two-version, txid-based, single-writer `RowState`.

**The cache hierarchy is the performance story (Stage B).** p50 latency is set by the
**cache hit rate**, p99 by the S3 tail:

- **Index + filters: always resident in RAM.** Every query needs them; they are tiny.
- **RAM block cache** — fixed frames, CLOCK/CLOCK-Pro; optionally two-level (cache
  *compressed* blocks for capacity, keep a small *decompressed* hot set).
- **Local NVMe disk cache** — larger warm tier; because every block is re-fetchable
  from the bucket, an evicted or torn disk-cache block is just a miss, never data loss.
- **Negative caching** via blooms avoids fetching blocks that cannot contain a key.
- **Prefetch / read-ahead** turns scan latency into throughput: issue the next N block
  GETs concurrently.
- **Hedged requests** tame the S3 tail: if a GET exceeds a p95 deadline, issue a second
  and take the first to return — a standard, high-leverage object-store latency trick.

**Write path & compaction economics (Stages D, E).** Never PUT per row: buffer in the
memtable + WAL (**group-commit** fsyncs), then flush a *frozen* memtable to one large
SST — few, big PUTs (**multipart** for large ones). Compaction shape is an explicit
economic choice on object storage, where a write is money + latency: **leveled** gives
low read-amp (good for point lookups) but high write-amp; **tiered/size-tiered** gives
low write-amp but higher read-amp, which the cache + blooms cushion. pos3ql should start
**leveled at the lower levels with a tiered top** (RocksDB/Scylla-style hybrid), tune by
measured read/write-amp, and **spread object keys across hash prefixes** to stay under
per-prefix rate limits. GC deletes orphaned objects only once unreferenced by any live
manifest generation *and* below the oldest snapshot (bounded sweeps already exist).

**Closest prior art — borrow deliberately.** **Neon** (Postgres-on-S3) is the most
directly relevant system: it stores 8 KiB Postgres pages as **LSN-keyed image + delta
layers** in object storage, served by a pageserver with a local cache — i.e. Postgres
semantics + LSN-versioned MVCC + object storage + local cache, which is precisely
Stage F at row rather than page granularity; its **layer-file** design informs the SST
+ version layout, and its pageserver cache informs Stage B. **RocksDB** informs the SST
format, blooms, and leveled/tiered compaction knobs. **Loki** informs object-storage-native
chunks, index shipping, the compactor, and the multi-tier chunk cache. **TigerBeetle**
informs the checksummed block grid, the manifest-log + superblock root, statically-allocated
caches, and paced allocation-free compaction. pos3ql's job is to compose these under its own
stricter discipline (static memory, `libc`-only, differential-frozen fidelity, VOPR).

**The row-oriented tradeoff (kept, with one refinement).** Row storage is the right call
for the OLTP access this engine targets — point lookups and full-row reads fetch one block
and get the whole row. Its weakness is wide analytical scans (a block carries all columns
even when a query wants one). The founding decision keeps storage row-oriented; *if* analytical
scans later matter, the low-risk refinement is a **PAX / row-group layout within a block**
(rows grouped by key, but columns clustered inside the block, Parquet-row-group style), which
buys scan/column-projection efficiency and better compression without abandoning the row model
or the point-lookup path. Full columnar storage is out of scope and not planned.

### Considered and deferred: an SQLite-style bytecode VM

A VDBE-style bytecode VM was weighed and **deferred** — not adopted. SQLite's VM exists
to separate prepare from execute, give a stable plan representation, and make execution
step-wise/suspendable; for pos3ql the first two are already handled (arena AST + `sql::prep`),
and re-expressing the executor as opcodes would mean **re-deriving every operator and
function's PostgreSQL semantics in bytecode — putting the differential fidelity at
serious risk** for little gain.

**A bytecode VM would *not* make queries "rearrange more smartly" for object storage.**
That adaptivity is three separable concerns, and none of them is bytecode: **(1) plan
choice** — a *storage-aware cost model* that prices request latency, request *count*
(money + per-prefix rate limits), bandwidth, and cache residency, so the planner prefers
one sequential prefetchable scan over a nested-loop of cold point lookups, picks
hash/semi-joins that touch each side's blocks once, and pushes predicates/aggregates/
projection down to *prune blocks* (zone maps / min-max + bloom); **(2) I/O scheduling** —
an async scheduler that coalesces, parallelizes, prefetches, and *hedges* block GETs; and
**(3) block-at-a-time (vectorized) execution** for throughput once bytes are in RAM. A VM
is merely one substrate for the scheduling slice — and the *wrong* one to copy here, since
SQLite's VDBE is **row-at-a-time**, a throughput liability an object-store (bandwidth-bound)
backend must move *away* from, toward the **push-based, batched, async operator pipeline**
(Volcano-with-batching / DuckDB-style) that delivers all three concerns at a fraction of the
fidelity risk. The one real execution-side pressure — a slow GET must not block the
single-threaded reactor once Stage F pages from the bucket — is met by making **only the
row-source suspendable** (an async cursor that yields while a block is fetched and resumes),
leaving the tree-walking *expression* evaluator in `eval` untouched, since expressions only
ever run over already-materialized batches.

Revisit a bytecode layer only for reasons that are *not* object-storage-driven: fine-grained
step-wise **server-side cursors**, a **persisted/portable compiled-plan cache** across a
fleet, or a **JIT** to native for CPU-bound execution — none of which the
storage-aware-planner + async-scheduler + push-based-pipeline approach needs.

## Maturity roadmap — what remains, in order (2026-07-29)

A full step-back audit against the founding goal — a mature,
PostgreSQL-compatible engine whose *primary* storage is object storage, with
local disk and memory as **mere caches** — found the SQL/wire-fidelity axis
substantially complete (differential + sqllogictest + fuzzer green; the
remaining open ledger entries document explicitly accepted or architectural
concurrency differences) and the remaining work concentrated in one open
structural storage gap, one compatibility wave, and the adaptive-execution
capstone. This section is the plan of record for all of it.

### Decisions of record (fixed with the project owner)

- **Object-storage interface: one provider-neutral semantic contract.** The
  storage engine never branches on a provider. The existing S3-compatible
  transport serves AWS S3, MinIO, and compatible endpoints; Google Cloud
  Storage, Azure Blob Storage, and other providers enter through adapters or
  gateways implementing the same immutable-object, range-read, listing,
  deletion, and conditional-root semantics. MinIO is the always-on local
  conformance target; hosted-provider tests verify adapters without creating
  provider-specific database behavior. RAM and local disk are bounded,
  disposable caches in front of this durable tier.
- **"WAL-compatible" means the logical replication protocol.** Physical XLOG
  compatibility would require adopting PostgreSQL's heap page format wholesale
  — re-implementing PostgreSQL's storage engine and defeating the
  object-native design — and is a **non-goal**. The target is pos3ql as a
  logical replication *publisher* (`START_REPLICATION`, pgoutput,
  publications) so real PostgreSQL subscribers and the CDC ecosystem (Debezium
  and kin) consume pos3ql changes, and later the *subscriber* side as the
  migration on-ramp (pos3ql subscribes to a live PostgreSQL and takes over).
  The WAL already carries LSNs and full row images, so the decode side is
  well-matched.
- **Durability model: commit-durable-on-bucket is the durable-mode invariant.**
  Group commit batches WAL records and the commit acknowledges only after the
  segment PUT lands (an S3 PUT is ~10–50 ms, amortized across the group).
  VSR becomes the *multi-replica* ordering and availability mode, but a quorum
  of replica disks never substitutes for the durable object tier: commit
  acknowledgment still waits for the provider-neutral WAL object. Replication
  can coalesce that PUT and continue serving through node loss; RAM and every
  replica disk remain caches.

### The structural storage gaps ("disk and RAM are mere caches")

1. **Closed: acknowledged durability moved off local disk.** With object
   storage enabled, commit-durable-on-bucket is the default: the acknowledged
   WAL segment is present in the durable object tier before success is
   reported. Wiping local disk at any instant therefore loses no acknowledged
   transaction. Local-only mode remains an explicit operating mode, not the
   target durable architecture.
2. **Closed: row and secondary-index state are object-resident.** Cold committed entries and row
   bytes live only in immutable SST objects and are synthesized through
   bloom-gated point probes or merged walks; map and heap pressure evict them,
   and cold start installs no per-row entries. `table_rows` bounds the working
   set, not the durable dataset. Immutable secondary-index generations carry
   encoded key tuples, equality hashes, row identities, and commit LSNs in the
   same block grid and manifest. Their startup-sized RAM maps and disk blocks
   are caches only; an incomplete map probes the durable generation plus the
   resident post-manifest overlay. Equality/uniqueness and range candidates
   are always MVCC-rechecked, and a dirty checkpoint atomically rebuilds the
   generation, compacting stale entries without provider-specific behavior.
   Chained immutable roster roots remove the former flat-roster size ceiling;
   historical snapshots conservatively use the ordinary MVCC row walk because
   a latest-overlay key generation does not encode every intermediate key.
3. **Closed at the scheduling boundary: checkpoint work is sliced.** Automatic
   checkpoints and compaction advance through bounded event-loop beats instead
   of monopolizing a connection. **Status (2026-07-30): the transactional
   publication gate is gone.** WAL now owns one startup-sized private stage per
   maximum connection. DDL and final row images enter only their transaction's
   stage; commit copies exactly that stage into the durable batch, rewrites its
   provisional record LSNs into commit order above the current manifest/storage
   floor, recomputes CRCs, fsyncs, and synchronously uploads through the common
   object-store interface. Rollback discards one stage and savepoint rollback
   truncates only its stage. A checkpoint can therefore publish committed state
   while other transactions retain rollback-capable catalog work, without
   either leaking that work or advancing past its eventual committed WAL.
   Cold recovery after interleaved commit/rollback/savepoint/checkpoint
   sequences is covered with both local cache tiers deleted. The regression
   also exposed and fixed heap compaction skipping rows owned by a pending
   `CREATE TABLE`; compaction now preserves those private row images.
   Absolute serial/sequence positions are staged for transaction-visible
   pending creates too, and their retry markers clear only after WAL
   publication preflight succeeds.
   Historical read-only snapshots remain compatible with publication. Stage
   I's suspendable row source will extend the same asynchronous scheduling
   model to cache misses and large object reads.

### The compatibility wave (a fresh audit of what real deployments touch)

- **COPY** — IN/OUT in text, CSV, and binary are implemented (see the datatype
  section above; binary is byte-exact against PostgreSQL across the whole type
  surface, composites included). The extended-protocol sub-flow is complete:
  COPY IN holds its implicit transaction through CopyDone, observes extended
  Sync/error recovery, preserves CopyFail's client reason, and COPY OUT streams
  independently of Execute's row limit. PostgreSQL restore into pos3ql is now
  covered end to end by a checked-in vanilla 18.4 plain dump and a
  provenance-stamped PostgreSQL 15.18 custom archive: setup GUCs,
  schema/type/domain/table DDL, generated and owned identity sequences, COPY,
  `setval`, constraints/indexes, views and materialized views all restore and
  survive restart. Real pg_restore runs ownerful, with four parallel workers,
  and replaces a populated database through `--clean --if-exists`. It also
  restores under `--single-transaction` and bounded `--transaction-size`,
  exercising identity/constraint ALTER TABLE inside explicit transactions. Its cleanup
  surface includes `ALTER TABLE IF EXISTS ONLY`, typed `ALTER ... OWNER`, and a
  transactional DROP SCHEMA sweep of tables, views, materialized views,
  sequences, domains and enums. The opposite direction is now gated too:
  PostgreSQL 18.4 pg_dump takes a REPEATABLE READ, READ ONLY snapshot, acquires
  ACCESS SHARE locks, completes catalog discovery, emits
  schema/data/view/identity state, and its plain dump restores into vanilla
  PostgreSQL with sequence continuation intact. CI runs this outbound round
  trip. Remaining pg_dump work expands breadth for object kinds not yet
  implemented; it is no longer blocked on consistency.
- **Server-side TLS for clients** — done. With `tls_on` (plus `tls_cert_file`
  and `tls_key_file`), the SSLRequest probe is answered `S` and the connection
  negotiates TLS; a client that does not ask for TLS still connects in the
  clear, and GSSAPI encryption is declined. Unlike the blocking S3 client, the
  server socket is non-blocking and reactor-driven, so it uses the low-level
  rustls `read_tls`/`process_new_packets`/`write_tls` API rather than
  `StreamOwned`: `recv`/`send` tunnel through the session, `wants_write` covers
  rustls's outbound queue so write-interest tracks it, and the large-result
  streaming drain encrypts through the session onto the (blocking) socket. Every
  runtime rustls call runs inside `mem::guard::tls_scope`; the `tls_pool_bytes`
  budget is grown by `max_connections` server sessions at startup, and each
  session is dropped inside a scope so its buffers are credited back. Verified
  against real psql over `sslmode=require` (TLS 1.3), including a byte-exact
  500 KiB streamed result and plaintext coexistence.
- **ALTER TABLE breadth** — done: ADD/DROP/RENAME COLUMN, RENAME TO, SET
  SCHEMA, `ALTER COLUMN TYPE [USING]`, `SET`/`DROP NOT NULL`, `SET`/`DROP
  DEFAULT`, `ADD`/`DROP`/`RENAME CONSTRAINT` — the metadata changes journal
  through the shape swap, and a type change rewrites every stored row through
  the shared `ColSource` plan. Remaining ALTER surface (owner/tablespace/storage
  parameters) is properties this engine does not model.
- **CREATE TABLE AS** — done: `CREATE TABLE [IF NOT EXISTS] name [(cols)] AS
  <query> [WITH [NO] DATA]`. The query's output schema (via `describe_query`)
  builds an ordinary backing table — no new persistence, it round-trips like any
  table — then the two-pass `INSERT ... SELECT` populate loop fills it; the tag
  is `SELECT <count>`. Verified byte-for-byte against PostgreSQL (corpus
  `52_create_table_as`).
- **Materialized views** — done: `CREATE MATERIALIZED VIEW [IF NOT EXISTS] name
  [(cols)] AS <query> [WITH [NO] DATA]`, `REFRESH MATERIALIZED VIEW`, `DROP
  MATERIALIZED VIEW`. A matview is an ordinary backing table (its rows, populated
  by the CREATE TABLE AS path) plus a new parallel `MatviewDef` catalog holding
  the defining query and populated flag — mirroring the `ViewDef` machinery, so
  the backing table needs no codec change and there is no dual-entry name
  collision (a new catalog `create_table_in` does not check). SELECT reads the
  backing table; REFRESH truncates it and re-runs the stored query; the catalog
  reports relkind `m` and lists it in `pg_matviews`; `DROP TABLE` on it is
  refused with 42809. The `MatviewDef` is durable through both a new
  `CreateMatview`/`DropMatview`/`SetMatviewPopulated` WAL op set (kinds 14–16)
  and an `mv2` checkpoint manifest line; `SetMatviewPopulated` makes REFRESH /
  WITH NO DATA state survive pure WAL replay. Verified byte-for-byte against
  PostgreSQL (corpus `53_materialized_view`) and by a WAL-replay restart test.
- **Durable stored-query dependencies** — done: views and materialized views
  capture a bounded, resolved identity set for referenced tables, views,
  domains, enums, and constant-regclass sequences when they are created.
  Query expansion follows the stable catalog slot while retaining the source
  spelling needed to rewrite the stored AST to the object's current qualified
  name. Table/type renames and table schema moves therefore preserve
  PostgreSQL binding, and one transitive graph drives RESTRICT/CASCADE for
  relation, type, sequence, and schema drops. The graph is carried through
  view replacement, WAL replay, and versioned checkpoint manifests; restart
  and differential coverage include table→view→materialized-view→view chains,
  explicit casts, sequence calls, rename, schema move, refresh, and cascade
  (B-204).
- **Sequences** — done: `CREATE SEQUENCE [IF NOT EXISTS] name [AS type]
  [INCREMENT] [MIN/MAXVALUE] [START] [CACHE] [[NO] CYCLE]`, `ALTER SEQUENCE`
  (redefine + `RESTART`), `DROP SEQUENCE`, and the functions `nextval` /
  `currval` / `lastval` / `setval`. A sequence is a first-class relation
  (relkind `S`, listed in `pg_sequences` / `pg_sequence`) backed by a parallel
  `SequenceDef` catalog, mirroring the `ViewDef` machinery. Its *existence* is
  transactional catalog MVCC (with `DdlUndo`); its *value* state
  (`last_value`/`is_called`) is deliberately **not** — an advance survives
  `ROLLBACK`, exactly as PostgreSQL leaves gaps — carried in `Cell` fields so the
  pure `&`-only expression evaluator can advance a generator through a
  `SequenceAccess` hook on `EvalHooks`. `currval`/`lastval` are per-connection
  session state on `GucState`, `created_at`-stamped so a reused catalog slot
  cannot leak a dropped sequence's value. `nextval` fires per row in `SELECT`,
  `INSERT`, and `UPDATE ... SET` (the `INSERT ... SELECT` counting pass uses a
  *dry* evaluator so the advance happens exactly once). Durable through a new
  `CreateSequence`/`DropSequence`/`SequenceAdvance` WAL op set (kinds 17–19,
  advances journaled at commit like the serial machinery) and an `sq2` checkpoint
  manifest line. Verified byte-for-byte against PostgreSQL (corpus
  `54_sequence`) and by a WAL-replay restart test. SERIAL and identity columns
  now use these same first-class owned sequences; a `DEFAULT nextval(...)` uses
  the expression-default path below.
- **Non-constant column DEFAULTs** — done: a `DEFAULT` with a function call
  (`now()`, `nextval(...)`, `gen_random_uuid()`, …) is kept as source text
  (`ColumnMeta.default_expr`) and re-evaluated per inserted row, instead of being
  folded once — which also fixes a latent bug where `DEFAULT now()` froze to the
  CREATE-TABLE time. A literal-only default still folds to a constant
  (`default_value`) for a fast insert; exactly one of the two is set. The fold
  test is "contains no function call" (`Expr::contains_call`), so volatile *and*
  stable functions are stored as text. Defaults are re-parsed once per statement
  (like CHECK predicates) and evaluated with the sequence hook, only for columns
  the row does not set explicitly (so a supplied value never wastes a `nextval`).
  `ADD COLUMN ... DEFAULT nextval(...)` backfills existing rows by evaluating the
  default per row, exactly as PostgreSQL rewrites the table; `ALTER COLUMN SET
  DEFAULT` and `DROP DEFAULT` handle expressions too. Durable through an additive
  `default_expr` field in the table column codec (WAL + `col` checkpoint line);
  `pg_attribute.atthasdef` and `pg_attrdef` reflect it. Verified byte-for-byte
  against PostgreSQL (corpus `55_default_expr`) and a WAL-replay restart test.
  (The stored text is the raw source, so `pg_get_expr`/`\d` reconstruction is not
  PostgreSQL's normalized form — a cosmetic gap, not a behavioral one.)
- **Generated columns** — done: `GENERATED ALWAYS AS (expr) STORED`, computed
  from the row's other columns at insert/update. The generation expression
  reuses the `default_expr` slot with an `is_generated` flag (they never coexist,
  so no extra per-column storage), packed into the column codec's existing flags
  byte (bit 4) for WAL + checkpoint. It is validated at CREATE/ADD against
  PostgreSQL's rules: immutable only (42P17 for `now()`/`nextval(...)`/…, via an
  inverse whitelist of volatile/stable functions), no reference to another
  generated column (42P17), no subquery (0A000). At DML the column is recomputed
  from the finished row (after defaults, once other columns are set), never
  writable — an explicit non-DEFAULT `INSERT` value is 428C9 and an `UPDATE`
  to anything but `DEFAULT` is 428C9; a dependency change reflows it. `ADD
  COLUMN ... GENERATED` backfills existing rows per row, like PostgreSQL's table
  rewrite. `pg_attribute.attgenerated` reports `'s'`. A Boyscout fix on the way:
  `LIKE` copied generated columns unconditionally (and conflated `INCLUDING
  GENERATED` with `IDENTITY`); it now copies a generated column as plain unless
  `INCLUDING GENERATED`, matching PostgreSQL. Verified byte-for-byte against
  PostgreSQL (corpus `56_generated`) and a WAL-replay restart test.
- **Identity columns** — done: `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY
  [(sequence options)]`, the SQL-standard auto-increment. Each identity (and
  each SERIAL pseudo-type) creates a first-class sequence owned by the column;
  custom `SEQUENCE NAME`, START/INCREMENT, MIN/MAXVALUE, CACHE and CYCLE all
  feed the ordinary sequence implementation. The lifecycle ownership edge and
  value-generator edge are distinct (so `OWNED BY NONE` detaches serial cleanup
  without changing its `nextval` behavior); both are durable in WAL and `sq4`
  checkpoints and follow table/schema/column renames. The generator drives
  INSERT and transactional TRUNCATE RESTART, while ownership cascades through
  DROP COLUMN/IDENTITY/TABLE.
  `ALWAYS` rejects an explicit
  `INSERT` value (428C9) unless `OVERRIDING SYSTEM VALUE`; `BY DEFAULT` accepts
  one, and `OVERRIDING USER VALUE` discards it for the sequence. `ALTER COLUMN
  ADD GENERATED ... AS IDENTITY` (requires NOT NULL, else 55000) and `DROP
  IDENTITY [IF EXISTS]` (55000 / notice when the column is not an identity) round
  it out. `pg_attribute.attidentity` reports `'a'`/`'d'`. Verified byte-for-byte
  against PostgreSQL (corpus `57_identity`), a WAL-replay restart test, and the
  PostgreSQL 18.4 plain-dump restore gate.
- **MERGE** — done: `MERGE INTO target [AS alias] USING source [AS alias] ON
  cond` with `WHEN [NOT] MATCHED [AND cond] THEN { UPDATE SET | DELETE | INSERT |
  DO NOTHING }` clauses. Source-driven, as PostgreSQL specifies: the source
  (a table, subquery, or `(VALUES ...)`) is materialized via the same path as
  `INSERT ... SELECT`, and each source row is matched against a one-time
  snapshot of the target on `cond`; a match runs the first WHEN MATCHED clause
  whose AND-condition holds, a miss the first WHEN NOT MATCHED clause. A
  `MergeLookup` gives ON/WHEN/SET expressions both tables in scope (qualified
  names to each half, unqualified searched in both — 42702 if ambiguous); INSERT
  values see only the source. A per-target-row affected flag enforces the
  cardinality rule (21000 when a target row would be affected a second time),
  and UPDATE/DELETE/INSERT reuse the existing row ops (constraints, generated
  columns, identity/defaults, and the sequence hook all apply). The tag is
  `MERGE <count>`. It is a *new* statement path, so nothing about SELECT/INSERT/
  UPDATE/DELETE changed. Verified byte-for-byte against PostgreSQL (corpus
  `58_merge`).
- **COMMENT ON** — done: `COMMENT ON { TABLE | VIEW | MATERIALIZED VIEW | INDEX |
  SEQUENCE } name IS { 'text' | NULL }`, `COMMENT ON COLUMN table.column IS ...`,
  `COMMENT ON SCHEMA name IS ...`, and `COMMENT ON { TYPE | DOMAIN } name IS ...`;
  `IS NULL` removes the comment. Type targets include built-ins, arrays, `reg*`
  catalog types, domains, enums, and the composite row types owned by tables,
  views, and materialized views.
  `obj_description(oid[, catalog])`, `col_description(oid, column)` and the
  `pg_description` catalog read them back. A comment is stored in its own fixed
  catalog keyed by `(class, schema, name, subid)` — restart-stable, since object
  OIDs derive from catalog slots but names do not — with the same commit/rollback
  MVCC overlay as a row (a transaction sees its own uncommitted comment, others
  the committed one; a rolled-back `COMMENT` restores the prior text exactly,
  savepoint-aware via a `DdlUndo::CommentSet` undo entry). Durable via a `Comment`
  WAL record (set and removal) and a `cmt` checkpoint line; dropping an object
  removes its comments through the same commit/replay drop paths, so a stale
  comment can never reattach to a same-named new object. Object resolution
  matches PostgreSQL's: the name binds to the first schema on the path holding
  any relation of that name, then the keyword's kind is checked (42809 on a
  mismatch, 42P01 for a missing relation, 42703 for a missing column, 3F000 for a
  missing schema). `'seq'::regclass` now resolves a sequence too (a Boyscout fix
  the sequence comments needed). Bounded loudly: at most `MAX_COMMENTS` comments,
  each at most `COMMENT_MAX` bytes. Plain views gained synthesized relation and
  composite-type OID ranges. A view-column target describes the stored body
  under its creator-captured search path, so later session path changes cannot
  retarget its dependencies. Sequence and index columns raise PostgreSQL's
  42809 (they are not commentable columns). `pg_description` reads the running
  transaction's overlay, and type/relation drops clear both ordinary and row-type
  metadata before a same-named replacement can reuse it, while `CREATE OR
  REPLACE VIEW` preserves the logical view's relation, column, and row-type
  comments. Verified byte-for-byte against PostgreSQL (corpus `59_comment`) plus
  WAL replay and captured-path unit tests.
- **LATERAL joins** — done: a FROM item (`, LATERAL (...)`, `CROSS`/`INNER`/`LEFT
  JOIN LATERAL`) may reference the columns of the items to its left and is
  re-run per outer row. A lateral subquery or set-returning function defers
  materialization: the scan (`scan_source`/`level`) assembles the outer row
  from the tables bound at shallower depths and resolves the body against it,
  reusing the correlated-subquery machinery (`Chained`, `scan_source`'s and
  `select_into_rows`'s `outer` parameter). A FROM-less lateral projection
  (`SELECT t.a*2`) types and evaluates against the outer scope; a lateral body
  with its own FROM correlates through its WHERE/ON. Any lateral entry pins the
  join to FROM order (a lateral item can't precede a table it references), and a
  `RIGHT`/`FULL JOIN LATERAL` is rejected loudly, as PostgreSQL does. Verified
  byte-for-byte against PostgreSQL (corpus `64_lateral`) plus a unit test.
- **Network address types** — done: `inet`, `cidr`, `macaddr`, `macaddr8`, a full
  type family in the mould of ranges/multiranges/bit-strings. A `NetAddr` (family
  4/6, mask bits, 16 address bytes) backs `inet`/`cidr`; MACs are fixed 6/8-byte
  arrays. **Text I/O** matches PostgreSQL exactly: IPv4 dotted-quad and canonical
  RFC 5952 IPv6 (lowercase, longest-zero-run `::` compression, `::ffff:1.2.3.4`
  v4-mapped tail), the family-default mask (`/32`, `/128`) omitted for `inet` and
  always shown for `cidr`, `cidr` abbreviation dropping trailing zero octets, MAC
  as lowercase colon hex, and `macaddr8` EUI-64 widening (`ff:fe` inserted into a
  six-byte input). `cidr` rejects host bits set right of the mask (22P02);
  bad literals are 22P02. **Casts**: text↔each type, `inet`↔`cidr` (the latter
  clears host bits), `macaddr`↔`macaddr8` (EUI-64, with the ff:fe check on the way
  back). **Ordering** is PostgreSQL `network_cmp` (family, then address, then
  mask), so `ORDER BY`/`DISTINCT`/`GROUP BY` all work; comparison, hashing and the
  sort/projected encodings carry the new tags. **Operators**: `<< <<= >> >>= &&`
  (containment/overlap — `<<=`/`>>=` are new lexer tokens and `BinaryOp` variants),
  `~ & |` (bitwise over the address), and `+`/`-` (`inet ± int`, and `inet - inet`
  → `int8` distance, with big-endian carry/borrow over the family width and loud
  overflow). **Functions**: `family`, `host`, `masklen`, `set_masklen`,
  `broadcast`, `netmask`, `hostmask`, `network`, `abbrev`, `inet_same_family`,
  `inet_merge`, plus the MAC `trunc` (folded into the numeric `trunc`) and
  `macaddr8_set7bit`. Durable through the row codec (fixed 18/6/8-byte layouts),
  the schema-carried `ColType::code` (new codes 43/44/45/47, clear of the retired
  20..=40 band — guarded by the B-095 round-trip test), the binary wire send/recv,
  `pg_type` (typcategory `I`), and constant column defaults (new `OwnedDatum`
  variants + WAL/manifest default codec). Verified byte-for-byte against
  PostgreSQL 18 (corpus `60_network`), with restart-durability unit tests and a
  `run.sh` crash-recovery assertion. No known gaps.
- **`FETCH FIRST` / `OFFSET FETCH` + `WITH TIES`** — done: the SQL-standard
  spelling of LIMIT/OFFSET (`FETCH { FIRST | NEXT } [count] { ROW | ROWS }
  { ONLY | WITH TIES }`, the count defaulting to 1), and the `WITH TIES`
  modifier — after the row limit, also return every row tying with the last on
  the `ORDER BY` keys. Parsed as part of the `ORDER BY`/`LIMIT`/`OFFSET` tail
  (with a `with_ties` flag on `Select`/`SetQuery`); `WITH TIES` without
  `ORDER BY` is 42601, as PostgreSQL requires. The tie extension runs at every
  sorted-and-limited execution path — plain (`materialized_select`), grouped
  (`grouped_select`), set-operation (`set_query`), and the FROM-less
  set-returning path — sharing one `extend_ties` helper that walks past the
  limit while the hidden ORDER BY key columns (the set-op path compares its
  output columns) still tie, NULLs equal. Verified byte-for-byte against
  PostgreSQL 18 (corpus `61_fetch_ties`), covering every path plus multi-key
  ties and OFFSET composition.
- **`CREATE DOMAIN`** — done (casts, nesting, arrays, validation, durability): a user-defined type is a base type
  plus optional `NOT NULL` / `DEFAULT` / `CHECK (VALUE ...)` constraints. This is
  the engine's first user-defined type and its first catalog-aware type
  resolution. A domain adds **no** `Datum`/`ColType` variant — a domain value is a
  plain base-type value — so the comparison/storage/wire pipeline is untouched; a
  domain is (a) a new `DomainDef` catalog (transactional MVCC + WAL + a `dom2`
  checkpoint line, mirroring the sequence catalog), (b) a schema-qualified
  identity on `ColumnMeta` (durable through the column codec and checkpoint
  `col2` field), and (c) constraint
  enforcement reusing the CHECK machinery. Column-type resolution became
  catalog-aware: an unknown type name falls back to the domain catalog (threading
  `&Storage`/`txid` into `build_column`), yielding the domain's base type + typmod
  and its identity. On write/cast a value is base-coerced (a `varchar(5)` domain's
  `22001` applies first) then checked — `NOT NULL` is `23502` "domain X does not
  allow null values", each `CHECK` is `23514` naming the violated constraint
  (unnamed CHECKs get PostgreSQL's `<domain>_check` names); a column omitted on
  INSERT inherits the domain's DEFAULT. `pg_typeof` reports the domain on a bare
  column and the base type through any expression (a new `column_domain` lookup
  hook threaded through the scan scope); `pg_type` gains domain rows (`typtype`
  'd', `typbasetype`, `typnotnull`, `typdefault`, own OID range). `ALTER DOMAIN`
  (`ADD`/`DROP CONSTRAINT`, `SET`/`DROP NOT NULL`, `SET`/`DROP DEFAULT`, journaled
  as a redefinition) and `DROP DOMAIN [CASCADE|RESTRICT]` (`2BP01` on a dependent
  column). B-172 completes the catalog-aware surface: exact `value::domain`
  casts; domains over domains with immediate-parent identity and copied
  defaults; generated array types whose elements run the full recursive
  coercion chain; and `ALTER DOMAIN` revalidation of existing rows, with the
  old definition restored atomically on failure. User-type array identities
  carry their slot and base representation through projected rows, WAL,
  checkpoints, wire OIDs, and restart. Verified byte-for-byte against
  PostgreSQL 18 (expanded corpus `62_domains`), with transactional and
  schema-collision durability tests.
- **`CREATE TYPE ... AS ENUM`** — done (arrays, renames, binary COPY): the engine's first
  user-defined *value* type. Unlike a domain, an enum is its own type with its
  own OID and an ordered label set, so it adds a `ColType::Enum(slot)` and a
  `Datum::Enum { slot, sort, label }`. The value is stored inline as its
  `(sort-key, label)` — a deliberate choice so `compare_datums` stays pure
  (enums order by the member's sort key, PostgreSQL's `enumsortorder`, never by
  label text) and row decode stays catalog-free. It is (a) a new `EnumDef`
  catalog (transactional MVCC + WAL `CreateEnum`/`DropEnum` + an `enm`
  checkpoint line, mirroring the domain catalog); (b) a slot in the column's
  `ColType`, persisted as the enum's schema-qualified identity and re-bound to a live slot on load, since slots are not stable across
  restart. Column-type resolution falls back through the enum catalog after the
  domain catalog. A write coerces text→member (invalid label is `22P02`);
  comparison against an unknown literal resolves it to a member; `pg_typeof`
  reports the enum, `pg_type` gains `typtype` 'e' rows and `pg_enum` lists the
  members. `ALTER TYPE ... ADD VALUE [IF NOT EXISTS] [BEFORE|AFTER]` (fractional
  sort keys, journaled as a redefinition) and `DROP TYPE [CASCADE|RESTRICT]`
  (`2BP01` on a dependent column). Verified byte-for-byte against PostgreSQL 18
  (expanded corpus `63_enums`), with WAL-replay unit tests and a `run.sh` crash +
  cold-start assertion. B-173 adds generated enum arrays, catalog-aware binary
  COPY label decoding, transactional `RENAME VALUE` (rewriting every inline
  scalar/array label while preserving sort identity), and `RENAME TO`
  (dependent columns, generated array row, and comments move together).
  Compact inverse undo entries preserve transaction/savepoint semantics
  without multiplying a whole enum catalog by the per-session DDL pool.
- **Data-modifying CTEs** — done for autocommit statements: `WITH x AS (INSERT
  / UPDATE / DELETE ... RETURNING ...)` may feed a `SELECT`, `INSERT`, `UPDATE`,
  `DELETE`, or `MERGE` main statement; ordinary, recursive, and modifying CTEs
  chain left-to-right, and each modifying sub-statement runs exactly once with
  its RETURNING rows becoming a materialized relation. The
  correctness subtlety is PostgreSQL's single command snapshot: all the WITH
  sub-statements and the main query see the tables as they were *before* the
  statement, so the main query counts rows a DELETE CTE just removed and reads
  the pre-image of rows an UPDATE CTE just changed — visible only through the
  CTE's own RETURNING relation. This required a per-command MVCC layer under the
  existing pending-row model: `PendingChange` gained a command-id (`cid`), the
  transaction bumps a `command_id` per statement, and `RowState::visible_at`
  takes a read snapshot (default `SNAPSHOT_ALL`, so every existing read path is
  unchanged; a data-modifying WITH statement lowers it to the command id) so a
  CTE's own writes are invisible to its siblings and the main query. Verified
  byte-for-byte against PostgreSQL (expanded corpus `65_dml_cte`) and through
  psycopg's Parse/Bind/Describe/Execute path. The bounded pending-version chain
  also retains an earlier command's image when a later data-modifying CTE
  rewrites the same row inside one explicit transaction. The DML-main implementation split
  catalog and arena lifetimes in the CTE substitution graph, so the rebuilt AST
  releases its immutable catalog borrow before storage mutates.
- **EXPLAIN / ANALYZE statistics — first real slice done.** `ANALYZE` builds
  persistent MVCC-visible table/column statistics and exposes them through
  `pg_class` and `pg_stats`. Its fixed version slab gives column statistics
  PostgreSQL's transactional/savepoint visibility while relation estimates
  retain PostgreSQL's in-place behavior; WAL recovery and provider-neutral
  manifests preserve both. The planner uses them for selectivity, join order,
  and persistent-index access. `EXPLAIN` renders that real plan in PostgreSQL's
  four formats, and `EXPLAIN ANALYZE` executes through the normal path with
  provider-neutral cache/object, WAL, memory, and serialization measurements.
  Richer statistics and the remaining Stage I executor pillars are described
  in the Stage I status above.
- **VACUUM — real operation done.** It drives the LSM checkpoint/compaction and
  object garbage-collection machinery; it is not an accept-and-ignore utility
  command.
- **Roles, ownership, and object ACLs — complete for the modeled object
  classes.** Roles/users/groups are transactional catalog objects with SCRAM
  verifiers, expiry and connection limits; membership edges carry PostgreSQL
  18's ADMIN/INHERIT/SET options and support creation-time `IN ROLE`/`ROLE`/
  `ADMIN` clauses. Ownership and grant chains cover schemas, tables, views,
  materialized views, sequences, domains, enums, and indexes. DDL requires
  ownership, schema `CREATE`/`USAGE`, type `USAGE`, and foreign-key
  `REFERENCES`; DML, view-owner execution, and sequence operations enforce
  their exact object privileges. `GRANT`/`REVOKE` grant options and recursive
  CASCADE are transactional, WAL-backed, and manifest-backed, and
  `pg_roles`/`pg_authid`/`pg_auth_members`, catalog ACL arrays, ownership
  columns, and `has_*_privilege` expose the same state. A wiped-RAM-and-disk
  recovery test proves the role graph, owners, and explicit ACL tombstones
  come from the provider-neutral object store. RAM and disk remain disposable
  caches; no provider identity enters authorization, checkpoint, WAL, or
  recovery code. ALTER TABLE recovery likewise preserves the catalog identity:
  its WAL marker and final-definition pair rewrite rows and shape in place, so
  retained indexes, ownership, ACLs, and dependency edges survive local and
  object-store replay rather than being cascaded away by a synthetic drop.
  Broader PostgreSQL command/object classes remain measured in
  `tests/postgresql18_commands.tsv`; that inventory is not collapsed into a
  false claim that the whole server surface is complete.
- **LISTEN / NOTIFY** — done: `LISTEN`/`UNLISTEN [*]`/`NOTIFY channel[, payload]`
  with PostgreSQL's transactional semantics (delivered at commit, discarded on
  rollback, subtransaction-aware, same-transaction de-duplication) and
  cross-connection delivery. The registry and delivery outbox live on the shared
  engine; the server drains the outbox after each message and fans each
  notification out to the listening connections as an asynchronous
  NotificationResponse (the notifying connection's id is the PID). Fixed pools:
  a transaction buffers at most `PER_TXN` notifications over `PER_TXN_PAYLOAD_BYTES`
  of payload and a connection listens on at most `CHANNELS_PER_CONN` channels —
  exceeding any is a loud, bounded error.

### Bug sweep (2026-07-25, between steps 5 and 6)

One deliberate adversarial pass — 52 fresh fuzzer seeds × 1200 statements
against real PostgreSQL, a 64-seed × 250-step storage-VOPR sweep, and three
new probing corpora (COPY × every type, NullTest-fold reach, explicit
indexes) — netted six fixes and one recorded gap: the silently-unenforced
UNIQUE index (B-162, the sweep's catch of consequence), the two
qual-planning error-timing divergences the fuzzer isolated (B-163 fold
reach, B-164 negator rewrite), `i64::MIN` unparseable (B-165), float8's
missing scientific notation (B-166), the absent `pg_indexes` view, and
`real`-is-really-float8 recorded open as B-167 (its own PR, the smallint
playbook). The storage half came through the sweep clean.

The sweep also exposed a CI blind spot the moment coverage.sh started
printing run.sh's FAIL lines: crash torture had failed on **every** main
coverage run since it landed — dead on `import psycopg` (the CI venv only
carried psycopg2) and reduced to one uncounted NOTE line by the tolerant
path built for docker-less laptops (B-168). The repair is structural, and it
also brought the coverage job under the 15-minute CI ceiling (it ran ~30
minutes; the policy is that **no CI job runs past 15**): run.sh's steps are
now grouped (`POS3QL_RUN_GROUPS` — proto, dur, overlay, ingest, torture,
tls, spilldiff, each self-contained) with per-step wall-clock reporting, and
the coverage workflow fans out into parallel shards (`COVERAGE_SHARD`),
each running its slice *strictly* — a failing step fails the shard — and
exporting an lcov tracefile; `tools/coverage-merge.py` unions the shards
and holds the 70% floor over the merged whole, since one shard's percentage
alone means nothing.

Getting under the ceiling turned up a real scaling wall, not just slow CI.
The overlay-pressure step spent ~256 s of its budget in one place: it loaded
5000 rows into a `PRIMARY KEY` table, and uniqueness enforcement is O(rows)
per insert once the table spills — every insert probes the whole spilled SST
forest for a duplicate, quadratic overall (B-169, the deferred
secondary-index forest). The step never even *asserted* uniqueness; the
constraint was pure cost. Rewritten to earn it: the scale table drops the
constraint (the overlay/spill read path is what it exercises, now ~17 s),
and a separate bounded 1500-row `PRIMARY KEY` table asserts the property
that actually matters — a duplicate of a key long evicted from the overlay
is still caught against its spilled row. **(B-169 fixed 2026-07-25:** a
per-constraint in-RAM value index turns that probe into a hash seek, so the
scale table carries a `PRIMARY KEY` again and the spill-boundary check runs at
5000 rows — see B-169 and Stage E gap 4 below.**)** The crash-torture shard, still ~15
minutes at 12 rounds on the instrumented binary, is split across two seeds
at half depth each (`POS3QL_TORTURE_ROUNDS`/`POS3QL_TORTURE_SEED`), which
also widens the random coverage. Torture is a correctness shard, not a
coverage one: it kill -9's every server it starts, and SIGKILL never runs the
profiler's atexit flush, so a torture shard yields no `.profraw` and stays out
of the coverage merge (`runtest:*`) — it earns its place by running and
passing, which for the whole of its prior life (B-168) it did neither. Being
coverage-free, it builds *uninstrumented* and so runs several times faster,
which is what keeps its two seed-shards comfortably under the ceiling.

The same `runtest:*` reasoning generalises into which groups can be coverage
shards at all: only those whose work lands on a server that shuts down
*gracefully* and flushes a profile. That is the base-port server run.sh
stops at cleanup (sql's in-process tests + corpora, wire-durability) and the
differential harness's own server (spilldiff). Everything else run.sh drives
either kill -9's a side server — overlay flushes nothing, its idle base-port
server would give ~zero — or, like ingest, merely iterates at scale the same
spill/merge/cold-start code the forced-spill differential already
instruments. So overlay, ingest and tls join torture as uninstrumented
correctness shards: they earn their place by running and passing, and their
line coverage is carried by the coverage shards. To make the coverage shards
that *do* flush deterministic rather than racing the caller, run.sh's cleanup
now stops the base-port server gracefully and waits for it to exit — up to
five seconds, then forces it — before returning, so `cargo llvm-cov report`
never reads `target/` while the profile is still being written. The floor is
carried comfortably by the differential corpora alone (~80% on their own),
with wire-durability and spilldiff adding the durability, WAL and forced-spill
paths on top.

### real/float4 as a genuine type (2026-07-25, B-167)

`real` was the last of the P14 "distinct types" still faked as its wider
sibling — stored, sent, and printed as `double precision`, so every value
outside float8's fixed-notation window came out wrong. It is now a real
`Datum::Float4(f32)` on the smallint playbook (B-126): OID 700, typlen 4,
4-byte wire, input rounded through `f32`, `float4out` output (shortest f32
digits, fixed notation for decimal exponent in [-4, 6)), single-precision
`real op real` arithmetic widening to double precision when mixed,
`sum(real)` accumulated in `f32` while avg/variance fold to f64, and typed
`abs`/rounding/`greatest`/`least`/UNION/`real[]` (OID 1021) and JSON output.
On disk it keeps the historical 8-byte layout and narrows at decode, so no
data migrates. Corpus `44_float4` pins the surface against PostgreSQL 18.
The sweep also turned up four Boyscout fixes (float→int now ties to even;
`float8 % float8` rejected at plan time; smallint/real no longer JSON-quoted;
smallint/real UNION unifies) and one honest open item, B-170 (shortest-float
boundary output), since fixed for `real` — see below.

### Exact float output via PostgreSQL's Ryū (2026-07-25, B-170 — closed)

`real` and `double precision` output diverged from PostgreSQL at rounding
boundaries (~0.3% of reals, ~0.07% of float8s). The cause, pinned precisely:
PostgreSQL builds its Ryū float formatter **without** `STRICTLY_SHORTEST`, so
`acceptBounds` is always false and it keeps an extra digit at rounding
boundaries (`87535936::real` → `8.7535936e+07`, where Rust's `{:e}` gives the
equally-valid `8.753594e+07`; `632900811120955.2::float8` likewise) — not the
tie-to-even rounding first suspected. Fixed by porting PostgreSQL 18.4's exact
`src/common/f2s.c` and `d2s.c` Ryū into `src/sql/ryu.rs` (`f32_shortest`/
`f64_shortest`, `acceptBounds = false`) and feeding the digits into the shared
notation formatter. The f32 tables are transcribed verbatim; the larger 128-bit
double tables are generated from the Ryū definition (`DOUBLE_POW5_BITCOUNT`
121, inverse 122) and validated to match PostgreSQL. 4000+ random f32 and
10000+ random f64 bit patterns — subnormals and the extremes (`5e-324`,
`DBL_MAX`) included — diff empty against PostgreSQL. Both types are now
byte-exact; B-170 is closed.

### ALTER TABLE ALTER COLUMN family (2026-07-25)

`ALTER TABLE` handled ADD/DROP/RENAME COLUMN and RENAME TO but not the ALTER
COLUMN sub-commands. Added `ALTER [COLUMN] col SET/DROP DEFAULT` and
`SET/DROP NOT NULL` (the COLUMN keyword optional, as in PostgreSQL): SET
DEFAULT evaluates and casts the constant through the column's type on the
same path CREATE TABLE uses; SET NOT NULL scans the committed rows and
refuses with 23502 (naming the column and relation) while a NULL is present,
then is enforced on later DML; DROP NOT NULL is refused on a primary-key
column (42P16). All changes journal as the existing DropTable+CreateTable
shape swap, so they survive a kill -9 restart. Corpus `45_alter_column`
matches PostgreSQL 18. `ALTER COLUMN TYPE` followed (2026-07-25): `TYPE t`
and `SET DATA TYPE t`, with the stored rows rewritten through the row-rewrite
path — the value casts through the assignment cast (a `alter_type_auto_castable`
mirroring `pg_cast` plus the to-string/from-string I/O rule, so an
explicit-only cast such as text→int is refused with 42804 as PostgreSQL does),
or, with `USING`, through an expression evaluated per row over the old
columns; the type modifier is applied and the change survives a restart.
Corpus `46_alter_column_type`. `ADD`/`DROP CONSTRAINT` followed
(2026-07-25): `ADD [CONSTRAINT name] {CHECK | UNIQUE | PRIMARY KEY | FOREIGN
KEY}` builds the constraint into the definition (reusing CREATE TABLE's
`attach_constraints`) and validates every committed row against the whole
constraint set before attaching — the added one is the only one that can
fail, surfacing the INSERT-path SQLSTATE (23514/23505/23503, or 23502 for a
new PK's NOT NULL); it is then enforced on later DML. `DROP CONSTRAINT [IF
EXISTS] name` removes a CHECK / table-level UNIQUE-or-PK / FK by its stored
name, or a single-column PK/UNIQUE by its generated name (`<table>_pkey`,
`<table>_<column>_key`), with the `IF EXISTS` skip notice. Corpus
`47_alter_constraint`. Known gap (its own follow-up): a single-column
UNIQUE/PK added under an *explicit* name is stored as a column flag that does
not retain that name, so `DROP CONSTRAINT <explicit_name>` cannot find it —
retaining names for single-column keys is the fix. `RENAME CONSTRAINT old TO
new` followed (2026-07-25): renames a CHECK / table-level UNIQUE-or-PK / FK by
its stored name, refusing a name already in use (42710) and a missing old name
(42704); the renamed constraint keeps enforcement and is reachable by the new
name. Same single-column-key naming limitation applies (a single-column
UNIQUE/PK flag has no stored name to rename). Corpus `49_rename_constraint`.
A related pre-existing gap surfaced and is noted for its own PR: CHECK
constraints auto-name as `<table>_check` rather than PostgreSQL's
`<table>_<column>_check` for column-level checks (and lack the numeric
disambiguation suffix). **All three follow-ups landed 2026-07-25** —
single-column-key naming (an explicitly named single-column UNIQUE/PK is now a
first-class named key; an unnamed one materializes on RENAME; `DROP NOT NULL` on
a PK column is refused whichever form it takes; `find_conflict` now consults the
table-level keys too, closing a multi-column-UNIQUE ON CONFLICT gap; corpus
`differential_exact/03_named_constraints`), CHECK auto-naming
(`<table>_<column>_check` for a one-column predicate, `<table>_check` otherwise,
with numeric disambiguation; corpus `differential_exact/02_check_naming`), and
comma-separated multi-action `ALTER TABLE` (applied in PostgreSQL's fixed pass
order, atomic — all row content validated against the transformed images before
any journaling, which also closed a latent mid-rewrite-cast atomicity gap and a
NOT-NULL-over-a-spilled-table check that read the evicted overlay; corpus
`50_alter_multi`).

### VACUUM and ANALYZE (2026-07-25)

Both were flat syntax errors, which broke tools that issue them after bulk
loads. `VACUUM [options] [table [(cols)] [, ...]]` now reclaims space by
driving a checkpoint — the LSM's flush + compaction, which prunes superseded
versions and tombstones — subsuming any named table; with
`object_store = off` there is
no store to compact to and it succeeds with nothing to reclaim, as PostgreSQL
does on a clean table. It is non-transactional (25001 inside a block).
`ANALYZE [options] [table [(cols)] [, ...]]` is allowed inside a transaction.
The original implementation only validated and walked its targets. The
adaptive-planning slice completed the semantics: it computes MVCC-visible
table/cardinality/width statistics and per-column null/distinct/width
statistics, updates only named columns for a targeted analysis, persists the
result through WAL and the provider-neutral manifest, exposes it through
`pg_class` and `pg_stats`, and feeds the actual planner. PostgreSQL's unusual
transaction boundary is preserved: column statistics are private until commit
and roll back through savepoints, while relation row/page estimates update in
place and survive rollback. Corpus `48_vacuum_analyze` retains the syntax/error
contract; engine tests cover targeted updates, empty tables, cross-session and
savepoint visibility, WAL recovery, planner use, and cold object-store restart.
(`VERBOSE` remains omitted because its INFO progress stream is not implemented.)

### ON CONFLICT arbiter fidelity (2026-07-27)

`ON CONFLICT` had shipped as `DO NOTHING`/`DO UPDATE` with `excluded.*`, but the
arbiter — the specific unique constraint the clause acts on — was ignored:
`find_conflict` treated a violation of *any* unique as the conflict, so
`ON CONFLICT (a)` and `ON CONFLICT (b)` behaved identically and, worse, a
conflict on a unique *other* than the named target silently updated the wrong
row instead of raising `23505`. And `RETURNING` on the `DO UPDATE` branch
emitted nothing (the update applied, but its row was dropped — an
accept-and-ignore of a client-observable clause).

Both are now PostgreSQL-faithful. The arbiter is resolved once per statement as
a data-independent analysis step: an `ON CONFLICT (columns)` target must match a
unique/exclusion constraint on exactly that column set (order-independent), a
new `ON CONFLICT ON CONSTRAINT name` names it directly (a UNIQUE/PK, a unique
index, or a single-column key's synthesized `<table>_pkey` / `<table>_<col>_key`
name), and `find_conflict` then conflicts on that arbiter alone — so a violation
of a different unique falls through to the ordinary `23505`. The resolution
errors fire regardless of whether any row conflicts, byte-for-byte with
PostgreSQL: `42601` (`DO UPDATE` with no arbiter), `42P10` (target matches no
unique), `42703` (target column absent), `42704` (named constraint absent).
`RETURNING` on `DO UPDATE` now decodes the arena-encoded updated row and
projects its post-update values (and `excluded.*`), composing with a multi-row
upsert (each inserted or updated row appears) and the data-modifying-CTE capture
path. Corpus `66_on_conflict`, with unit tests.

### Row-locking clauses — FOR UPDATE / SHARE (2026-07-27)

`SELECT ... FOR UPDATE` (and `FOR SHARE` / `FOR NO KEY UPDATE` / `FOR KEY
SHARE`, each with an optional `OF table` list and `NOWAIT` / `SKIP LOCKED`) was
a flat syntax error — a real hole, since every ORM emits it
(`select_for_update()`, `.lock`, `with_for_update()`). It now parses, and its
analysis-time semantics are byte-for-byte with PostgreSQL: the clause returns
the query's rows unchanged in a single session, the restrictions raise `0A000`
with the clause's own keyword (aggregate / `GROUP BY` / `HAVING` / `DISTINCT` /
window function / set operation), and an `OF` target that names no FROM relation
raises `42P01` (an aliased table is reachable only by its alias). A FROM-less
`SELECT 1 FOR UPDATE` is allowed (it locks nothing) — which surfaced a latent
bare-alias bug the fix also closes: `for` and `fetch` were missing from the
implicit-alias reserved set, so `SELECT 1 FOR UPDATE` / `SELECT 1 FETCH FIRST 1
ROW ONLY` swallowed the keyword as a column alias. The clause validates in the
simple-query, extended-protocol, and `DECLARE CURSOR` paths. Corpus
`67_for_update`, with unit tests. The one behavior deliberately left unmodeled
is the cross-transaction lock *contention* (block / `55P03` / skip when another
transaction holds the row) — pos3ql's run-to-completion core cannot block, and
the no-lost-update safety is already delivered by the existing `40001`
write-conflict detection; documented as B-175, the same architecture as B-004.

### current_setting + a cross-connection GUC leak (2026-07-27)

`current_setting(name [, missing_ok])` — the function form of `SHOW`, which
drivers and tools call constantly to introspect settings — was missing (a flat
`function does not exist`). It now returns any readable setting's value as text,
matching PostgreSQL byte-for-byte: the same value `SHOW` reports (fixed server
params + session GUCs, published per statement like the session user), a
case-insensitive name, composition inside expressions, `42704` on an unknown
setting, and `NULL` when `missing_ok` is true. `server_version_num` was added to
the fixed parameters (so both `SHOW` and `current_setting` report it), and the
`SHOW ALL` name list is now the single shared `SETTING_NAMES`. Corpus
`68_current_setting`, with a unit test.

Probing it surfaced a pre-existing **cross-connection session leak**: connection
slots are statically pre-allocated and recycled, but `Conn::open` reset the
arena, transaction, prepared statements and portals while leaving `guc`, the
cursor pool, the auth flow, and any in-flight `COPY` carrying the *previous*
client's state — so a `SET search_path = …` on one connection was still visible
(through `SHOW` / `SET` / `current_setting`) to the next client to reuse that
slot. `open` now resets all of it, closing the whole slot-reuse leak class.

### Transactional GUC mutation (2026-07-27, B-176 — closed)

The write side is now complete as one state machine rather than a punctual
`set_config` patch. `set_config(name, value, is_local)` mutates during expression
evaluation, returns the canonical stored value, and a later
`current_setting` in the same row sees it; a NULL new value means RESET, a NULL
scope means session, and invalid/unknown/read-only settings raise PostgreSQL's
`22023`/`42704`/`55P02` errors at the call site. Rendering settings changed by
the function (`bytea_output`, `DateStyle`, `TimeZone`,
`client_min_messages`) take effect on that same output row through a
statement-scoped live render context.

The larger bug class surfaced while wiring it: `SET LOCAL` had been silently
parsed as session `SET`, ordinary `SET` survived a transaction rollback, and
`RESET` / `RESET ALL` did not parse. `GucState` now keeps separate visible,
transaction-start, and eventual-session snapshots behind fixed-size
interior-mutable state. Session changes persist only after commit; local changes
end at commit or rollback; `SET` followed by `SET LOCAL` publishes the SET value
after commit; a session assignment following an unrelated local overlay changes
only its named setting rather than promoting that overlay; and savepoint
rollback restores both tracks while RELEASE keeps them. RESET uses the
connection's startup value (including startup-packet settings), not a hard-coded
process default. Corpus `75_guc_mutation` exercises the whole surface
byte-for-byte against PostgreSQL 18.4, with an engine test covering the same
transaction/savepoint transitions.

That verification exposed a separate harness blind spot (B-177): the
sqllogictest runner configured 64 tables but left the new uniqueness-index pool
at its 16-index default, so `select5` exhausted the pool at table 17 and reported
424 cascading "unsupported" blocks while the wrapper still passed. The harness
now sizes 64 value indexes too; all 3205 blocks are again real matches, with
zero unsupported and zero divergence.

The final full-suite pass caught another quality-gate false positive (B-178):
the SQLSTATE scanner treated any standalone five-character uppercase string as
an inline code, even ordinary SQL such as `"BEGIN"`. Bare literals are now
classified only when they immediately follow `sql_err!(`, with a regression
test proving the scanner still rejects a multiline `"22P02"`.

### SELECT ... INTO (2026-07-27)

`SELECT ... INTO table` — the older spelling of `CREATE TABLE AS`, still emitted
by scripts and some ORMs — was a flat syntax error. It now materializes the
query's result into a new table, matching PostgreSQL: `INTO [TABLE] name` after
the select list, computed/renamed columns, `SELECT *`, `WHERE`/`GROUP BY`/
`ORDER BY`, and a set operation carrying `INTO` on its first branch; re-running
into an existing table is `42P07`, and `INTO` in a subquery / CTE / set-op branch
is `42601` ("SELECT ... INTO is not allowed here"), exactly as PostgreSQL scopes
it. The implementation adds no AST node: the parser recognizes the clause only
where it is legal (a top-level query, gated by an `allow_into` flag a subquery
clears), then reconstructs the query text with the `INTO` clause excised and
hands it to the existing CREATE TABLE AS machinery — so the whole materialize /
column-inference / durability path is reused unchanged. `into` was also added to
the bare-alias reserved set (it stays usable as an explicit `AS into`). Corpus
`69_select_into`, with a unit test. `SELECT INTO TEMP` is not supported, for the
same reason `CREATE TEMP TABLE` is not: this engine has no temporary-table
storage class yet.

### COPY (query) TO STDOUT (2026-07-27)

`COPY (SELECT ...) TO STDOUT` — the way psql `\copy` and export tooling stream a
query's result rather than a whole table — was a flat "expected table name". It
now runs the parenthesized query and formats each result row exactly as a table
`COPY TO` does, matching PostgreSQL byte-for-byte in text, CSV, and binary: the
header line, `NULL` marker, `DELIMITER`, `QUOTE`/`ESCAPE`, CSV quoting of
embedded delimiters/quotes/newlines, and `FORCE_QUOTE` (resolved against the
query's *output* column names). The query may aggregate / group / join like any
`SELECT`. Implementation: the parser treats a `(` after `COPY` as a query source
(a column list needs a table, so it is unambiguous) and captures the query text;
execution describes the query for its columns, then drives `select_into_rows`
with an emit closure that reuses the existing `datum_wire_text` + COPY field
encoders — so the output path is shared with table `COPY TO` and cannot drift.
Only `TO STDOUT` is accepted for a query (a query is never a `COPY FROM` target).
Corpus `70_copy_query`, with a unit test.

### JSON date-time rendering (2026-07-27)

`to_json` / `row_to_json` / `to_jsonb` / `json_build_object` / `json_agg` /
`array_to_json` rendered a `timestamp` or `timestamptz` in the space-separated
`::text` form (`2020-01-01 00:00:00+00`) where PostgreSQL uses ISO 8601 —
a `T` between date and time and, for `timestamptz`, a full `+HH:MM` offset
(`2020-01-01T00:00:00+00:00`). Any application serializing a timestamp column to
JSON got the wrong string. Fixed with a JSON-specific `format_timestamp_json`
and explicit `Datum::Timestamp`/`Timestamptz` arms in the JSON writer;
fractional seconds still trim trailing zeros, and `date`/`time`/`timetz`/
`interval` keep their ordinary text form (which already matched). Corpus
`71_json_datetime`, with unit-test coverage. (The session-zone shift PostgreSQL
also applies to a JSON `timestamptz` is the same pre-existing limitation the
plain `::text` render carries — the whole `Datum` Display path renders at UTC —
and is unchanged here.)

### Set-returning functions in a value subquery (2026-07-27)

A set-returning function in the SELECT list of an `IN` / `ANY` / `ALL` / `ARRAY`
subquery (`WHERE id IN (SELECT unnest(ARRAY[1,3]))`) raised "set-returning
function called where not allowed", where PostgreSQL expands it to the set of
rows. SRFs already worked in derived-table, scalar, and `EXISTS` subqueries — the
value-subquery path (`run_subquery`) just had a narrower routing condition:
the grouped / DISTINCT / windowed cases went to the row-source executor (which
handles SRF expansion) while a plain SRF projection was evaluated per row and
rejected. Fixed by routing an SRF subquery through that same executor via
`find_srf`, so `IN`/`ANY`/`ALL`/`ARRAY(subquery)` over an `unnest` /
`generate_series` now expand correctly, including the empty-array and NOT IN
NULL semantics. Corpus `72_srf_subquery`, with a unit test.

### jsonb containment operators (2026-07-27)

The `jsonb @> jsonb` / `jsonb <@ jsonb` deep-containment operators — the primary
way applications query jsonb — errored ("operator range operator does not accept
jsonb"): `@>`/`<@` dispatched to the array and range paths but had no jsonb
branch. Added PostgreSQL's `JsonbDeepContains`: an object contains an object when
every contained member matches a container member (recursively); an array
contains an array when every contained element is deeply contained by some
container element; an array contains a bare primitive that appears as an element;
scalars match by value (so `1.0 @> 1`, via numeric comparison, not text); every
other type pairing is non-containment. An unknown string literal coerces to jsonb
against a jsonb operand (`doc @> '{"k":1}'`), while plain `json` still has no
containment operator (`42883`), as PostgreSQL. Implemented on the existing `Json`
value tree (`json::contains`); the key-existence operators `?`/`?|`/`?&` already
worked. Corpus `73_jsonb_containment`, with unit-test coverage.

### Array slice subscripting (2026-07-27)

Array slices `a[lower:upper]` — with either bound optional (`a[:2]`, `a[2:]`,
`a[:]`) — were a lexer error ("unexpected ':'"): a lone colon was rejected, so
only single-element subscripts parsed. The lexer now emits `:` as a token (the
parser still rejects it anywhere but a slice), a new `Expr::Slice` carries the
optional bounds, and evaluation extracts the 1-based inclusive range, matching
PostgreSQL: bounds clamp to the array (a lower below 1 clamps to 1, an upper past
the end clamps to it), a non-overlapping range is an empty array, and a NULL
bound yields NULL. The slice keeps the *array* type (unlike a subscript, which
yields the element type) and the base column's name (`m[1:2]` → `m`, a sliced
`ARRAY[...]` → `array`), so it composes with `||`, `array_length`, `unnest`, and
the rest. Corpus `74_array_slicing`, with unit-test coverage.

### The order (dependency-driven)

1. **Storage VOPR (Stage H)** — the virtual object store + grid disk with
   PCG-driven fault injection and seeded invariant checks. Deliberately
   *before* the durability surgery, so steps 2–4 are born simulation-tested
   rather than retrofitted. **Done** (see Stage H status above).
2. **Durability to the bucket (gaps 1 + 3)** — group-commit WAL-segment PUT
   on the acknowledge path; asynchronous checkpoint through the reactor.
   **Status (2026-07-24): landed as defaults + the sliced checkpoint.**
   Commit-durable-on-bucket is now mandatory whenever
   `object_store = on`
   (`wal_upload`/`wal_upload_sync` resolve on and an explicit off value is
   rejected; run.sh proves the posture with an ack → immediate kill -9 → wiped-disk
   cold start, no drain pause, no checkpoint). The checkpoint stall is
   broken up: the auto-checkpoint is **sliced** — one table's SST/delta/
   merge work per beat, a beat per query message plus idle-loop beats with
   backoff, publishing only in a beat where no table changed since its
   slice (per-table `generation` behind the new `Table::mark_dirty` choke
   point); the explicit `CHECKPOINT` statement drives the same beats to
   completion atomically, so there is one code path. The storage VOPR
   promptly caught the new machinery's one real hazard before it ever
   merged — a publish failing *after* its CAS+installs (in the advisory GC
   tail) left the sweep active over swapped scratch, and the retry CAS'd a
   manifest whose lsn claimed state its lists did not carry, shadowing the
   local WAL tail — fixed by ending the sweep at the install point and
   demoting GC to logged-and-retried cleanup; two pre-existing bugs fell
   out of the same investigation (B-158 ambiguous manifest-CAS lockout,
   B-159 GC failure mislabeling a completed checkpoint). *Deliberately
   deferred from this step:* cross-connection group commit (holding acks so
   one segment PUT covers many concurrent commits) — a throughput
   optimization, not a correctness gap (every ack already follows its PUT);
   its ack-deferral plumbing belongs with the reactor's suspendable
   row-source work (Stage I pillar 2). Per-block beat pacing for a single
   huge table's slice remains Stage E's item in step 3.
   **Provider-neutral follow-through (2026-07-29): done at the architectural
   boundary.** The S3-compatible client and deterministic simulator implement
   one semantic object-store contract; no provider name, signing rule,
   endpoint quirk, or retry dialect crosses into WAL, checkpoint, compaction,
   recovery, cache, or query code. Generic `object_store*` configuration is the
   documented surface and legacy `s3*` keys are strict aliases. Hosted GCS,
   Azure Blob Storage, and equivalent qualification remains an adapter/gateway
   deployment test, not a storage-engine branch.
3. **Stage F MVCC + Stage E beat pacing** — LSN-keyed row versions,
   snapshot-aware merge reads, compaction retention above the oldest-snapshot
   watermark, merge work amortized across statements.
   **Status (2026-07-29): beat pacing and object-resident historical MVCC landed.**
   The merge is a background job bounded to a few block transfers per beat;
   committed histories are keyed by LSN; REPEATABLE READ registers an
   oldest-live-snapshot watermark; and checkpoint/compaction retention respects
   it. Versioned row SSTs and the merge iterator now retain histories in the
   object store against that watermark; the resident eight-version chain is
   staging space between publications, not a durable-history limit.
4. **The map spills (gap 2)** — block-resident row index; secondary indexes
   as the LSM forest; block compression and the multi-block index / sized
   filters (the remaining Stage C refinements) ride along since they touch
   the same format.
   **Status (2026-07-24, later): the overlay is live — a table's row count
   is no longer bounded by RAM.** The map now holds only the working set:
   pending changes, heap-resident rows, deletion markers, and whatever hot
   entries pressure has not yet shed. Everything else lives only in the
   bucket and is reached through the seam — the merged walk (per-member
   block cursors leased from a fixed context pool, newest member winning a
   rowid, tombstones suppressing) for enumeration, bloom-gated point probes
   for lookups, and synthesis of `RowState` on the way out, so no call site
   knows the difference. Writes to entry-less rows synthesize their
   committed home on entry (a pending change must not hide the old image
   from uniqueness scans); a committed DELETE leaves a shadowing marker
   until the next publish's install makes the SSTs themselves say deleted
   (the storage VOPR caught the resurrection the first build without it);
   map-occupancy pressure now drives spilling exactly as heap pressure does
   (a table of tiny rows fills its map long before its heap); and cold
   start installs no entries at all — O(manifest), not O(rows), with the
   rowid floor read from each SST's last data block in three block reads.
   Proven by the VOPR with `table_rows` dropped far below the live row
   count (the overlay sheds and re-fetches constantly under fault storms),
   and by a run.sh step pushing 5000 rows through a 1024-entry map —
   deletes, updates, point reads, counts, and a wiped-disk cold start of a
   dataset larger than the map. Remaining in step 4 at that point: the
   secondary-index LSM forest and block compression (with the multi-block
   index and sized filters), on top of the settled overlay.
   **Status (2026-07-25): the format follow-ups landed.** Data blocks are
   **LZ4-compressed** when it pays (a hand-rolled implementation of the
   published LZ4 block format — the same dependency-policy footing as the
   hand-rolled SHA-256 and TZif — with the writer keeping whichever of
   raw/compressed is smaller, strict bounds-checked decompression, and the
   block *type* now traveling through every cache tier so a cached
   compressed block still says so). The index went **two-level** — leaves in
   the classic count-prefixed layout under a magic-prefixed root carrying
   per-leaf block counts — so an SST is no longer capped at one index
   block's worth of data (~1.6 GB); the cap is now terabytes, and every
   read path (point get, probe, scans, bounded-scan resumption, the
   overlay's ordinal cursors, cold-start rowid floors) navigates both
   shapes through shared resolvers. Filters are **sized from a ladder**:
   every key inserts into three candidate sizes and the finish keeps the
   smallest still giving ~10 bits per key, so a small SST no longer pays
   128 KiB for a handful of rows. (Stage C's own "remaining: multi-block index and sized filter" is
   thereby closed.)
   **Status (2026-07-29): value indexes became caches, then gained an
   object-resident authority.**
   The B-169 `value_hash → rowid` multimap still accelerates PRIMARY KEY and
   UNIQUE checks, but exhausting `value_index_rows` now marks that cache
   incomplete and falls through to the authoritative row SSTs; it can no
   longer cap a constrained table or create a false-negative probe. Named
   unique and non-unique indexes share the same binding lifecycle, including
   immediate rebuild on transactional CREATE/DROP, and the single-table
   planner consumes complete single-column equality caches while preserving
   row order and declining around pending images or unsafe cross-type
   coercions. The object generation closes the object-scale fallback: key-only
   immutable blocks persist `(encoded tuple, equality hash, rowid, commit LSN)`
   through the same provider-neutral cache stack; chained roster roots are named
   by the manifest CAS alongside row SSTs. An incomplete RAM map probes that
   generation and every newer resident change. Equality and composite-unique
   candidates are hash-filtered; range candidates compare encoded values; all
   are rechecked against the authoritative MVCC row so old key versions can
   only be false positives. Each dirty checkpoint performs a bounded-memory
   full generation rebuild, which is the compaction step, and GC traces its
   roster after publication. Empty generations, transactional CREATE/DROP,
   cache-cold recovery, and old manifests/WAL are covered. Index direction and
   null placement survive AST → WAL → manifest → catalog reconstruction.
   RAM and local disk never become authoritative.

   The SQLLogicTest runner now reclaims every file's catalog objects and only
   classifies errors whose message explicitly identifies an absent feature or
   static bound. `42601`, missing tables, bad casts, and invalid values are no
   longer broad escape hatches. The former eight- and sixteen-relation parser /
   executor ceilings are gone: one shared 64-relation envelope covers the full
   configured catalog and the vendored `select5` workload. Qualification
   planning retains 128 top-level terms, so a 64-relation equality chain is
   pushed down incrementally instead of degenerating into a Cartesian product.
   The full unsharded PostgreSQL 18.4 replay is now 10,911/10,911 exact,
   unsupported 0, divergence 0, and CI carries a zero unsupported ratchet.
   Range-table, decode, predicate, and match scratch are statement-arena slices
   sized to the actual FROM clause. A merged SST walk releases its block context
   before invoking the row callback, so recursively nested joins reuse one
   startup-sized context. A walk-owner token invalidates only residency claims
   overwritten by nested reuse; increasing the SQL envelope therefore reserves
   neither recursive stacks nor one nine-block object reader per possible join
   depth.

   *Earlier (same day):* **the choke points went in first** — the first half of the
   two-PR shape this step takes (the query.rs-split playbook: mechanical
   seam first, semantics behind it after, each diff-gated). Every consumer
   that walked `Table.rows` by hand — the join scanner, the DML collectors,
   TRUNCATE, ALTER's rewrite, every uniqueness and foreign-key scan, the
   checkpoint's slice collection — now goes through three storage seams:
   `for_each_row_state` (states by *value*, errors threaded), `row_state`,
   and `visible_row_count`. Behavior-identical today (the seam walks the
   map); the second half flips the seam's internals to the overlay model —
   the map holding only pending + hot rows, SST-resident rows enumerated by
   a newest-wins merge over the spill list, entries evicted under pressure,
   cold start installing no entries — and no call site changes again. The
   VOPR promptly paid for itself once more during this PR: a fresh seed
   sweep caught B-160 (the ambiguous-CAS recognition of B-158 failing once
   state advanced past the lost write; fixed structurally with a manifest
   writer-identity line).
5. **Compatibility wave** — COPY (+ the pg_dump round-trip milestone),
   server-side TLS, ALTER TABLE breadth, roles/GRANT, EXPLAIN,
   VACUUM/ANALYZE as real operations, LISTEN/NOTIFY.
   **Status (2026-07-25): COPY landed** — the wave's largest hole. The
   wire subprotocol (CopyInResponse/CopyOutResponse/CopyData/CopyDone/
   CopyFail, the connection holding its query cycle open in copy-in mode,
   `\.` honored for pg_dump scripts), PostgreSQL's text format exactly
   (tab-delimited, `\N` nulls, the full escape set both directions,
   literal-carriage-return refusal), values through each type's input
   function on the way in and the *wire output function* on the way out
   (styled timestamps, GUC-honoring bytea, `t` booleans — a shared
   `datum_wire_text` so COPY and SELECT can never drift), rows through
   the same insert core as INSERT (defaults, sequences, NOT NULL,
   uniqueness, CHECK, foreign keys), one transaction per COPY (aborts
   store nothing; inside BEGIN it commits and rolls back with the
   transaction), insertion-order output, and column lists both ways.
   Proven differentially: corpus 40 runs
   the whole surface — escape round-trips, typed columns, constraint
   aborts, transactional behavior — against real PostgreSQL, which also
   caught two fidelity bugs en route (boolean COPY output as `true`
   instead of the output function's `t`, and column-level PRIMARY KEY
   violations naming `<table>_<column>_pkey` where PostgreSQL names
   `<table>_pkey` — the catalog already knew better).
   **CSV followed (2026-07-25):** `FORMAT csv` both directions with the full
   option set — `DELIMITER`, `NULL`, `HEADER`, `QUOTE`, `ESCAPE`,
   `FORCE_QUOTE (*|cols)`, `FORCE_NOT_NULL`, `FORCE_NULL`, `ENCODING` — and
   both the modern `WITH (FORMAT csv, ...)` and legacy `CSV HEADER ...`
   spellings real tools emit. CSV quoting is exact (a field is quoted only when
   it holds the delimiter/quote/newline, or is forced, or matches the NULL
   string; the quote and escape characters double inside), and a CSV input row
   is found quote-aware in the connection layer so a newline inside a quoted
   field spans CopyData chunks rather than splitting the row. Corpus
   `51_copy_csv` runs it against real PostgreSQL 18. **Binary format followed
   (2026-07-25):** `FORMAT binary` both directions — the `PGCOPY\n\377\r\n\0`
   file signature, per-row int16 field count, per-field int32 length (or -1 for
   NULL), the -1 trailer, and CopyIn/CopyOutResponse's format code — with a
   quote-free, length-based copy-in state machine that assembles a row spanning
   several CopyData chunks before decoding it. Every column of the scalar /
   numeric / temporal / uuid / bytea / json tower encodes and decodes byte-exact
   against PostgreSQL (including the ones the extended-protocol path already got
   right — `smallint` as a true int2, `timetz`'s westward zone flip, numeric's
   base-10000 groups, interval, timestamptz). The composite types now follow the
   real binary wire format too — **arrays** (int32 ndim / has-null / element OID,
   the one dim descriptor, per-element int32 length + binary, NULL element as
   length -1, empty array as ndim 0), **ranges** (the flags byte —
   empty/inclusive/infinite — then each finite bound as int32 length + binary),
   **multiranges** (int32 range count, each range length-framed), and **bit
strings** (int32 bit length then MSB-first packed bytes) — encoded on the
   arena-aware `copy_out` path (range bounds need typed parsing) and decoded via
   the shared per-type receiver; only anonymous `record`, which has no stored
   column representation, stays refused (0A000). The extended-protocol *binary
   results* path shares the array and bit codecs (arena-free), and falls back to
   canonical text only for ranges/multiranges, whose bounds the arena-free wire
   primitive cannot re-parse — the rare case of a client Binding binary results
   for a range column. Binary data is not line-oriented, so it cannot be fed
   through a psql `-f` corpus; `tests/external/copy_binary_diff.py` drives both
   engines over the wire with psycopg and checks TO-binary byte-identity plus
   FROM-binary round-trips in both directions across the full scalar tower and
   every composite (arrays with NULL/empty, ranges with empty/infinite bounds,
   multiranges, bit/varbit), wired into the CI differential. **Binary
   *parameters* of composite types** are also accepted now: the extended-protocol
   Bind decoder (`decode_binary_param`) routes an array / range / multirange /
   bit OID through the same COPY-binary receiver via a new `ColType::from_oid`
   reverse map, and a Bind resolves any parameter the client left untyped
   (OID 0 — an empty range has no subtype to declare) from its use, including a
   `$n::type` cast in the select list, so it decodes as its real type without a
   prior Describe. Verified by `tests/external/binary_param_diff.py` (arrays with
   NULL/empty, int4/int8 ranges incl. the untyped empty range, multiranges)
   diffing against real PostgreSQL. The extended-protocol COPY flow is refused
   fully streams COPY IN and OUT through Parse/Bind/Execute too, including
   PostgreSQL's CopyFail and Sync recovery. COPY FROM now also shares INSERT's
   expression/sequence-default and generated-column semantics. The psql
   introspection half of the catalog milestone is complete: detailed
   table/view/materialized-view/index/sequence/domain/type displays and the
   standard relation/schema/database/role/function/tablespace/publication/FDW
   listings execute end-to-end. PostgreSQL 18.4 plain dumps and ownerful custom
   archives restore into pos3ql, including parallel clean replacement, and
   survive restart. Outbound PostgreSQL 18.4 pg_dump now completes under its
   real repeatable-read/read-only/ACCESS SHARE workflow and restores into
   vanilla PostgreSQL with data, a dependent view, identity metadata, and
   sequence continuation. Remaining tooling work is breadth across additional
   object kinds, not a consistency shortcut.

   **Administrative ownership/default-privilege batch (2026-07-31):**
   `ALTER DEFAULT PRIVILEGES`, `SET SESSION AUTHORIZATION`, `REASSIGN OWNED`,
   and `DROP OWNED` are complete and differential-tested against PostgreSQL
   18.4. Default ACLs are transactional fixed-capacity catalog state, visible
   through `pg_default_acl`, applied to tables/views/materialized views,
   sequences, schemas, domains, and enums, and durable through both WAL and the
   manifest. Ownership transitions rewrite ACL grantee/grantor identity
   atomically and merge collisions; DROP OWNED follows stored-query dependency
   closure under RESTRICT/CASCADE. The cold-start regression creates an object
   from a recovered default template after deleting both local cache tiers.
   The authoritative copy therefore remains behind the provider-neutral object
   store contract; RAM and local disk remain bounded, disposable caches, and
   none of these SQL/catalog paths knows whether the implementation is S3,
   MinIO, Google Cloud Storage, Azure Blob Storage, or another provider.
   `tests/postgresql18_commands.tsv` records all four commands as complete.

6. **Logical replication** — publisher first, subscriber second.
7. **Stage I — object-storage-adaptive execution** — cost model,
   batched/hedged I/O scheduler, vectorized scan path, late materialization;
   after 2–4 because it optimizes the read path those steps finalize.
   **Status (2026-07-30): external ORDER/DISTINCT execution landed on the
   wide-executor groundwork.** Range-table
   state and scan scratch now scale with the actual plan rather than the static
   envelope; projected rows have a shared direct writer and a two-byte width,
   removing the hidden 255-value failure in wide deferred projections; and
   spill-read contexts remain a startup-bounded pool behind the existing
   provider-neutral block stack. A recycling physical-row scan now feeds a
   startup-bounded stable external sorter; its immutable SST runs and fixed
   eight-way merge use the same block stack. Durable top-level ORDER BY,
   DISTINCT, and DISTINCT ON therefore exceed the statement arena without
   making local disk authoritative. Remaining Stage I work is the real
   batch/vector expression path, a fixed in-flight/hedged GET scheduler, late
   materialization, and extending the run pipeline through grouped aggregates,
   set operations, windows, and nested row sources. Object storage remains
   their remote backing in durable mode, while RAM and local disk are
   disposable cache tiers. No executor code may branch on S3, MinIO, Google
   Cloud Storage, Azure Blob Storage, or any other provider identity.
8. **VSR productionization** — live write-routing, quorum ordering, failover,
   and group commit. It never changes the durability root: an acknowledgment
   follows the object-store WAL PUT, while replica journals and disks remain
   caches/recovery accelerators.

## Deviations from the original plan (deliberate, revisitable)

- **Snapshot checkpoints instead of a leveled LSM** — *superseded.* Stages
  A–E replaced the full-rewrite checkpoint with content-addressed block SSTs,
  row-byte spilling under memory pressure, delta flushes with tombstones, and
  paced level-aware pair merges. The row map is now a working-set overlay,
  and the in-RAM value map is a bounded cache backed by manifest-published
  key-only generations. Equality/uniqueness and range probes no longer require
  an object-scale row walk when that cache is incomplete.
- **Checkpoint object-store calls are synchronous** — *superseded by the sliced
  checkpoint* (maturity-roadmap step 2): the auto-checkpoint runs one
  table's write per beat, beats interleaved with statements and driven by
  the idle event loop, publishing only when no table changed since its
  slice. The remaining stall is one table's slice (per-block beats are
  Stage E's pacing); the explicit `CHECKPOINT` statement stays atomic by
  design. WAL-segment upload is synchronous and required with object storage
  on (commit-durable-on-bucket); configurations that would acknowledge against
  local disk alone are rejected.

## Verification

- `cargo test` — 540 unit/property tests plus the integration suites
  (memory guard incl. unwind safety and the TLS budget scope, differential
  FixedMap vs std, PCG32/CRC-32C/SHA-256/SHA-512/HMAC/SigV4 official vectors,
  row codec fuzz-by-truncation, WAL corruption/floor/stale-tail, engine
  restart persistence, protocol framing, block/SST/bloom/cache stores, an
  in-process rustls round trip, the sqlstate gate).
- SQLLogicTest differential against PostgreSQL 18.4: 10,911 vendored blocks,
  10,911 exact matches, zero unsupported, and zero divergences. The CI shards
  keep a zero unsupported/divergence ratchet.
- `POS3QL_MINIO_ENDPOINT=... cargo test --test minio_it` — S3 client CAS/range
  /list + engine checkpoint/cold-start integration against real MinIO.
- `POS3QL_RUN_GROUPS=dur tests/external/run.sh` — 19 real-MinIO durability
  assertions, including immediate kill after acknowledgment, local-disk wipe
  and WAL-only rebuild, and manifest cold start. All green as of 2026-07-30.
- `tests/external/run.sh` — the external conformance suite (16 scenario
  steps): psql 18.4 golden files, protocol 3.0/3.2, raw wire probes,
  psycopg 3, kill -9 recovery, object-WAL rebuild, cold start from bucket,
  spill beyond `memtable_bytes`, crash torture vs real PostgreSQL, the TLS
  durability cycle, and the forced-spill differential (the whole suite over a
  256 KiB memtable on MinIO). All green as of 2026-07-24.
- `tests/external/differential.sh` — 80 curated corpora + 3 exact-error
  corpora + binary COPY + the vendored sqllogictest replay against real
  PostgreSQL 18, plus the generative fuzzer. SQLLogicTest files reclaim their
  objects between files,
  unsupported classification is message-qualified rather than a broad
  SQLSTATE allowlist, and a one-way budget makes every newly supported block
  permanent. Divergences include bounded error text, and the fuzzer groups
  unsupported results by SQLSTATE plus concrete message with a reproducible
  seed/statement/SQL example instead of hiding distinct gaps under one state.
  CI runs the deterministic
  wire/tooling/corpus gates once, four query-balanced sqllogictest slices, and
  the full zero-budget fuzzer as separate jobs against a hermetic PostgreSQL
  service, keeping every phase within the fixed 15-minute ceiling.
- `cargo clippy --lib --bins --tests -- -D warnings` — zero warnings.
- `tools/coverage.sh` — line coverage across both test layers (~78–80%,
  CI floor 70%).
- **No-op guard** (`tools/check-noops.sh`, gated by `cargo test` and CI): fails
  on any silent accept-and-ignore of SQL/protocol semantics, so a gap is
  implemented or rejected loudly, never quietly skipped. The initial debt
  (B-019 SET/GUCs, B-020 varchar/numeric, B-021 PREPARE types) is fully burned
  down; the ratchet budget is 0.
- Post-freeze allocation is enforced at runtime: the guard aborted on a real
  bug (ToSocketAddrs allocating in the checkpoint path) during development,
  which is exactly its job.
