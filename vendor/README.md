# Third-party test material

The differential tests run the same SQL, for example a query from PostgreSQL's
own regression tests, against PostgreSQL and pos3ql and compare the results.
This directory supplies those inputs and records where they came from.

Nothing here is compiled into the pos3ql binary: `build.rs` only records the
target triple, and no `include_str!`/`include_bytes!` names these paths. Source
files ported from or generated against PostgreSQL 18.6 (date/time input, type
layouts, time zone abbreviations, catalog view definitions, information schema
columns) cite `vendor/test/postgres-regress/COPYRIGHT` for that license; other
compiled-in third-party code and data are listed under
[Snowball provenance](../docs/postgresql-18-compatibility.md#snowball-provenance).
Runtime dependencies are libc and the isolated, budgeted object-store TLS
component (rustls, compiled root data and their dependencies), declared in
`Cargo.toml`, pinned in `Cargo.lock` and fetched by Cargo, not vendored.

Each corpus records its upstream source, pinned commit or tag, and license file,
and every file is checksum-pinned in `SHA256SUMS`. Because test material is
never linked into the binary, its license need not be compatible with pos3ql's
AGPL-3.0-or-later; anything vendored for the build would have to live outside
`vendor/test/` and meet that stricter rule. There is no such item today.

    vendor/
      README.md      <- this file
      SHA256SUMS     <- checksum of every vendored file
      test/          <- test-only corpora replayed by tests/external/
        sqllogictest/
        postgres-regress/

## Test-only corpora (`vendor/test/`)

### `test/sqllogictest/` — testing only

- **Upstream:** https://github.com/gregrahn/sqllogictest
- **Pinned commit:** `c67f97bf3ca7e590d12e073408bcacaf2ff0f3a0`
- **License:** multi-licensed (GPL / BSD / MIT / CC0) by D. Richard Hipp —
  see `test/sqllogictest/COPYRIGHT.md`. "No attribution is required."
- **What:** the `select1..5` core corpus plus a few `evidence/` files
  (~11k statement/query blocks). Originally SQLite's tests; the SQL is replayed
  against PostgreSQL and pos3ql, ignoring the embedded SQLite result hashes.
- **Fetch:**
  `curl -sSL https://raw.githubusercontent.com/gregrahn/sqllogictest/<commit>/test/<file>`

The full upstream corpus has millions of query records; point
`tests/external/slt_diff.py` at a full local checkout to run all of it.

### `test/postgres-regress/` — testing only

- **Upstream:** https://github.com/postgres/postgres (`src/test/regress`)
- **Pinned tag/commit:** `REL_18_6` = `724edf9bde9d356724ad384a2e196edc3c9f80f7`
- **License:** The PostgreSQL License — see `test/postgres-regress/COPYRIGHT`.
- **What:** a slice of the official regression `.sql` inputs (and their
  `expected/*.out`) that exercises `boolean`, `text`, `int4`, `int8`,
  `float8`, `case`, `bit`, `uuid`, `enum`, `varchar`, `char`, `name`, `md5`,
  and `pg_lsn`. The manifest in
  `tests/external/postgres_regress_schedule.tsv` selects the tests: every
  listed statement-boundary range is run against PostgreSQL and pos3ql with a
  zero-mismatch budget. Its explicit gaps correspond to documented
  architecture limits, not hidden expected failures. `float8.sql` stops before
  its native `regress.so` test type, a server-extension ABI pos3ql does not
  provide.

## Integrity

`SHA256SUMS` lists the checksum of every vendored file (paths relative to
`vendor/`). Verify with:

    (cd vendor && shasum -a 256 -c SHA256SUMS)

To refresh a corpus, re-fetch at the pinned commit above and regenerate the
checksums:

    (cd vendor && find test -type f | sort | xargs shasum -a 256 > SHA256SUMS)
