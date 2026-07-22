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
| P12 | Compatibility polish | SCRAM-SHA-256 + cleartext auth, GROUP BY/HAVING/joins/subqueries, `pg_catalog` + `information_schema`, binary result format, portal max_rows, NOTICE, more types (date/timestamp/uuid/bytea), differential suite vs real PostgreSQL 18. TLS decision still deferred (dev targets MinIO plaintext). | **done** (TLS decision still open) |
| P13 | Full PostgreSQL fidelity | Strict differential/sqllogictest fidelity (no papering over gaps): arbitrary-precision **NUMERIC** (base-10000, PG numeric.c representation, exact division scale), plan-time semantic type analysis (42883 before scanning), **correlated subqueries + EXISTS/NOT EXISTS** (scalar/IN/EXISTS, streaming + materialized paths), **subqueries in FROM-less SELECT** and `SELECT *` single-column subqueries, **INSERT ... SELECT** (materialize-then-insert, self-insert safe), exact **`x IN (subquery)`** empty/all-NULL/operand-type semantics, and **DISTINCT aggregates** (`count/sum/avg/min/max/string_agg(DISTINCT ...)`), **`string_agg`**, **date arithmetic** (`date ± int`, `date - date`), **derived tables** (`FROM (SELECT ...) alias`, materialized; compose with WHERE/aggregates/ORDER BY/joins), **non-recursive CTEs** (`WITH`, expanded into derived tables), **GROUP BY/aggregates as a row source** (derived tables, CTEs, set-op branches, and INSERT ... SELECT over grouped queries), **aggregate ORDER BY** (`string_agg(x ORDER BY k)`), **durable CREATE VIEW/DROP VIEW** (registry + WAL + manifest; expanded as a derived table at query time; columns validated at creation), and **durable CREATE INDEX/DROP INDEX** (catalog + WAL + manifest, composite UNIQUE enforcement via 23505; no query acceleration; DROP TABLE cascades to indexes), and **DML on auto-updatable views** (rewritten onto the base table). sqllogictest replay (3205 blocks): match=3205, divergence=0, 0 unsupported (the former 2 bit-string-literal blocks now match since bit-string types landed — B-061). Large ORDER BY / DISTINCT / GROUP BY results materialize in a shared `work_mem`-analogue arena (`work_arena_bytes`, default 64 MiB — larger than PostgreSQL's 4 MiB default), reset per statement; by design this `work_mem` is a hard bound (a result exceeding it errors 54000 rather than spilling to temporary files — B-006, accepted). No known SQL-surface fidelity gaps remain. | **done** |

| P14 | Client/tooling & datatype fidelity | Driver- and tool-facing fidelity from a fresh audit of the wire/SQL/CLI/SDK surface: accept the common session GUCs real drivers set (`extra_float_digits`, `client_min_messages`, `bytea_output`, `lock_timeout`, `row_security`, zero-valued `statement_timeout`/`idle_*`), casts with type modifiers (`x::varchar(10)`, `CAST(x AS numeric(8,2))` — truncation applied), `SET`/`SHOW TRANSACTION ISOLATION LEVEL` and `SHOW ALL`, distinct **smallint/real/varchar/char** types (own OIDs/names/typmod, `varchar(n)`/`char(n)` length enforcement, 22003/22P02 surfaced), **SERIAL/bigserial/smallserial** (max-based auto-increment), **INSERT ... ON CONFLICT** (`DO NOTHING`/`DO UPDATE`, `excluded.*`), `JOIN ... USING`, and **named/DST time zones** (`SET timezone='America/New_York'` etc.; per-timestamp offset+abbrev via POSIX DST rules; ~25 IANA zones + `Etc/GMT±n` + bare numeric offsets; JDBC/psql now connect and introspect from any zone), and **table constraints** (multi-column PRIMARY KEY / UNIQUE, CHECK, and FOREIGN KEY — durable in the table catalog, enforced on INSERT/UPDATE/DELETE with PG-matching SQLSTATEs; parent-side NO ACTION/RESTRICT, with CASCADE/SET-actions rejected loudly pending a follow-up — see B-029). and **join/DML breadth** (RIGHT and FULL OUTER JOIN; `UPDATE ... FROM` and `DELETE ... USING`; NATURAL JOIN and multi-join RIGHT/FULL rejected loudly pending follow-up — see B-030). and **subtransactions** (SAVEPOINT / RELEASE / ROLLBACK TO SAVEPOINT — the transaction undo log records every row write with its prior image so nested rollback is byte-exact vs PostgreSQL; see B-031). and **window functions** (row_number/rank/dense_rank, lag/lead, and aggregate windows with PARTITION BY / ORDER BY and the default frame — running-with-peers or whole-partition; see B-032). and the **`time`** and **`interval`** types (time-of-day; and interval with months/days/micros fields, verbose parse, PG-exact output, and date/timestamp/interval arithmetic with calendar-month clamping — B-034). and **`json`/`jsonb`** (json verbatim; jsonb parsed and canonicalized — sorted/deduped keys, canonical numbers; `->`/`->>` accessors — B-034). and **one-dimensional arrays** (`ARRAY[...]`/`'{...}'::elem[]`, subscripting, `= ANY/ALL`, `array_length`/`cardinality`, element-wise ordering — B-034, done). and **range types** (`int4range`/`int8range`/`numrange`/`daterange`/`tsrange`/`tstzrange` — canonical-text `Datum::Range`, constructors, text cast, `lower`/`upper`/`isempty`/`lower_inc`/`upper_inc`, the `@>`/`<@`/`&&` predicate operators, value-based comparison/ordering `= <> < <= > >=` including `ORDER BY`/`GROUP BY`/`DISTINCT`, the set operators `* + -`, the positional predicates `<< >> &< &> -|-`, and the functions `range_merge`/`lower_inf`/`upper_inf`; storage/WAL/wire I/O; B-047, done). and **interval/symbolic-date arithmetic** (`interval * / number` with PostgreSQL's fractional-month/day spill, `justify_hours`/`justify_days`/`justify_interval`, and `age(a[, b])` — the symbolic calendar interval; B-049, done). and **FROM-item column-alias lists** (`(subquery) AS v(c1, c2, …)` and `func(args) AS g(c)`, so `VALUES (…) AS v(cols)` works in `FROM`; B-050, done). and **aggregate `FILTER (WHERE …)`** (plain/grouped/DISTINCT/windowed; B-051, done). and **value/positional window functions, `AT TIME ZONE`, `SIMILAR TO`, `LIKE ANY/ALL`, ordered-set aggregates (`percentile_cont`/`percentile_disc`/`mode`), `make_interval` with named arguments, and `GROUPING SETS`/`ROLLUP`/`CUBE` + `GROUPING()`** (B-053..B-060, done). and **bit-string types** (`bit(n)`/`varbit`, `B'…'`/`X'…'`, operators/casts — B-061, done). and **multirange types** (B-062, done). and **`regexp_matches`** (capture-group tracking in the regex engine, SRF in SELECT and FROM — B-063, done). and **`WITH RECURSIVE`** (fixpoint materialization), **full regex quantifiers** (bounded `{m,n}`, non-greedy, PostgreSQL length-preference), **`regexp_replace` backreferences `\1`–`\9`**, and **`string_agg(DISTINCT x … ORDER BY x)`** (B-064, done). and the **B-066 fidelity sweep** (transactional view/index DDL visibility — B-016; NATURAL JOIN + real USING merge semantics; qualified star `t.*`; ORDER BY ordinals through stars; WITH before set operations + views in set-op leaves; GROUP BY/HAVING/DISTINCT in subqueries and EXISTS; DISTINCT over grouped output; FROM-less aggregates; WHERE-level correlated subqueries in grouped/window queries; ANY/ALL/SOME over subqueries; the full to_char numeric code set; fractional-second typmods; FK referential actions CASCADE/SET NULL/SET DEFAULT; explicit window frames ROWS/RANGE/GROUPS; window functions over grouped queries and with DISTINCT — residual loud-error gaps tracked as B-067, closed by the **B-068 sweep**: RIGHT/FULL JOIN in any chain position, set-returning functions composed with aggregates/grouping/DISTINCT/ORDER BY/LIMIT, frame EXCLUDE, windows over GROUPING SETS, `count(t.*)`, correlated subqueries in every clause position of grouped queries (including inner queries that group over the outer reference), parenthesized set-op branches with ORDER BY/LIMIT, and `bytea_output = escape` — remaining loud-error gaps in B-069, then closed by the **B-070 sweep**: records as first-class values (`t.*`, `ROW(...)`, `row_to_json`/`to_jsonb`, record comparison), correlated subqueries in window select lists / ORDER BY / derived-table columns, `DISTINCT ON`, `array_agg`, and the array set operators `@> <@ &&`, plus latent fixes to UNION ALL ordering and untyped-NULL set/VALUES type unification, then the **B-072 sweep**: the single-column JSON set-returning functions (`json_object_keys`/`jsonb_object_keys`, `json_array_elements`/`jsonb_array_elements`, `json_array_elements_text`/`jsonb_array_elements_text`) in both the select list and the FROM clause — the `json` variants preserving the input's key order, duplicates, and whitespace, the `jsonb` variants normalized; jsonb object-key ordering corrected to length-then-bytewise (PostgreSQL's storage order); JSON text-escape decoding in `->>`/`#>>`/`*_text`; scalar function-scan whole rows (`SELECT x FROM json_array_elements(j) x` yields the scalar, not a one-field record); and a silent output-truncation bug class — fixed-size 40-/64-/256-byte render buffers that silently cut long json/array/range/record values on output and `::text` casts, replaced with unbounded `Arena::alloc_str_display` and length-counted Display streaming — remaining unimplemented features in B-071), then the **B-073 sweep**: the two-column `json_each`/`jsonb_each`/`json_each_text`/`jsonb_each_text` family (FROM-clause table functions with a two-column `(key, value)` `TableDef` and positional aliases; a `(key, value)` record per member in the select list), the `KEY` keyword corrected to non-reserved (usable as a column name, as in PostgreSQL), and the `json_to_text` jsonb re-serializer switched off a fixed 16 KiB buffer onto unbounded arena rendering (same silent-truncation class as B-072) — remaining unimplemented features (schemas, the full IANA time-zone database, and general composite-value field access / expansion `(record).field` and `(record).*`) in B-071), then the **B-074 sweep**: composite-value field access `(record).field` (resolving a field of a `ROW(...)`, a table whole row, or a `json_each` record — with static field-type inference via a new `table_columns` resolver hook) and expansion `(record).*` (a new `SelectItem::RecordStar` wired through every projection/describe/count/FROM-less path and composing with set-returning functions, so `SELECT (json_each(j)).*` works), plus PostgreSQL's exact `XX000` error for selecting a field of a `ROW(...)` containing a bare unknown literal — leaving field access on a scalar-subquery record and record-typed derived-table columns as loud-error follow-ups, and a fuzzer-found projection-postponement/div-by-zero cost-model divergence tracked as B-075, then the **B-076 sweep**: the array-manipulation function family (`array_append`/`array_prepend`/`array_cat`/`array_remove`/`array_replace`/`array_ndims`/`array_dims`/`trim_array`/`array_to_json`) with PostgreSQL's polymorphic element-type promotion (a wider new element widens the whole array), plus two pre-existing array gaps the family exposed — array-to-array casts with a different element type (`ARRAY[1,2]::int8[]`) and `pg_typeof` reporting the real `integer[]`/`numeric[]` name instead of a bare `array`, then the **B-077 sweep**: the regular-expression string-function family — the regex forms of `substring` (`substring(str FROM posix_pattern)` and the SQL-regex `substring(str FROM sql_pattern FOR escape)` with `#"..."#` capture extraction), `regexp_like`, and `regexp_split_to_array` (`regexp_substr`/`count`/`instr` were already present), then the **B-078 sweep**: `regexp_split_to_table` and `generate_subscripts` (as both select-list SRFs and FROM-clause table functions) and `WITH ORDINALITY` on any table function (appends a 1-based bigint column, composing with the multi-column `json_each` family and positional column aliases), then the **B-079 sweep**: temporal `generate_series` (over timestamp/timestamptz/date, iterated by calendar addition, in both SRF and FROM positions and with `WITH ORDINALITY`), `date_bin`, and the scalar temporal functions `make_timestamptz`, `clock_timestamp`, and `isfinite`, then the **B-080 sweep**: multiple set-returning functions in one select list now expand in lockstep to the longest (shorter ones NULL-pad), and a bare-string-literal `ARRAY[...]` now infers `text[]` instead of `int4[]` at describe time, then the **B-081 sweep**: the encoding/hashing/bytea/quoting function family (`encode`/`decode` base64·hex·escape, `sha224`/`sha256`/`sha384`/`sha512` with a new FIPS-validated SHA-512, `get`/`set_byte`/`_bit`, `bit_count`, `convert_to`/`from`, `length(bytea)`, `quote_ident`/`literal`/`nullable`, `parse_ident`), the `OVERLAPS` period operator, and a root-cause fix letting `bytea` input accept the `escape` text form (`'abc'::bytea`) not just `\x` hex, then the **B-082 sweep**: the jsonb manipulation family (`jsonb_set`, `jsonb_insert`, `jsonb_strip_nulls`, `jsonb_pretty`, and the `-` / `#-` delete operators, with `||` coercing an unknown text literal to jsonb) plus a fuzzer-found `to_char(float8)` overflow fractional fix, then the **B-083 sweep**: the statistical-aggregate family (`var_pop`/`var_samp`/`variance`/`stddev_pop`/`stddev_samp`/`stddev` and the two-argument `corr`/`covar_pop`/`covar_samp`/`regr_*`, all also usable as window functions), with variance/stddev over integer/numeric inputs returning an **exact numeric** result via a new `numeric::var_stddev` mirroring PostgreSQL's `numeric_stddev_internal` (float8 inputs and the two-argument family fold in f64), the `percent_rank`/`cume_dist` window functions, `pg_size_pretty(bigint|numeric)`, the array form of `width_bucket(operand, thresholds[])`, and an incidental `pg_typeof`-of-NULL fix (it now recovers the argument's static type instead of reporting `unknown`, including aggregates over an empty group via a schema-only projection lookup), then the **B-084 sweep**: `EXTRACT`/`date_part` on intervals (PostgreSQL's `interval2tm` field decomposition, with `epoch` scaling a year by 365.25 days and a residual month by 30), interval comparison operators (`= <> < <= > >=`, and thereby `ORDER BY`/`GROUP BY`/`DISTINCT`/`min`/`max`, via the canonical `interval_cmp_value` microseconds), the `bit_and`/`bit_or`/`bit_xor` aggregates over integers and bit strings (also as window functions), and the scalars `num_nonnulls`/`num_nulls`, `array_fill`, `array_positions`, and the `isoyear` field of `EXTRACT`, then the **B-085 sweep**: implicit row constructors `(a, b, …)` (previously only an `OVERLAPS` period pair) parsed as `ROW(...)` outside an `OVERLAPS`, with PostgreSQL's three-valued, short-circuiting **row comparison** (`= <> < <= > >=`, NULL-propagating, distinct from the total order `ORDER BY` uses) and the **row null-test** (`(...) IS NULL` iff every field is null, `IS NOT NULL` iff every field is non-null), plus `substring(str FOR len)`. then the **B-086 sweep**: **named window definitions** (`WINDOW w AS (...)` with `OVER name`, the parenthesized copy form inheriting PARTITION BY and adding a missing ORDER BY, definitions referencing earlier ones, and PostgreSQL's 42704/42P20 restrictions — resolved entirely in the parser by a bounded lookahead, so the AST and executor see only inlined specs; `window` is now reserved except after `AS`), together with four pre-existing window bugs its verification exposed: window functions in a derived table / CTE / set-operation leaf were counted as plain aggregates and took the grouped path (wrong results, e.g. a `UNION` leaf returning the aggregate instead of the windowed rows) — fixed by distinguishing an aggregate *use* (no `OVER`) from an aggregate *name* via a new `Expr::is_aggregate_use`; a window function only in `ORDER BY` was never dispatched to the window path; a window function in a scalar/IN/EXISTS subquery was not routed to the row-source executor; and a correlated subquery whose body computes a window could not resolve its outer row (previously masked, returning a silently wrong value), plus an incomplete `Chained` column lookup that implemented four of `ColumnLookup`'s five methods and so rendered a single-column table function as a record. Remaining: full psql `\d <table>` (\dt works; \d table needs more pg_class/pg_attribute — B-033), and then the **B-088 sweep**: keyword classification — one flat `is_reserved` list had been standing in for PostgreSQL's four keyword categories, so identifiers were both over-restricted (`insert`, `values`, `set` rejected as column names, `insert` wrongly quoted by `quote_ident`) and under-restricted (`all`, `array`, `null`, `authorization` accepted as column names, and not quoted); replaced with a `keyword_category` table generated from `pg_get_keywords()` and applied at the 30 `ColId` positions, leaving the positions PostgreSQL keeps permissive (`t.col` and select-list aliases are `ColLabel`) alone — verified by sweeping all 494 keywords through `CREATE TABLE t(<kw> int)` against real PostgreSQL (same 101 rejected, same 393 accepted) and all 494 through `quote_ident` (exact match), closing B-087. then the **B-090 sweep**: `CREATE TABLE t (LIKE source [INCLUDING ...])` — columns spliced in at the position the element was written (so `(z int, LIKE src, w text)` keeps PostgreSQL's order), always carrying name/type/NOT NULL, with `DEFAULTS`, `CONSTRAINTS` (CHECK), `INDEXES` (PRIMARY KEY, UNIQUE and secondary indexes), `IDENTITY`/`GENERATED` and `ALL` each adding a group and `EXCLUDING` removing one; foreign keys are never copied and copied constraint/index names are regenerated from the new table, both as PostgreSQL does; the four options describing properties this engine does not model are rejected with 0A000 rather than silently dropped. Closes B-089, and with it all 494 PostgreSQL keywords now behave identically in a `ColId` position, then the **B-091 fix**: the projection-postponement cost model ignored the implicit casts `GREATEST`/`LEAST`/`COALESCE` place on their arguments, so near PostgreSQL's 10-operator threshold pos3ql decided the opposite way and surfaced a division-by-zero or overflow for a row that sorts past the LIMIT and that PostgreSQL never evaluates; the per-operator costs were read straight out of `EXPLAIN` rather than guessed, closing B-075 and bringing the fuzzer to its first fully clean 40-seed sweep (16,000 statements, zero divergences), then the **B-094 fix**: fractional-second precision above 6 was clamped silently because the parser had no channel to the responder — it now records parse-time warnings that the engine drains and emits before the statement runs, so `timestamp(7)` and its siblings report PostgreSQL's `precision reduced to maximum allowed, 6` (closing the last open B-071 item). then the **B-095 fix**: a column's one-byte stored type code was ambiguous between the multirange and array families, so an `int4[]` or `bool[]` column replayed from the journal (or the checkpoint) came back as a multirange with its values gone — silent data loss on any restart; the families are rebased clear of each other and of every retired code, so older data fails loudly instead, guarded by a round-trip/collision unit test and a durability check that both fail on the parent commit. Remaining: schemas and the full IANA time-zone database (B-071), the repeated DDL warning PostgreSQL emits twice (B-093), then the **B-096 sweep**: the `timetz` type (instant-then-zone ordering as `timetz_cmp`, session-zone resolution for a zoneless source, casts, `± interval`, `extract`, typmod, storage/wire round-trip), which uncovered three pre-existing bugs — a **server crash** on `SELECT DISTINCT` over a time, interval, json, range, multirange, bit string, uuid or numeric (the sort path kept its own stale copy of the projected encoding's tag table and hit `unreachable!()`), `'12:00:00-05'::time` rejected because only `+`/`Z` suffixes were stripped, and `WHERE time_col > 'literal'` failing for want of a `coerce_unknown` arm, then **B-097**: the parenthesis-less SQL-standard functions (`current_date`, `current_timestamp`, `current_user`, …) had become syntax errors — a regression B-088 shipped, since every one of them is a reserved word and the new reserved-word test ran before the list that recognizes them, with no corpus probe naming any of them to catch it — fixed by ordering the test last, and completed with `current_time`/`localtime`, the optional precision argument, and a session-zone-aware `localtimestamp`, then the **B-104 surface sweep**: after that regression shipped unnoticed, the expression and statement surface was enumerated from the routers and compared form by form against PostgreSQL, fixing the bare `user` keyword, `LIMIT ALL`, `INSERT ... DEFAULT VALUES`, `POSITION`'s output-column label, and `COALESCE`'s result type (it took its first argument's type, so `coalesce(NULL, 1)` described as text) — and keeping the sweep as a fifteenth corpus, `14_surface.sql`, which fails on the commit before it. Gaps it found that need their own work are tracked: `TRUNCATE` (B-098, blocked on a persistent identity high-water mark), the `INTERVAL '1' DAY` qualifier (B-099), window functions in a FROM-less SELECT (B-100), statement-stable `now()` (B-101), the `case` column label on desugared `IS` forms (B-102), and the `name` type (B-103). The `query.rs` split continues: `WITH` expansion, recursive-CTE materialization and the AST substitution they rest on move to `query/cte.rs`, and FROM-clause scope resolution to `query/scope.rs`, and source-row enumeration to `query/scan.rs`, leaving `query/mod.rs` at 5731 lines, down from 10787 across seven extractions (set operations, window functions, aggregates, CTEs, scope resolution, row scanning) — no file in the crate now exceeds 5731 lines. Then **B-105**: the timestamp family all read the wall clock afresh, so two `now()`s in one statement could differ and none meant what PostgreSQL means; they now anchor as PostgreSQL anchors them — `now`/`current_timestamp`/`transaction_timestamp` and the `current_*` family to the transaction, `statement_timestamp` to the statement, only `clock_timestamp` live — closing B-101 and restoring the corpus probe that had to be withdrawn for flaking, and **B-106**: a window function in a FROM-less SELECT is no longer rejected — such a query *is* one row, so it is rewritten to select from a one-row derived table and handed to the ordinary scanning path, which needs neither a synthetic scope nor a second copy of the window family's semantics (closing B-100), then a **type × operation sweep** — every supported type through fifteen operations (cast, equality, ORDER BY, DISTINCT, GROUP BY, min/max, count, UNION, coalesce, CASE, array_agg, IS NULL, nullif, IN, pg_typeof), 434 probes against real PostgreSQL — which found that **`array_agg` was returning integers** for every element type arrays cannot carry (a `.unwrap_or(Int4)` standing in for an unrepresentable value: `array_agg` over a `time` gave `{250327040}`), now a loud 0A000 (B-107, remaining element types B-108), and that an array element needing quotes was written bare unless it was text, so a timestamp array printed a literal PostgreSQL would read back as two elements (B-109). The split continues: row materialization for GROUP BY / DISTINCT / ORDER BY moves to `query/materialize.rs`, leaving `query/mod.rs` at 5307 lines — down from 10787 across eight extractions, and every module in `query/` now under 1300 lines but the root. Then **B-111**: `json` compared equal where PostgreSQL has no operator at all — it declines because two documents differing only in whitespace or key order are the same value but not the same text — so the operator now declines too, `jsonb` still comparing; the `DISTINCT`/`GROUP BY`/`ORDER BY` forms that sort by the encoding rather than the operator remain (B-112), and `min`/`max` over `json`, `jsonb`, bit strings, ranges and multiranges now decline as PostgreSQL does (B-113 — whose first recording also blamed `boolean` and `uuid`, which already worked; the sweep's report had slipped a row against its probe names, and every claim was re-checked one type at a time before the fix). `exec.rs` is now a module directory too: constraint enforcement — uniqueness, NOT NULL, CHECK, and both sides of a foreign key including the referential actions that re-enter DML — moves to `exec/constraints.rs`, leaving `exec/mod.rs` at 4285 lines. Also **B-110**: an array constructor kept its `array` column label through a cast. `sql/mod.rs` follows: its 2632-line inline test module becomes `sql/tests.rs`, leaving the engine itself at 1468 lines — the file was never mostly engine. Then `parser.rs` becomes a directory too, its data-definition statements — `CREATE TABLE` with its constraints and `LIKE` clauses, `CREATE INDEX`, `CREATE VIEW` and the `DROP` family — moving to `parser/ddl.rs` as a second `impl Parser` block; probing those paths afterwards found that a `DROP` reported `relation` where PostgreSQL names the kind, and that `DROP INDEX` raised 42P01 where PostgreSQL raises 42704 (B-114). `eval/mod.rs` follows, its casting machinery — one arm per target plus the parsers the harder ones need (bit strings, uuid, bytea's two input forms) — moving to `eval/cast.rs`; round-tripping every type through a cast afterwards found two gaps, both recorded: a range does not quote a bound carrying a space (B-115, the same rule as the array elements fixed in B-109, but decided when the canonical stored text is built rather than when it prints) and `char(n)` keeps its blank padding through a cast to text (B-116). `exec/mod.rs` gives up its table-definition building too — column metadata, PRIMARY KEY/UNIQUE, CHECK reference validation and FOREIGN KEY resolution — to `exec/ddl.rs`, leaving it at 3788 lines. Across the session the largest file has gone from 10787 to 4444 and nothing exceeds it. `query/mod.rs` then gives up its subquery machinery — the uncorrelated evaluation done once up front, the correlated re-evaluation per outer row, and the scalar/IN/EXISTS/ARRAY forms — to `query/subquery.rs`. Sweeping that surface against PostgreSQL turned up a NULL inside a row being invisible to `IN` (B-117) and row-constructor `IN (subquery)` being rejected outright (B-118), both fixed, plus a golden expectation left stale by an earlier fix (B-119). Grouped execution (`GROUP BY`, grouping sets, `HAVING`) and qualification planning (conjunct order, pushdown, canonicalization) then follow into `query/group.rs` and `query/plan.rs`, leaving `query/mod.rs` at 3147 — down from 10787 where the session started. Sweeping the grouping surface found `GROUP BY <n>` not reading as a select-list position (B-120) and the ungrouped-column error not naming its column (B-121), both fixed, and three more recorded open (B-122, B-123, B-124). A sweep for doc comments stranded by the earlier splits — a moved function's doc silently reattaching to whatever followed it — returned eleven, each moved back to what it describes or dropped where the subject was already documented accurately. `exec/mod.rs` then gives up static type analysis (what a query's columns are before a row exists) to `exec/describe.rs` and the self-describing row encoding to `exec/projected.rs`, leaving it at 2220. Sweeping that surface found a non-boolean being accepted — and returned — wherever a boolean belonged (B-125), fixed, plus two recorded open (B-126, B-127). `parser/mod.rs` then gives up expression parsing (precedence climbing and the prefix forms) to `parser/expr.rs` and the window clause to `parser/window.rs`, leaving it at 2152. Sweeping the expression grammar found `BETWEEN SYMMETRIC` unsupported (B-128), fixed, plus two more recorded open (B-129, B-130). `eval/mod.rs` then gives up scalar-argument reading and text building to `eval/args.rs` and the LIKE / SIMILAR TO / regex family to `eval/pattern.rs`, leaving it at 2555. Sweeping that surface found the `ESCAPE` clause of LIKE and SIMILAR TO unparsed (B-131), fixed, plus two recorded open (B-132, B-133). `query/mod.rs` finally gives up the set-returning functions — the ones written in the select list and the ones written in FROM — to `query/srf.rs`, leaving it at 2298. The sweep of that surface found `string_to_table` missing entirely (B-134), recorded rather than added. No file in the tree now exceeds 2555 lines, against 10787 at the start of the session, and the four files that began it — query, exec, parser, eval — are all now within the same band as the modules extracted from them. With the file sizes settled, the quality gates get the same treatment: dead-code detection, which `lib.rs`'s fictional public API had disabled crate-wide, is turned back on (B-136), and coverage is measured for the first time — across both test layers, since instrumenting only the in-process tests reports 59% and the wire protocol at 6% (B-137). Three open bugs close alongside: `string_to_table` (B-134) and the `|/`, `||/` and `@` prefix operators (B-130). A further batch then closes four more: an untyped literal now takes the type of an array operand it faces (B-129), the desugarings of `SIMILAR TO` and `OVERLAPS` no longer leak into the function router (B-132), a bare row constructor is no longer a field-access target (B-135), and an undefined operator is reported under the operator that was written (B-127). Two entries were re-examined rather than fixed and now say what is actually wrong: `smallint` has no runtime representation at all rather than merely widening under arithmetic (B-126), and undefined-function errors omit their argument types (B-138). B-138 is then fixed in the next batch alongside `substring(x SIMILAR p ESCAPE e)` (B-133), which turned out to be a parser gap alone — the extraction already existed under SQL:1999's `FROM p FOR e` spelling, so the two syntaxes now reach one implementation. `json` is then refused as a DISTINCT, GROUP BY or ORDER BY key (B-112), at one site rather than the three the entry expected. Two entries were re-examined against the server and found wider than recorded — range bound quoting is missing on input as well as output, so a range literal copied from PostgreSQL does not load (B-115), and `char(n)`'s padding lives in the value, making `length` and equality wrong rather than only a cast (B-116) — and the second turned up a third: `format_type` ignored its modifier argument, so every column read back as an unconstrained type (B-139) — the entry that recorded it blamed the catalog, which turned out to report `atttypmod` correctly, so checking the claim first is what kept a wide `ColDesc` change from being built on a false premise. Fixing it exposed B-140: the temporal types encode that modifier with a 4-byte header PostgreSQL does not use.| in progress |
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
runtime dependency (`libc`), TLS the single flagged exception (Stage G).

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

Done so far: jscpd is gated in CI (`tools/check-dups.sh` + `.jscpd.json`), and the
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

### Stage C — a real SST: sorted data blocks + sparse index + bloom filter

Replace the whole-table SST with a **block-granular** SST so a read fetches only the
blocks it needs, decoupling dataset size from RAM. LevelDB/TigerBeetle shape, all
grid blocks: sorted **data blocks** + a sparse **index block** (first-key → data
block) + a **filter block** (bloom, to skip SSTs that cannot hold a key — Loki's
bloom tier). Point lookup = bloom → index → one data block, each pulled through the
Stage-B cache; range scan streams the covering blocks. **Milestone:** cold start no
longer rehydrates whole tables into RAM; a point lookup touches O(1) blocks
(verified by fetch counters).

### Stage D — memtable flush + the manifest log (continuous ingest)

Kill the "flush not implemented" wall: ingest becomes bounded by flush *rate*, not
RAM *size*. The Loki **ingester** pattern, disciplined à la TigerBeetle: at a
high-water mark, freeze the memtable (a read-only second memtable), flush it to a
level-0 SST (Stage C), drop it, and keep writing against a fresh one — the WAL
already protects the un-flushed tail. Replace the monolithic manifest rewrite with a
**manifest log** (TigerBeetle `manifest_log` / Loki index shipping): append
SST-added/removed records, compact the log periodically, and let a small
**superblock** root — the only CAS'd object — point at the log tail. **Milestone:**
insert far more than `memtable_bytes` of live rows with zero "memtable full" errors;
kill -9 mid-flush recovers exactly. **Risk (crux invariant):** a flushed SST must be
referenced in the manifest log *before* its WAL range is reclaimed — model it in
Stage H.

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

## Deviations from the original plan (deliberate, revisitable)

- **Snapshot checkpoints instead of a leveled LSM.** Each checkpoint uploads
  the full live state per table as one SST and swaps the manifest via
  compare-and-swap; the working set is bounded by `memtable_bytes` (a full
  memtable fails loudly — flush is not implemented). This gives
  cold-start-from-bucket and bounded read amplification (always 0 extra) at
  the cost of write amplification per checkpoint. Leveled SSTs + block/disk
  cache become worthwhile when the working set must exceed RAM — the path there
  is the *Object-storage LSM roadmap* above (Stages A–H).
- **Simple query strings run statement-by-statement without an implicit
  rollback** (see BUGS.md B-001) until real transactions land.
- **Checkpoint S3 calls are synchronous** in the single-threaded loop: a
  checkpoint stalls other connections while it runs. Fine at dev scale; async S3
  through the reactor is the fix when it matters. WAL-segment upload is already
  asynchronous — drained off the commit path by the event loop (B-008), with a
  `wal_upload_sync` opt-in for synchronous RPO=0-to-S3.
- **TLS**: still deliberately absent (MinIO/plaintext for dev; decision
  documented in README).

## Verification

- `cargo test` — 275 unit/property tests (memory guard incl. unwind safety,
  differential FixedMap vs std, PCG32/CRC-32C/SHA-256/HMAC/SigV4 official
  vectors, row codec fuzz-by-truncation, WAL corruption/floor/stale-tail,
  engine restart persistence, protocol framing).
- `POS3QL_MINIO_ENDPOINT=... cargo test --test minio_it` — S3 client CAS/range
  /list + engine checkpoint/cold-start integration against real MinIO.
- `tests/external/run.sh` — the external conformance suite (12 checks):
  psql 18.4 golden files, protocol 3.0/3.2, raw wire probes, psycopg 3,
  kill -9 recovery, cold start from bucket. All green as of 2026-07-15.
- `cargo clippy --all-targets` — zero warnings.
- **No-op guard** (`tools/check-noops.sh`, gated by `cargo test` and CI): fails
  on any silent accept-and-ignore of SQL/protocol semantics, so a gap is
  implemented or rejected loudly, never quietly skipped. The initial debt
  (B-019 SET/GUCs, B-020 varchar/numeric, B-021 PREPARE types) is fully burned
  down; the ratchet budget is 0.
- Post-freeze allocation is enforced at runtime: the guard aborted on a real
  bug (ToSocketAddrs allocating in the checkpoint path) during development,
  which is exactly its job.
