# `vendor/` — third-party material, kept out of the build

Everything under `vendor/` is **third-party** and is **never compiled into the
pos3ql binary**: there is no `build.rs`, no `include_str!`/`include_bytes!`, and
nothing in `src/` references these paths. The shipped engine depends only on
`libc` (MIT OR Apache-2.0), declared in `Cargo.toml` — which is *not* vendored
here; Cargo fetches it. This directory is for material we keep in-tree but do
not distribute as part of the compiled program.

Each item records its **upstream source**, the **exact pinned commit/tag** it
was taken at, and its **license** (a `COPYRIGHT`/`LICENSE` file alongside it),
and every file is checksum-pinned in `SHA256SUMS`.

## Layout — what is here and what it is for

    vendor/
      README.md      <- this file
      SHA256SUMS     <- checksum of every vendored file (see "Integrity")
      test/          <- TEST-ONLY corpora; used by the differential harness,
                        not shipped, not part of any build
        sqllogictest/
        postgres-regress/

Anything under **`vendor/test/` is for testing only** — replayed by
`tests/external/` (differential vs. real PostgreSQL and pos3ql). Because it is
never linked into the binary, its licensing may be more relaxed than a runtime
dependency; it must still be clearly marked (source + pinned commit + license +
checksum), which it is. If we ever vendor something that *is* linked into the
build, it must go **outside `vendor/test/`** and satisfy the stricter rule that
its license be compatible with pos3ql's **AGPL-3.0-or-later**. Today there is no
such item — the only build dependency is `libc`, via Cargo.

## Test-only corpora (`vendor/test/`)

### `test/sqllogictest/` — testing only
- **Upstream:** https://github.com/gregrahn/sqllogictest
- **Pinned commit:** `c67f97bf3ca7e590d12e073408bcacaf2ff0f3a0`
- **License:** multi-licensed (GPL / BSD / MIT / CC0) by D. Richard Hipp —
  see `test/sqllogictest/COPYRIGHT.md`. "No attribution is required."
- **What:** the `select1..5` core corpus plus a few `evidence/` files
  (~11k statement/query blocks). Originally SQLite's tests; here we use the
  SQL as a *source of statements* and diff real PostgreSQL against pos3ql,
  ignoring the embedded SQLite result hashes.
- **Fetch:**
  `curl -sSL https://raw.githubusercontent.com/gregrahn/sqllogictest/<commit>/test/<file>`

The full upstream corpus is far larger (millions of query records). Point
`tests/external/slt_diff.py` at a full local checkout to run all of it; the
subset here keeps the repo small while covering the core.

### `test/postgres-regress/` — testing only
- **Upstream:** https://github.com/postgres/postgres (`src/test/regress`)
- **Pinned tag/commit:** `REL_18_4` = `f5cc81719e6da4cbdb1f797c48b693e91018153a`
- **License:** The PostgreSQL License — see `test/postgres-regress/COPYRIGHT`.
- **What:** a slice of the official regression `.sql` inputs (and their
  `expected/*.out`) that exercises features pos3ql implements: `int4`,
  `int8`, `float8`, `text`, `boolean`, `case`. PostgreSQL's own suite is
  thousands of statements across ~200 files; most of it uses features
  outside our subset, so we vendor the parts we can meaningfully diff and
  let the harness categorize the rest as "unsupported".

## Integrity

`SHA256SUMS` lists the checksum of every vendored file (paths relative to
`vendor/`). Verify with:

    (cd vendor && shasum -a 256 -c SHA256SUMS)

To refresh a corpus, re-fetch at the pinned commit above and regenerate the
checksums:

    (cd vendor && find test -type f | sort | xargs shasum -a 256 > SHA256SUMS)
