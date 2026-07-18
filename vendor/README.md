# Vendored external test corpora

Third-party SQL test suites, vendored here so the differential harness can
replay them against real PostgreSQL 18 and against pos3ql. Nothing here is
part of the shipped engine; it is test input only. Each source is pinned to
an exact upstream commit and its license is included alongside it.

To refresh a corpus, re-fetch at the pinned commit below and update the
checksums in `SHA256SUMS`.

## Sources

### `sqllogictest/`
- **Upstream:** https://github.com/gregrahn/sqllogictest
- **Pinned commit:** `c67f97bf3ca7e590d12e073408bcacaf2ff0f3a0`
- **License:** multi-licensed (GPL / BSD / MIT / CC0) by D. Richard Hipp —
  see `sqllogictest/COPYRIGHT.md`. "No attribution is required."
- **What:** the `select1..5` core corpus plus a few `evidence/` files
  (~11k statement/query blocks). Originally SQLite's tests; here we use the
  SQL as a *source of statements* and diff real PostgreSQL against pos3ql,
  ignoring the embedded SQLite result hashes.
- **Fetch:**
  `curl -sSL https://raw.githubusercontent.com/gregrahn/sqllogictest/<commit>/test/<file>`

The full upstream corpus is far larger (millions of query records). Point
`tests/external/slt_diff.py` at a full local checkout to run all of it; the
subset here keeps the repo small while covering the core.

### `postgres-regress/`
- **Upstream:** https://github.com/postgres/postgres (`src/test/regress`)
- **Pinned tag/commit:** `REL_18_4` = `f5cc81719e6da4cbdb1f797c48b693e91018153a`
- **License:** The PostgreSQL License — see `postgres-regress/COPYRIGHT`.
- **What:** a slice of the official regression `.sql` inputs (and their
  `expected/*.out`) that exercises features pos3ql implements: `int4`,
  `int8`, `float8`, `text`, `boolean`, `case`. PostgreSQL's own suite is
  thousands of statements across ~200 files; most of it uses features
  outside our subset, so we vendor the parts we can meaningfully diff and
  let the harness categorize the rest as "unsupported".

## Integrity

`SHA256SUMS` lists the checksum of every vendored file. Verify with:

    (cd vendor && shasum -a 256 -c SHA256SUMS)
