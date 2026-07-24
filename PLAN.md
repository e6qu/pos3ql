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
| P7 | Object storage is the database | CHECKPOINT + auto-checkpoint snapshot SSTs + CAS'd manifest; cold start from a wiped disk; WAL truncation; heap compaction; S3 GC | **done** (snapshot model — see "Deviations") |
| P8 | External conformance suite | psql 18 golden tests (dialect, SQLSTATEs, extended), raw wire probes, psycopg, durability + cold-start scenarios; 12/12 pass | **done** |
| P9 | Transactions | BEGIN/COMMIT/ROLLBACK, READ COMMITTED, per-row pending/committed with fail-fast 40001 conflicts, WAL-batch-at-commit, transactional DDL | **done** |
| P10 | VSR multi-replica | Sans-io Replica state machine (normal op + view change), TCP transport (`vsr::cluster`), wire codec; 3-node cluster replicates and fails over | **done** (live psql write-routing into the cluster is the remaining productionization step) |
| P11 | VOPR hardening | Deterministic whole-cluster simulator (`sim`) with loss/reorder/dup/delay/crash/partition; found and fixed two real consensus bugs (B-009, B-010); reproducible from a seed | **done** |
| P12 | Compatibility polish | SCRAM-SHA-256 + cleartext auth, GROUP BY/HAVING/joins/subqueries, `pg_catalog` + `information_schema`, binary result format, portal max_rows, NOTICE, more types (date/timestamp/uuid/bytea), differential suite vs real PostgreSQL 18. | **done** (the TLS decision, deferred here, was later resolved in Stage G: isolated rustls to the object store) |
| P13 | Full PostgreSQL fidelity | Strict differential/sqllogictest fidelity (no papering over gaps): arbitrary-precision **NUMERIC** (base-10000, PG numeric.c representation, exact division scale), plan-time semantic type analysis (42883 before scanning), **correlated subqueries + EXISTS/NOT EXISTS** (scalar/IN/EXISTS, streaming + materialized paths), **subqueries in FROM-less SELECT** and `SELECT *` single-column subqueries, **INSERT ... SELECT** (materialize-then-insert, self-insert safe), exact **`x IN (subquery)`** empty/all-NULL/operand-type semantics, and **DISTINCT aggregates** (`count/sum/avg/min/max/string_agg(DISTINCT ...)`), **`string_agg`**, **date arithmetic** (`date ± int`, `date - date`), **derived tables** (`FROM (SELECT ...) alias`, materialized; compose with WHERE/aggregates/ORDER BY/joins), **non-recursive CTEs** (`WITH`, expanded into derived tables), **GROUP BY/aggregates as a row source** (derived tables, CTEs, set-op branches, and INSERT ... SELECT over grouped queries), **aggregate ORDER BY** (`string_agg(x ORDER BY k)`), **durable CREATE VIEW/DROP VIEW** (registry + WAL + manifest; expanded as a derived table at query time; columns validated at creation), and **durable CREATE INDEX/DROP INDEX** (catalog + WAL + manifest, composite UNIQUE enforcement via 23505; no query acceleration; DROP TABLE cascades to indexes), and **DML on auto-updatable views** (rewritten onto the base table). sqllogictest replay (3205 blocks): match=3205, divergence=0, 0 unsupported (the former 2 bit-string-literal blocks now match since bit-string types landed — B-061). Large ORDER BY / DISTINCT / GROUP BY results materialize in a shared `work_mem`-analogue arena (`work_arena_bytes`, default 64 MiB — larger than PostgreSQL's 4 MiB default), reset per statement; by design this `work_mem` is a hard bound (a result exceeding it errors 54000 rather than spilling to temporary files — B-006, accepted). No known SQL-surface fidelity gaps remain. | **done** |

| P14 | Client/tooling & datatype fidelity | Driver- and tool-facing fidelity from a fresh audit of the wire/SQL/CLI/SDK surface: accept the common session GUCs real drivers set (`extra_float_digits`, `client_min_messages`, `bytea_output`, `lock_timeout`, `row_security`, zero-valued `statement_timeout`/`idle_*`), casts with type modifiers (`x::varchar(10)`, `CAST(x AS numeric(8,2))` — truncation applied), `SET`/`SHOW TRANSACTION ISOLATION LEVEL` and `SHOW ALL`, distinct **smallint/real/varchar/char** types (own OIDs/names/typmod, `varchar(n)`/`char(n)` length enforcement, 22003/22P02 surfaced), **SERIAL/bigserial/smallserial** (max-based auto-increment), **INSERT ... ON CONFLICT** (`DO NOTHING`/`DO UPDATE`, `excluded.*`), `JOIN ... USING`, and **named/DST time zones** (`SET timezone='America/New_York'` etc.; per-timestamp offset+abbrev via POSIX DST rules; ~25 IANA zones + `Etc/GMT±n` + bare numeric offsets; JDBC/psql now connect and introspect from any zone), and **table constraints** (multi-column PRIMARY KEY / UNIQUE, CHECK, and FOREIGN KEY — durable in the table catalog, enforced on INSERT/UPDATE/DELETE with PG-matching SQLSTATEs; parent-side NO ACTION/RESTRICT, with CASCADE/SET-actions rejected loudly pending a follow-up — see B-029). and **join/DML breadth** (RIGHT and FULL OUTER JOIN; `UPDATE ... FROM` and `DELETE ... USING`; NATURAL JOIN and multi-join RIGHT/FULL rejected loudly pending follow-up — see B-030). and **subtransactions** (SAVEPOINT / RELEASE / ROLLBACK TO SAVEPOINT — the transaction undo log records every row write with its prior image so nested rollback is byte-exact vs PostgreSQL; see B-031). and **window functions** (row_number/rank/dense_rank, lag/lead, and aggregate windows with PARTITION BY / ORDER BY and the default frame — running-with-peers or whole-partition; see B-032). and the **`time`** and **`interval`** types (time-of-day; and interval with months/days/micros fields, verbose parse, PG-exact output, and date/timestamp/interval arithmetic with calendar-month clamping — B-034). and **`json`/`jsonb`** (json verbatim; jsonb parsed and canonicalized — sorted/deduped keys, canonical numbers; `->`/`->>` accessors — B-034). and **one-dimensional arrays** (`ARRAY[...]`/`'{...}'::elem[]`, subscripting, `= ANY/ALL`, `array_length`/`cardinality`, element-wise ordering — B-034, done). and **range types** (`int4range`/`int8range`/`numrange`/`daterange`/`tsrange`/`tstzrange` — canonical-text `Datum::Range`, constructors, text cast, `lower`/`upper`/`isempty`/`lower_inc`/`upper_inc`, the `@>`/`<@`/`&&` predicate operators, value-based comparison/ordering `= <> < <= > >=` including `ORDER BY`/`GROUP BY`/`DISTINCT`, the set operators `* + -`, the positional predicates `<< >> &< &> -|-`, and the functions `range_merge`/`lower_inf`/`upper_inf`; storage/WAL/wire I/O; B-047, done). and **interval/symbolic-date arithmetic** (`interval * / number` with PostgreSQL's fractional-month/day spill, `justify_hours`/`justify_days`/`justify_interval`, and `age(a[, b])` — the symbolic calendar interval; B-049, done). and **FROM-item column-alias lists** (`(subquery) AS v(c1, c2, …)` and `func(args) AS g(c)`, so `VALUES (…) AS v(cols)` works in `FROM`; B-050, done). and **aggregate `FILTER (WHERE …)`** (plain/grouped/DISTINCT/windowed; B-051, done). and **value/positional window functions, `AT TIME ZONE`, `SIMILAR TO`, `LIKE ANY/ALL`, ordered-set aggregates (`percentile_cont`/`percentile_disc`/`mode`), `make_interval` with named arguments, and `GROUPING SETS`/`ROLLUP`/`CUBE` + `GROUPING()`** (B-053..B-060, done). and **bit-string types** (`bit(n)`/`varbit`, `B'…'`/`X'…'`, operators/casts — B-061, done). and **multirange types** (B-062, done). and **`regexp_matches`** (capture-group tracking in the regex engine, SRF in SELECT and FROM — B-063, done). and **`WITH RECURSIVE`** (fixpoint materialization), **full regex quantifiers** (bounded `{m,n}`, non-greedy, PostgreSQL length-preference), **`regexp_replace` backreferences `\1`–`\9`**, and **`string_agg(DISTINCT x … ORDER BY x)`** (B-064, done). and the **B-066 fidelity sweep** (transactional view/index DDL visibility — B-016; NATURAL JOIN + real USING merge semantics; qualified star `t.*`; ORDER BY ordinals through stars; WITH before set operations + views in set-op leaves; GROUP BY/HAVING/DISTINCT in subqueries and EXISTS; DISTINCT over grouped output; FROM-less aggregates; WHERE-level correlated subqueries in grouped/window queries; ANY/ALL/SOME over subqueries; the full to_char numeric code set; fractional-second typmods; FK referential actions CASCADE/SET NULL/SET DEFAULT; explicit window frames ROWS/RANGE/GROUPS; window functions over grouped queries and with DISTINCT — residual loud-error gaps tracked as B-067, closed by the **B-068 sweep**: RIGHT/FULL JOIN in any chain position, set-returning functions composed with aggregates/grouping/DISTINCT/ORDER BY/LIMIT, frame EXCLUDE, windows over GROUPING SETS, `count(t.*)`, correlated subqueries in every clause position of grouped queries (including inner queries that group over the outer reference), parenthesized set-op branches with ORDER BY/LIMIT, and `bytea_output = escape` — remaining loud-error gaps in B-069, then closed by the **B-070 sweep**: records as first-class values (`t.*`, `ROW(...)`, `row_to_json`/`to_jsonb`, record comparison), correlated subqueries in window select lists / ORDER BY / derived-table columns, `DISTINCT ON`, `array_agg`, and the array set operators `@> <@ &&`, plus latent fixes to UNION ALL ordering and untyped-NULL set/VALUES type unification, then the **B-072 sweep**: the single-column JSON set-returning functions (`json_object_keys`/`jsonb_object_keys`, `json_array_elements`/`jsonb_array_elements`, `json_array_elements_text`/`jsonb_array_elements_text`) in both the select list and the FROM clause — the `json` variants preserving the input's key order, duplicates, and whitespace, the `jsonb` variants normalized; jsonb object-key ordering corrected to length-then-bytewise (PostgreSQL's storage order); JSON text-escape decoding in `->>`/`#>>`/`*_text`; scalar function-scan whole rows (`SELECT x FROM json_array_elements(j) x` yields the scalar, not a one-field record); and a silent output-truncation bug class — fixed-size 40-/64-/256-byte render buffers that silently cut long json/array/range/record values on output and `::text` casts, replaced with unbounded `Arena::alloc_str_display` and length-counted Display streaming — remaining unimplemented features in B-071), then the **B-073 sweep**: the two-column `json_each`/`jsonb_each`/`json_each_text`/`jsonb_each_text` family (FROM-clause table functions with a two-column `(key, value)` `TableDef` and positional aliases; a `(key, value)` record per member in the select list), the `KEY` keyword corrected to non-reserved (usable as a column name, as in PostgreSQL), and the `json_to_text` jsonb re-serializer switched off a fixed 16 KiB buffer onto unbounded arena rendering (same silent-truncation class as B-072) — remaining unimplemented features (schemas, the full IANA time-zone database, and general composite-value field access / expansion `(record).field` and `(record).*`) in B-071), then the **B-074 sweep**: composite-value field access `(record).field` (resolving a field of a `ROW(...)`, a table whole row, or a `json_each` record — with static field-type inference via a new `table_columns` resolver hook) and expansion `(record).*` (a new `SelectItem::RecordStar` wired through every projection/describe/count/FROM-less path and composing with set-returning functions, so `SELECT (json_each(j)).*` works), plus PostgreSQL's exact `XX000` error for selecting a field of a `ROW(...)` containing a bare unknown literal — leaving field access on a scalar-subquery record and record-typed derived-table columns as loud-error follow-ups, and a fuzzer-found projection-postponement/div-by-zero cost-model divergence tracked as B-075, then the **B-076 sweep**: the array-manipulation function family (`array_append`/`array_prepend`/`array_cat`/`array_remove`/`array_replace`/`array_ndims`/`array_dims`/`trim_array`/`array_to_json`) with PostgreSQL's polymorphic element-type promotion (a wider new element widens the whole array), plus two pre-existing array gaps the family exposed — array-to-array casts with a different element type (`ARRAY[1,2]::int8[]`) and `pg_typeof` reporting the real `integer[]`/`numeric[]` name instead of a bare `array`, then the **B-077 sweep**: the regular-expression string-function family — the regex forms of `substring` (`substring(str FROM posix_pattern)` and the SQL-regex `substring(str FROM sql_pattern FOR escape)` with `#"..."#` capture extraction), `regexp_like`, and `regexp_split_to_array` (`regexp_substr`/`count`/`instr` were already present), then the **B-078 sweep**: `regexp_split_to_table` and `generate_subscripts` (as both select-list SRFs and FROM-clause table functions) and `WITH ORDINALITY` on any table function (appends a 1-based bigint column, composing with the multi-column `json_each` family and positional column aliases), then the **B-079 sweep**: temporal `generate_series` (over timestamp/timestamptz/date, iterated by calendar addition, in both SRF and FROM positions and with `WITH ORDINALITY`), `date_bin`, and the scalar temporal functions `make_timestamptz`, `clock_timestamp`, and `isfinite`, then the **B-080 sweep**: multiple set-returning functions in one select list now expand in lockstep to the longest (shorter ones NULL-pad), and a bare-string-literal `ARRAY[...]` now infers `text[]` instead of `int4[]` at describe time, then the **B-081 sweep**: the encoding/hashing/bytea/quoting function family (`encode`/`decode` base64·hex·escape, `sha224`/`sha256`/`sha384`/`sha512` with a new FIPS-validated SHA-512, `get`/`set_byte`/`_bit`, `bit_count`, `convert_to`/`from`, `length(bytea)`, `quote_ident`/`literal`/`nullable`, `parse_ident`), the `OVERLAPS` period operator, and a root-cause fix letting `bytea` input accept the `escape` text form (`'abc'::bytea`) not just `\x` hex, then the **B-082 sweep**: the jsonb manipulation family (`jsonb_set`, `jsonb_insert`, `jsonb_strip_nulls`, `jsonb_pretty`, and the `-` / `#-` delete operators, with `||` coercing an unknown text literal to jsonb) plus a fuzzer-found `to_char(float8)` overflow fractional fix, then the **B-083 sweep**: the statistical-aggregate family (`var_pop`/`var_samp`/`variance`/`stddev_pop`/`stddev_samp`/`stddev` and the two-argument `corr`/`covar_pop`/`covar_samp`/`regr_*`, all also usable as window functions), with variance/stddev over integer/numeric inputs returning an **exact numeric** result via a new `numeric::var_stddev` mirroring PostgreSQL's `numeric_stddev_internal` (float8 inputs and the two-argument family fold in f64), the `percent_rank`/`cume_dist` window functions, `pg_size_pretty(bigint|numeric)`, the array form of `width_bucket(operand, thresholds[])`, and an incidental `pg_typeof`-of-NULL fix (it now recovers the argument's static type instead of reporting `unknown`, including aggregates over an empty group via a schema-only projection lookup), then the **B-084 sweep**: `EXTRACT`/`date_part` on intervals (PostgreSQL's `interval2tm` field decomposition, with `epoch` scaling a year by 365.25 days and a residual month by 30), interval comparison operators (`= <> < <= > >=`, and thereby `ORDER BY`/`GROUP BY`/`DISTINCT`/`min`/`max`, via the canonical `interval_cmp_value` microseconds), the `bit_and`/`bit_or`/`bit_xor` aggregates over integers and bit strings (also as window functions), and the scalars `num_nonnulls`/`num_nulls`, `array_fill`, `array_positions`, and the `isoyear` field of `EXTRACT`, then the **B-085 sweep**: implicit row constructors `(a, b, …)` (previously only an `OVERLAPS` period pair) parsed as `ROW(...)` outside an `OVERLAPS`, with PostgreSQL's three-valued, short-circuiting **row comparison** (`= <> < <= > >=`, NULL-propagating, distinct from the total order `ORDER BY` uses) and the **row null-test** (`(...) IS NULL` iff every field is null, `IS NOT NULL` iff every field is non-null), plus `substring(str FOR len)`. then the **B-086 sweep**: **named window definitions** (`WINDOW w AS (...)` with `OVER name`, the parenthesized copy form inheriting PARTITION BY and adding a missing ORDER BY, definitions referencing earlier ones, and PostgreSQL's 42704/42P20 restrictions — resolved entirely in the parser by a bounded lookahead, so the AST and executor see only inlined specs; `window` is now reserved except after `AS`), together with four pre-existing window bugs its verification exposed: window functions in a derived table / CTE / set-operation leaf were counted as plain aggregates and took the grouped path (wrong results, e.g. a `UNION` leaf returning the aggregate instead of the windowed rows) — fixed by distinguishing an aggregate *use* (no `OVER`) from an aggregate *name* via a new `Expr::is_aggregate_use`; a window function only in `ORDER BY` was never dispatched to the window path; a window function in a scalar/IN/EXISTS subquery was not routed to the row-source executor; and a correlated subquery whose body computes a window could not resolve its outer row (previously masked, returning a silently wrong value), plus an incomplete `Chained` column lookup that implemented four of `ColumnLookup`'s five methods and so rendered a single-column table function as a record. Remaining: full psql `\d <table>` (\dt works; \d table needs more pg_class/pg_attribute — B-033), and then the **B-088 sweep**: keyword classification — one flat `is_reserved` list had been standing in for PostgreSQL's four keyword categories, so identifiers were both over-restricted (`insert`, `values`, `set` rejected as column names, `insert` wrongly quoted by `quote_ident`) and under-restricted (`all`, `array`, `null`, `authorization` accepted as column names, and not quoted); replaced with a `keyword_category` table generated from `pg_get_keywords()` and applied at the 30 `ColId` positions, leaving the positions PostgreSQL keeps permissive (`t.col` and select-list aliases are `ColLabel`) alone — verified by sweeping all 494 keywords through `CREATE TABLE t(<kw> int)` against real PostgreSQL (same 101 rejected, same 393 accepted) and all 494 through `quote_ident` (exact match), closing B-087. then the **B-090 sweep**: `CREATE TABLE t (LIKE source [INCLUDING ...])` — columns spliced in at the position the element was written (so `(z int, LIKE src, w text)` keeps PostgreSQL's order), always carrying name/type/NOT NULL, with `DEFAULTS`, `CONSTRAINTS` (CHECK), `INDEXES` (PRIMARY KEY, UNIQUE and secondary indexes), `IDENTITY`/`GENERATED` and `ALL` each adding a group and `EXCLUDING` removing one; foreign keys are never copied and copied constraint/index names are regenerated from the new table, both as PostgreSQL does; the four options describing properties this engine does not model are rejected with 0A000 rather than silently dropped. Closes B-089, and with it all 494 PostgreSQL keywords now behave identically in a `ColId` position, then the **B-091 fix**: the projection-postponement cost model ignored the implicit casts `GREATEST`/`LEAST`/`COALESCE` place on their arguments, so near PostgreSQL's 10-operator threshold pos3ql decided the opposite way and surfaced a division-by-zero or overflow for a row that sorts past the LIMIT and that PostgreSQL never evaluates; the per-operator costs were read straight out of `EXPLAIN` rather than guessed, closing B-075 and bringing the fuzzer to its first fully clean 40-seed sweep (16,000 statements, zero divergences), then the **B-094 fix**: fractional-second precision above 6 was clamped silently because the parser had no channel to the responder — it now records parse-time warnings that the engine drains and emits before the statement runs, so `timestamp(7)` and its siblings report PostgreSQL's `precision reduced to maximum allowed, 6` (closing the last open B-071 item). then the **B-095 fix**: a column's one-byte stored type code was ambiguous between the multirange and array families, so an `int4[]` or `bool[]` column replayed from the journal (or the checkpoint) came back as a multirange with its values gone — silent data loss on any restart; the families are rebased clear of each other and of every retired code, so older data fails loudly instead, guarded by a round-trip/collision unit test and a durability check that both fail on the parent commit. Remaining: schemas and the full IANA time-zone database (B-071), the repeated DDL warning PostgreSQL emits twice (B-093), then the **B-096 sweep**: the `timetz` type (instant-then-zone ordering as `timetz_cmp`, session-zone resolution for a zoneless source, casts, `± interval`, `extract`, typmod, storage/wire round-trip), which uncovered three pre-existing bugs — a **server crash** on `SELECT DISTINCT` over a time, interval, json, range, multirange, bit string, uuid or numeric (the sort path kept its own stale copy of the projected encoding's tag table and hit `unreachable!()`), `'12:00:00-05'::time` rejected because only `+`/`Z` suffixes were stripped, and `WHERE time_col > 'literal'` failing for want of a `coerce_unknown` arm, then **B-097**: the parenthesis-less SQL-standard functions (`current_date`, `current_timestamp`, `current_user`, …) had become syntax errors — a regression B-088 shipped, since every one of them is a reserved word and the new reserved-word test ran before the list that recognizes them, with no corpus probe naming any of them to catch it — fixed by ordering the test last, and completed with `current_time`/`localtime`, the optional precision argument, and a session-zone-aware `localtimestamp`, then the **B-104 surface sweep**: after that regression shipped unnoticed, the expression and statement surface was enumerated from the routers and compared form by form against PostgreSQL, fixing the bare `user` keyword, `LIMIT ALL`, `INSERT ... DEFAULT VALUES`, `POSITION`'s output-column label, and `COALESCE`'s result type (it took its first argument's type, so `coalesce(NULL, 1)` described as text) — and keeping the sweep as a fifteenth corpus, `14_surface.sql`, which fails on the commit before it. Gaps it found that need their own work are tracked: `TRUNCATE` (B-098, blocked on a persistent identity high-water mark), the `INTERVAL '1' DAY` qualifier (B-099), window functions in a FROM-less SELECT (B-100), statement-stable `now()` (B-101), the `case` column label on desugared `IS` forms (B-102), and the `name` type (B-103). The `query.rs` split continues: `WITH` expansion, recursive-CTE materialization and the AST substitution they rest on move to `query/cte.rs`, and FROM-clause scope resolution to `query/scope.rs`, and source-row enumeration to `query/scan.rs`, leaving `query/mod.rs` at 5731 lines, down from 10787 across seven extractions (set operations, window functions, aggregates, CTEs, scope resolution, row scanning) — no file in the crate now exceeds 5731 lines. Then **B-105**: the timestamp family all read the wall clock afresh, so two `now()`s in one statement could differ and none meant what PostgreSQL means; they now anchor as PostgreSQL anchors them — `now`/`current_timestamp`/`transaction_timestamp` and the `current_*` family to the transaction, `statement_timestamp` to the statement, only `clock_timestamp` live — closing B-101 and restoring the corpus probe that had to be withdrawn for flaking, and **B-106**: a window function in a FROM-less SELECT is no longer rejected — such a query *is* one row, so it is rewritten to select from a one-row derived table and handed to the ordinary scanning path, which needs neither a synthetic scope nor a second copy of the window family's semantics (closing B-100), then a **type × operation sweep** — every supported type through fifteen operations (cast, equality, ORDER BY, DISTINCT, GROUP BY, min/max, count, UNION, coalesce, CASE, array_agg, IS NULL, nullif, IN, pg_typeof), 434 probes against real PostgreSQL — which found that **`array_agg` was returning integers** for every element type arrays cannot carry (a `.unwrap_or(Int4)` standing in for an unrepresentable value: `array_agg` over a `time` gave `{250327040}`), now a loud 0A000 (B-107, remaining element types B-108), and that an array element needing quotes was written bare unless it was text, so a timestamp array printed a literal PostgreSQL would read back as two elements (B-109). The split continues: row materialization for GROUP BY / DISTINCT / ORDER BY moves to `query/materialize.rs`, leaving `query/mod.rs` at 5307 lines — down from 10787 across eight extractions, and every module in `query/` now under 1300 lines but the root. Then **B-111**: `json` compared equal where PostgreSQL has no operator at all — it declines because two documents differing only in whitespace or key order are the same value but not the same text — so the operator now declines too, `jsonb` still comparing; the `DISTINCT`/`GROUP BY`/`ORDER BY` forms that sort by the encoding rather than the operator remain (B-112), and `min`/`max` over `json`, `jsonb`, bit strings, ranges and multiranges now decline as PostgreSQL does (B-113 — whose first recording also blamed `boolean` and `uuid`, which already worked; the sweep's report had slipped a row against its probe names, and every claim was re-checked one type at a time before the fix). `exec.rs` is now a module directory too: constraint enforcement — uniqueness, NOT NULL, CHECK, and both sides of a foreign key including the referential actions that re-enter DML — moves to `exec/constraints.rs`, leaving `exec/mod.rs` at 4285 lines. Also **B-110**: an array constructor kept its `array` column label through a cast. `sql/mod.rs` follows: its 2632-line inline test module becomes `sql/tests.rs`, leaving the engine itself at 1468 lines — the file was never mostly engine. Then `parser.rs` becomes a directory too, its data-definition statements — `CREATE TABLE` with its constraints and `LIKE` clauses, `CREATE INDEX`, `CREATE VIEW` and the `DROP` family — moving to `parser/ddl.rs` as a second `impl Parser` block; probing those paths afterwards found that a `DROP` reported `relation` where PostgreSQL names the kind, and that `DROP INDEX` raised 42P01 where PostgreSQL raises 42704 (B-114). `eval/mod.rs` follows, its casting machinery — one arm per target plus the parsers the harder ones need (bit strings, uuid, bytea's two input forms) — moving to `eval/cast.rs`; round-tripping every type through a cast afterwards found two gaps, both recorded: a range does not quote a bound carrying a space (B-115, the same rule as the array elements fixed in B-109, but decided when the canonical stored text is built rather than when it prints) and `char(n)` keeps its blank padding through a cast to text (B-116). `exec/mod.rs` gives up its table-definition building too — column metadata, PRIMARY KEY/UNIQUE, CHECK reference validation and FOREIGN KEY resolution — to `exec/ddl.rs`, leaving it at 3788 lines. Across the session the largest file has gone from 10787 to 4444 and nothing exceeds it. `query/mod.rs` then gives up its subquery machinery — the uncorrelated evaluation done once up front, the correlated re-evaluation per outer row, and the scalar/IN/EXISTS/ARRAY forms — to `query/subquery.rs`. Sweeping that surface against PostgreSQL turned up a NULL inside a row being invisible to `IN` (B-117) and row-constructor `IN (subquery)` being rejected outright (B-118), both fixed, plus a golden expectation left stale by an earlier fix (B-119). Grouped execution (`GROUP BY`, grouping sets, `HAVING`) and qualification planning (conjunct order, pushdown, canonicalization) then follow into `query/group.rs` and `query/plan.rs`, leaving `query/mod.rs` at 3147 — down from 10787 where the session started. Sweeping the grouping surface found `GROUP BY <n>` not reading as a select-list position (B-120) and the ungrouped-column error not naming its column (B-121), both fixed, and three more recorded open (B-122, B-123, B-124). A sweep for doc comments stranded by the earlier splits — a moved function's doc silently reattaching to whatever followed it — returned eleven, each moved back to what it describes or dropped where the subject was already documented accurately. `exec/mod.rs` then gives up static type analysis (what a query's columns are before a row exists) to `exec/describe.rs` and the self-describing row encoding to `exec/projected.rs`, leaving it at 2220. Sweeping that surface found a non-boolean being accepted — and returned — wherever a boolean belonged (B-125), fixed, plus two recorded open (B-126, B-127). `parser/mod.rs` then gives up expression parsing (precedence climbing and the prefix forms) to `parser/expr.rs` and the window clause to `parser/window.rs`, leaving it at 2152. Sweeping the expression grammar found `BETWEEN SYMMETRIC` unsupported (B-128), fixed, plus two more recorded open (B-129, B-130). `eval/mod.rs` then gives up scalar-argument reading and text building to `eval/args.rs` and the LIKE / SIMILAR TO / regex family to `eval/pattern.rs`, leaving it at 2555. Sweeping that surface found the `ESCAPE` clause of LIKE and SIMILAR TO unparsed (B-131), fixed, plus two recorded open (B-132, B-133). `query/mod.rs` finally gives up the set-returning functions — the ones written in the select list and the ones written in FROM — to `query/srf.rs`, leaving it at 2298. The sweep of that surface found `string_to_table` missing entirely (B-134), recorded rather than added. No file in the tree now exceeds 2555 lines, against 10787 at the start of the session, and the four files that began it — query, exec, parser, eval — are all now within the same band as the modules extracted from them. With the file sizes settled, the quality gates get the same treatment: dead-code detection, which `lib.rs`'s fictional public API had disabled crate-wide, is turned back on (B-136), and coverage is measured for the first time — across both test layers, since instrumenting only the in-process tests reports 59% and the wire protocol at 6% (B-137). Three open bugs close alongside: `string_to_table` (B-134) and the `|/`, `||/` and `@` prefix operators (B-130). A further batch then closes four more: an untyped literal now takes the type of an array operand it faces (B-129), the desugarings of `SIMILAR TO` and `OVERLAPS` no longer leak into the function router (B-132), a bare row constructor is no longer a field-access target (B-135), and an undefined operator is reported under the operator that was written (B-127). Two entries were re-examined rather than fixed and now say what is actually wrong: `smallint` has no runtime representation at all rather than merely widening under arithmetic (B-126), and undefined-function errors omit their argument types (B-138). Later, range bound quoting (B-115) is fixed at the canonicalization choke point: a bound is normalized to its element type's output form and quoted when that text needs it, so a timestamp range gains its time-of-day and round-trips PostgreSQL's own quoted output — surfacing a separate error-wording gap recorded as B-141. The typmod family of bugs (B-139, B-140, B-116's blocker) is then retired as a
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
| P15 | Differential CI at scale | Wire the existing differential + fuzz machinery into CI as its own workflow (`differential.yml` → `tests/external/ci_diff.sh`): a real PostgreSQL 18 service (C collation, **UTF8** encoding to match pos3ql and vanilla PG) is the oracle; the suite replays the vendored sqllogictest corpus and the generative fuzzer against both engines and diffs rows + SQLSTATEs. Hardened so a pathological query can never wedge CI: **predicate pushdown** removes the O(Nᵏ) multi-way-equi-join blowup that hung the run for 45+ min (B-037, `select5` now seconds, divergence 0), a per-statement `statement_timeout` guard is set where the engine honors it (B-038), and the job carries a hard `timeout-minutes` ceiling. The comparator decodes text-returned-as-bytes losslessly, and both sessions pin `TimeZone='UTC'`, so neither a server-encoding nor a host-timezone quirk can masquerade as a data divergence. The generative fuzzer runs against a freshly-restarted pos3ql (a clean table space, since the corpus fills the bounded catalog) and fails loudly on any setup error; its error-timing/semantic divergences were driven to **zero** (B-039 → B-065: projection postponement, qual-ordering and plan-time-simplification fidelity, correctly-rounded numeric→float8, float8 to_char) — `FUZZ_BUDGET=0`, 9 seeds all clean. CI is deduplicated (one run per ref) and caches the Rust build. | **done** |

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

Remaining for Stage A: the free set / ref-map for the local grid, and
re-expressing the current SST writer/reader in terms of blocks — the half that
touches working durability code, with the cold-start and durability scenarios in
`tests/external/run.sh` as the guardrail.

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
-9 recovery, async-WAL rebuild, checkpointed cold start from a wiped disk —
passes over the block path, and the fidelity suites are untouched by
construction. What remains of Stage C proper: the multi-block index and a sized
filter, both refinements.

### Stage D — memtable flush + the manifest log (continuous ingest)

Kill the "flush not implemented" wall: ingest becomes bounded by flush *rate*, not
RAM *size*. **The core is built: row bytes spill to the bucket and the wall is
gone.** The shape this engine's architecture gave it: the rows *map* (rowid →
state) stays in RAM as the authoritative index — visibility, MVCC and uniqueness
never consult a block — while committed row *bytes* gain a second home
(`RowHome::Heap | Spilled`). The auto-checkpoint at 65% heap already wrote every
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
full count(*) of spilled rows, and the kill -9 / async-WAL / cold-start checks
all green over the spill machinery. Below the pressure threshold nothing spills
and reads stay heap-fast — the fidelity suites are untouched by construction.

**Deviations, stated:** (1) the monolithic text manifest remains (it is
kilobytes at this scale; the append-only manifest log + superblock come with
flush *frequency*, i.e. compaction pressure in Stage E). (2) A full scan of
spilled rows stages each row in the statement work arena — bounded and loud
(`work_arena_bytes`), never wrong; the streaming read path that lifts it is
Stage I's object-storage-adaptive execution. (3) Row *count* stays bounded by
`table_rows` (the RAM map); only row *bytes* spill. **Stage E's first half is
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
statement arena. **Remaining for Stage E:** paced background compaction (the
merge currently rides a checkpoint) and flush-rate-driven manifest logging.
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
What remains of Stage E: the manifest log — low value at today's table
counts, noted for the day manifests are large.

### Stage F — MVCC snapshot reads over object-resident data

Preserve snapshot isolation once the working set spills to the bucket. **This is more
than "wire the existing snapshots to the LSM."** Today MVCC is **txid-based,
single-writer, two-version** — each row is `RowState { committed, pending }`, at most
one committed image plus one uncommitted pending change, visibility by `transaction_id`
(not LSN), with a second concurrent writer failing fast (`40001`); `lsn` is only a
write-sequence / WAL position (`storage/mod.rs`). A long-running reader therefore has
**no historical version to see**, and Stage E's compaction would drop the only version
it still needs. So Stage F grows a **prerequisite**: genuine **multi-version rows keyed
by a commit LSN** (append versions instead of repoint-in-place; a read at snapshot `S`
sees the newest version with `commit_lsn ≤ S`), and compaction must **retain any
version still visible to the oldest live snapshot** (RocksDB sequence numbers /
TigerBeetle per-op timestamps are the model). Then a read merges live + frozen memtable
+ level SSTs through a snapshot-aware merge iterator (point via bloom+index, scans via
streamed blocks with bounded read-ahead), and the executor's scan path
(`sql/query.rs`) is wired to it transparently. **Milestone:** concurrent sessions show
identical SI semantics whether data is in RAM or on the bucket; the *full differential
suite is green with `memtable_bytes` shrunk tiny* — a powerful new forced-spill CI mode.

**Status (2026-07-24): the forced-spill differential mode landed early** — the
single-session half of the milestone needs no MVCC change, so it now runs as a
standing run.sh step: the whole suite (43 corpora, the exact-error corpus, all
3205 sqllogictest blocks) against a pos3ql with a 256 KiB memtable over MinIO,
every query continuously spilling, checkpointing (paced merges included), and
reading back through the cache tiers — green, with the bucket showing hundreds
of content-addressed blocks written during the run. What remains of Stage F is
the real multi-session prerequisite: LSN-keyed row versions and the
snapshot-aware merge read.

**Deferral, stated loudly (2026-07-24, maturity-roadmap step 3):** the
LSN-keyed version model is deferred to land *with Stage I's suspendable row
source*, not because it is hard but because until then it is unobservable:
every read path materializes within its statement (portals fully
materialize, cursors materialize at DECLARE), so no reader can outlive a
statement, the oldest-live-snapshot watermark is always "now", and
compaction can never drop a version any reader still needs. Beat pacing —
the half of step 3 with observable value — did not need it either: merges
read only immutable SSTs. The first construct that lets a reader suspend
(the async row source) is the first that needs a version history, and
building the two together means the version format is designed against its
real consumer.

### Stage G — S3 client hardening & multi-provider reach

Make the client production-shaped and reach real clouds, not just MinIO. Loki abstracts
every backend behind one `ObjectClient`; mirror that with a **provider trait** while
keeping the hand-rolled, static-memory discipline: **TLS** — the one explicitly-deferred
decision, resolved *here* (isolated `rustls` vs. terminating proxy); **multipart upload**
for large SSTs; **streaming (non-buffer-bound) response reads** so a large-block GET is
not capped by a fixed buffer (today's `ResponseTooLarge`); chunked-transfer decoding
(today a hard `Protocol` error); and **provider quirks** behind the trait — GCS
(resumable, XML/JSON auth), Azure Blob (shared-key/SAS signing). **Milestone:** the full
flush/compaction/cold-start pipeline runs against real AWS S3 and GCS over HTTPS —
extend `tests/minio_it.rs` into a provider matrix. **Risk:** TLS is the single
dependency-policy exception; keep it isolated behind the trait so the core stays
`libc`-only.

**Status (2026-07-24): chunked decoding and streaming WAL-segment replay
landed.** Chunked-transfer responses (hex-framed chunks with extensions and
trailers) decode into the bounded response buffer, refusing loudly on
overflow; and WAL-segment replay streams in ranged windows, closing a latent
unrecoverability — a committed batch larger than `s3_response_bytes` uploaded
fine but could never be replayed at cold start. run.sh proves the round trip
with the response buffer shrunk below one batch.

**Status (2026-07-24, later): TLS landed — the deferred decision resolved as
isolated rustls.** rustls (with compiled-in Mozilla roots) is the single
whitelisted dependency exception, and `mem::guard::tls_scope` is its only
door: the client configuration is built pre-freeze, and every runtime call —
handshakes, record I/O, teardown — runs inside a scope whose allocations are
charged against `tls_pool_bytes` and abort loudly past it, so the
static-memory discipline holds everywhere else. `s3_tls = on` turns it on;
`s3_tls_ca_file` adds PEM roots for self-signed endpoints (parsed by a
hand-rolled PEM/base64 reader at startup — no new parsing dependency). Proven
two ways: an in-process rustls server round trip in the unit tests (with a
checked-in certificate whose provenance and the `CA:FALSE` lesson live in
tests/data/README.md), and a run.sh durability cycle against MinIO serving
HTTPS — commit, checkpoint, kill -9, wiped disk, cold start entirely over
TLS. S3 I/O errors now carry the source error's words, so a certificate
rejection names itself instead of flattening to `InvalidData`.

**Scope decision (2026-07-24) — Stage G is done.** The provider trait, the
GCS/Azure dialects, and the real-cloud test matrix are dropped, fixed with the
project owner: **the S3 API is the object-storage interface, and the only
one.** Object storage presents a uniform S3-like surface — AWS S3, MinIO, and
every S3-compatible endpoint (GCS interop mode, Cloudflare R2, Ceph RGW, …)
speak it — so per-provider special cases would be abstraction for its own
sake, and cloud-hosted test runs add cost without adding a behavior MinIO
cannot exercise. MinIO is the conformance target; any S3-compatible endpoint
is reachable by configuration alone. Multipart upload stays deferred until a
producer exists (no current object exceeds a single PUT).

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
The seam is `ObjectClient` (an enum over the real HTTP client and `s3::sim`'s
*virtual bucket*, selected by `s3 = sim`, which the real server binary
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
statement (even ROLLBACK) happened to trigger it. What remains of Stage H:
the virtual *grid disk* (torn-write/bit-rot injection under the WAL and the
disk cache — today the harness scribbles the cache file between
incarnations), and folding the mid-flush/mid-compaction crash invariants
into longer standing runs.

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
RAM — you never pay an S3 round-trip to learn *where* data is, only to fetch it.

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

## Maturity roadmap — what remains, in order (2026-07-24)

A full step-back audit against the founding goal — a mature,
PostgreSQL-compatible engine whose *primary* storage is object storage, with
local disk and memory as **mere caches** — found the SQL/wire-fidelity axis
substantially complete (differential + sqllogictest + fuzzer green, bug ledger
empty but for B-124, unmatchable by design) and the remaining work
concentrated in three structural storage gaps, one compatibility wave, and the
adaptive-execution capstone. This section is the plan of record for all of it.

### Decisions of record (fixed with the project owner)

- **Object-storage interface: the S3 API, and only it.** AWS S3 and MinIO are
  the targets. No provider trait, no per-cloud dialects, no cloud-hosted test
  runs — object storage presents a uniform S3-like interface, MinIO is the
  conformance target, and any S3-compatible endpoint is reachable by
  configuration. (Closed Stage G above.)
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
- **Durability model: commit-durable-on-bucket is the single-node default.**
  Group commit batches WAL records and the commit acknowledges only after the
  segment PUT lands (an S3 PUT is ~10–50 ms, amortized across the group).
  Quorum durability (VSR: a quorum of replica journals fsyncs, bucket upload
  async) becomes the *multi-replica* mode — lower latency, but "disk is a
  cache" then holds of the cluster, not of a node. The two compose; the
  bucket-synchronous mode is what makes a **single** node's disk formally a
  cache.

### The three structural gaps ("disk and RAM are mere caches")

1. **Durability still lives on the local disk.** Commit durability today is
   the local WAL (`F_FULLFSYNC`); segments upload to the bucket
   *asynchronously* (`wal_upload_sync` is an opt-in). A machine lost between
   commit and upload loses acknowledged transactions — the disk is the
   durability tier, not a cache. Fix: the commit-durable-on-bucket mode above.
   After it, wiping the disk at any instant loses nothing acknowledged.
2. **RAM is still authoritative for the row map.** Only row *bytes* spill
   (Stage D); the rowid→state map, visibility, uniqueness checks, and every
   per-row bookkeeping entry live in RAM, bounded by `table_rows` — dataset
   size is RAM-bounded in row *count* rather than bytes. Fix: the map itself
   becomes block-resident — real key-ordered LSM levels where the index of
   record is the SSTs' sparse-index/bloom blocks (already built, Stage C), and
   RAM holds only the memtable plus *cached* blocks. The deepest remaining
   storage change; co-designed with Stage F's LSN-keyed MVCC so the
   row-version format is designed once.
3. **The checkpoint is synchronous.** Its S3 calls stall every connection in
   the single-threaded loop. Fix: checkpoint I/O through the reactor — urgent
   the moment gap 1 puts a PUT on the commit path.

### The compatibility wave (a fresh audit of what real deployments touch)

- **COPY is absent entirely** — no COPY IN/OUT, text/csv/binary. The largest
  single compatibility hole: `pg_dump`/`pg_restore`, bulk loaders, ETL, and
  many ORMs speak COPY. Milestone: *a pg_dump of a pos3ql database restores
  into real PostgreSQL, and vice versa* — which doubles as the forcing
  function for the remaining `pg_catalog` completeness (`\d table`, B-033).
- **Server-side TLS for clients** — the SSLRequest probe is answered `N`
  today. The isolated rustls component and the budgeted `tls_scope` machinery
  exist for the S3 side; pointing the same component at the server socket is
  now a bounded task, not a policy question.
- **ALTER TABLE breadth** — today: ADD/DROP/RENAME COLUMN, RENAME TO, SET
  SCHEMA. Missing: `ALTER COLUMN TYPE / SET NOT NULL / DROP NOT NULL / SET
  DEFAULT / DROP DEFAULT`, `ADD CONSTRAINT`, and kin.
- **EXPLAIN is absent** — humans and tools expect it; it becomes genuinely
  informative once Stage I's cost model exists (the plan it prints should be
  the real one).
- **VACUUM / ANALYZE** — silent no-ops are banned, rightly; the mature move
  gives them real semantics: VACUUM triggers compaction/GC, ANALYZE gathers
  the statistics Stage I's cost model needs anyway.
- **Roles and GRANT** — effectively single-user today; privilege enforcement
  is part of "mature".
- **LISTEN / NOTIFY** — smaller, but common in real applications.

### The order (dependency-driven)

1. **Storage VOPR (Stage H)** — the virtual object store + grid disk with
   PCG-driven fault injection and seeded invariant checks. Deliberately
   *before* the durability surgery, so steps 2–4 are born simulation-tested
   rather than retrofitted. **Done** (see Stage H status above).
2. **Durability to the bucket (gaps 1 + 3)** — group-commit WAL-segment PUT
   on the acknowledge path; asynchronous checkpoint through the reactor.
   **Status (2026-07-24): landed as defaults + the sliced checkpoint.**
   Commit-durable-on-bucket is now the *default* whenever `s3 = on`
   (`wal_upload`/`wal_upload_sync` resolve on unless explicitly set off;
   run.sh proves the posture with an ack → immediate kill -9 → wiped-disk
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
3. **Stage F MVCC + Stage E beat pacing** — LSN-keyed row versions,
   snapshot-aware merge reads, compaction retention above the oldest-snapshot
   watermark, merge work amortized across statements.
   **Status (2026-07-24): beat pacing landed; the MVCC half is deferred to
   ride with Stage I's suspendable row source** — see Stage E's status (the
   merge is now a background job bounded to a few block transfers per beat)
   and Stage F's stated deferral (no reader can observe a version history
   until a reader can suspend; the two land together).
4. **The map spills (gap 2)** — block-resident row index; secondary indexes
   as the LSM forest; block compression and the multi-block index / sized
   filters (the remaining Stage C refinements) ride along since they touch
   the same format.
5. **Compatibility wave** — COPY (+ the pg_dump round-trip milestone),
   server-side TLS, ALTER TABLE breadth, roles/GRANT, EXPLAIN,
   VACUUM/ANALYZE as real operations, LISTEN/NOTIFY.
6. **Logical replication** — publisher first, subscriber second.
7. **Stage I — object-storage-adaptive execution** — cost model,
   batched/hedged I/O scheduler, vectorized scan path, late materialization;
   after 2–4 because it optimizes the read path those steps finalize.
8. **VSR productionization** — live write-routing, the quorum durability
   mode. Last as listed, but it moves to step 2's slot if quorum durability
   is ever preferred over bucket-synchronous as the primary model.

## Deviations from the original plan (deliberate, revisitable)

- **Snapshot checkpoints instead of a leveled LSM** — *superseded.* Stages
  A–E replaced the full-rewrite checkpoint with content-addressed block SSTs,
  row-byte spilling under memory pressure, delta flushes with tombstones, and
  paced level-aware pair merges. What remains RAM-bound is the row *map*
  (visibility, uniqueness, per-row bookkeeping) — gap 2 of the maturity
  roadmap above.
- **Checkpoint S3 calls are synchronous** — *superseded by the sliced
  checkpoint* (maturity-roadmap step 2): the auto-checkpoint runs one
  table's write per beat, beats interleaved with statements and driven by
  the idle event loop, publishing only when no table changed since its
  slice. The remaining stall is one table's slice (per-block beats are
  Stage E's pacing); the explicit `CHECKPOINT` statement stays atomic by
  design. WAL-segment upload is synchronous-by-default with s3 on
  (commit-durable-on-bucket), with `wal_upload_sync = off` as the stated
  asynchronous opt-out (B-008's drain).

## Verification

- `cargo test` — 353 unit/property tests plus the integration suites
  (memory guard incl. unwind safety and the TLS budget scope, differential
  FixedMap vs std, PCG32/CRC-32C/SHA-256/SHA-512/HMAC/SigV4 official vectors,
  row codec fuzz-by-truncation, WAL corruption/floor/stale-tail, engine
  restart persistence, protocol framing, block/SST/bloom/cache stores, an
  in-process rustls round trip, the sqlstate gate).
- `POS3QL_MINIO_ENDPOINT=... cargo test --test minio_it` — S3 client CAS/range
  /list + engine checkpoint/cold-start integration against real MinIO.
- `tests/external/run.sh` — the external conformance suite (16 scenario
  steps): psql 18.4 golden files, protocol 3.0/3.2, raw wire probes,
  psycopg 3, kill -9 recovery, async-WAL rebuild, cold start from bucket,
  spill beyond `memtable_bytes`, crash torture vs real PostgreSQL, the TLS
  durability cycle, and the forced-spill differential (the whole suite over a
  256 KiB memtable on MinIO). All green as of 2026-07-24.
- `tests/external/differential.sh` — 42 corpora + the exact-error corpus +
  3205 sqllogictest blocks against real PostgreSQL 18.4, plus the generative
  fuzzer; also run sharded in CI with a hermetic PostgreSQL service.
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
