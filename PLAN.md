# pos3ql roadmap

Architecture summary lives in [README.md](README.md). Decisions fixed with the
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
| P13 | Full PostgreSQL fidelity | Strict differential/sqllogictest fidelity (no papering over gaps): arbitrary-precision **NUMERIC** (base-10000, PG numeric.c representation, exact division scale), plan-time semantic type analysis (42883 before scanning), **correlated subqueries + EXISTS/NOT EXISTS** (scalar/IN/EXISTS, streaming + materialized paths), **subqueries in FROM-less SELECT** and `SELECT *` single-column subqueries, **INSERT ... SELECT** (materialize-then-insert, self-insert safe), exact **`x IN (subquery)`** empty/all-NULL/operand-type semantics, and **DISTINCT aggregates** (`count/sum/avg/min/max/string_agg(DISTINCT ...)`), **`string_agg`**, **date arithmetic** (`date ± int`, `date - date`), **derived tables** (`FROM (SELECT ...) alias`, materialized; compose with WHERE/aggregates/ORDER BY/joins), **non-recursive CTEs** (`WITH`, expanded into derived tables), **GROUP BY/aggregates as a row source** (derived tables, CTEs, set-op branches, and INSERT ... SELECT over grouped queries), **aggregate ORDER BY** (`string_agg(x ORDER BY k)`), **durable CREATE VIEW/DROP VIEW** (registry + WAL + manifest; expanded as a derived table at query time; columns validated at creation), and **durable CREATE INDEX/DROP INDEX** (catalog + WAL + manifest, composite UNIQUE enforcement via 23505; no query acceleration; DROP TABLE cascades to indexes), and **DML on auto-updatable views** (rewritten onto the base table). sqllogictest replay (3205 blocks): match=3203, divergence=0, 2 unsupported (only bit-string literals — `x'…' IN (SELECT int)`, which PostgreSQL also rejects: 42883 bit=integer). Large ORDER BY / DISTINCT / GROUP BY results materialize in a shared `work_mem`-analogue arena (`work_arena_bytes`, default 64 MiB — larger than PostgreSQL's 4 MiB default), reset per statement; by design this `work_mem` is a hard bound (a result exceeding it errors 54000 rather than spilling to temporary files — B-006, accepted). No known SQL-surface fidelity gaps remain. | **done** |

| P14 | Client/tooling & datatype fidelity | Driver- and tool-facing fidelity from a fresh audit of the wire/SQL/CLI/SDK surface: accept the common session GUCs real drivers set (`extra_float_digits`, `client_min_messages`, `bytea_output`, `lock_timeout`, `row_security`, zero-valued `statement_timeout`/`idle_*`), casts with type modifiers (`x::varchar(10)`, `CAST(x AS numeric(8,2))` — truncation applied), `SET`/`SHOW TRANSACTION ISOLATION LEVEL` and `SHOW ALL`, distinct **smallint/real/varchar/char** types (own OIDs/names/typmod, `varchar(n)`/`char(n)` length enforcement, 22003/22P02 surfaced), **SERIAL/bigserial/smallserial** (max-based auto-increment), **INSERT ... ON CONFLICT** (`DO NOTHING`/`DO UPDATE`, `excluded.*`), `JOIN ... USING`, and **named/DST time zones** (`SET timezone='America/New_York'` etc.; per-timestamp offset+abbrev via POSIX DST rules; ~25 IANA zones + `Etc/GMT±n` + bare numeric offsets; JDBC/psql now connect and introspect from any zone), and **table constraints** (multi-column PRIMARY KEY / UNIQUE, CHECK, and FOREIGN KEY — durable in the table catalog, enforced on INSERT/UPDATE/DELETE with PG-matching SQLSTATEs; parent-side NO ACTION/RESTRICT, with CASCADE/SET-actions rejected loudly pending a follow-up — see B-029). and **join/DML breadth** (RIGHT and FULL OUTER JOIN; `UPDATE ... FROM` and `DELETE ... USING`; NATURAL JOIN and multi-join RIGHT/FULL rejected loudly pending follow-up — see B-030). and **subtransactions** (SAVEPOINT / RELEASE / ROLLBACK TO SAVEPOINT — the transaction undo log records every row write with its prior image so nested rollback is byte-exact vs PostgreSQL; see B-031). and **window functions** (row_number/rank/dense_rank, lag/lead, and aggregate windows with PARTITION BY / ORDER BY and the default frame — running-with-peers or whole-partition; see B-032). and the **`time`** and **`interval`** types (time-of-day; and interval with months/days/micros fields, verbose parse, PG-exact output, and date/timestamp/interval arithmetic with calendar-month clamping — B-034). and **`json`/`jsonb`** (json verbatim; jsonb parsed and canonicalized — sorted/deduped keys, canonical numbers; `->`/`->>` accessors — B-034). and **one-dimensional arrays** (`ARRAY[...]`/`'{...}'::elem[]`, subscripting, `= ANY/ALL`, `array_length`/`cardinality`, element-wise ordering — B-034, done). and **range types** (`int4range`/`int8range`/`numrange`/`daterange`/`tsrange`/`tstzrange` — canonical-text `Datum::Range`, constructors, text cast, `lower`/`upper`/`isempty`/`lower_inc`/`upper_inc`, the `@>`/`<@`/`&&` operators, and value-based comparison/ordering `= <> < <= > >=` including `ORDER BY`/`GROUP BY`/`DISTINCT`; storage/WAL/wire I/O; B-047, done). Remaining: full psql `\d <table>` (\dt works; \d table needs more pg_class/pg_attribute — B-033). | in progress |
| P15 | Differential CI at scale | Wire the existing differential + fuzz machinery into CI as its own workflow (`differential.yml` → `tests/external/ci_diff.sh`): a real PostgreSQL 18 service (C collation, **UTF8** encoding to match pos3ql and vanilla PG) is the oracle; the suite replays the vendored sqllogictest corpus and the generative fuzzer against both engines and diffs rows + SQLSTATEs. Hardened so a pathological query can never wedge CI: **predicate pushdown** removes the O(Nᵏ) multi-way-equi-join blowup that hung the run for 45+ min (B-037, `select5` now seconds, divergence 0), a per-statement `statement_timeout` guard is set where the engine honors it (B-038), and the job carries a hard `timeout-minutes` ceiling. The comparator decodes text-returned-as-bytes losslessly, and both sessions pin `TimeZone='UTC'`, so neither a server-encoding nor a host-timezone quirk can masquerade as a data divergence. The generative fuzzer runs against a freshly-restarted pos3ql (a clean table space, since the corpus fills the bounded catalog) and fails loudly on any setup error; its ~127 remaining error-timing/semantic divergences are gated by a ratchet `FUZZ_BUDGET` to drive toward 0 (B-039). CI is deduplicated (one run per ref) and caches the Rust build. | in progress |

Phase discipline: fine-grained commit per task; PLAN.md and BUGS.md updated in
the same commit series as the phase they describe; no phase numbers or bug IDs
in code or code comments (the "why" goes in commit messages).

## Deviations from the original plan (deliberate, revisitable)

- **Snapshot checkpoints instead of a leveled LSM.** Each checkpoint uploads
  the full live state per table as one SST and swaps the manifest via
  compare-and-swap; the working set is bounded by `memtable_bytes`. This gives
  cold-start-from-bucket and bounded read amplification (always 0 extra) at
  the cost of write amplification per checkpoint. Leveled SSTs + block/disk
  cache become worthwhile when the working set must exceed RAM.
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

- `cargo test` — 236 unit/property tests (memory guard incl. unwind safety,
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
