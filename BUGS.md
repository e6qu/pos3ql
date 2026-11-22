# Blocked defects

One known defect cannot be repaired within the runtime rules (reviewed
2026-11-22).

Fix reproducible defects, including incidental ones, in the PR that finds them.
Unfinished work belongs in [PLAN.md](PLAN.md). Add a row here only with a
reproducer, the observed behavior and the exact external blocker or reason it
cannot be repaired. [CONTRIBUTING.md](CONTRIBUTING.md) covers verification and
failure diagnosis.

| ID | Status | Found | Description | Reproducer | Blocker |
|----|--------|-------|-------------|------------|---------|
| B-351 | open | 2026-11-16 | A connection string naming its server by host name fails where libpq resolves the name: a subscription with `22023` (`subscription connection string requires a numeric host address`), a postgres_fdw server with `HV000` (`invalid foreign PostgreSQL endpoint: NonNumericHost`). | `CREATE SUBSCRIPTION s CONNECTION 'host=localhost port=5432 user=postgres dbname=postgres sslmode=disable' PUBLICATION p` | Intractable within the runtime rules (re-measured 2026-11-22). libpq resolves names with `getaddrinfo`, which allocates with libc `malloc`, outside the Rust allocator through which pos3ql budgets its one isolated component (rustls): no in-process thread can bound or account for it. Measured with `malloc_zone_statistics` on macOS 15.6.1 arm64, the first `getaddrinfo("localhost")` keeps 21,264 bytes of heap after `freeaddrinfo`, the first DNS lookup (`example.com`) 78,224 bytes, both caches whose size follows resolver state and responses. A resolver in a child process with a fixed address-space limit would isolate it on Linux (`RLIMIT_AS`), but macOS rejects lowering `RLIMIT_DATA` or `RLIMIT_AS` (`setrlimit` fails with `EINVAL`, and a 2 GiB allocation succeeds), so no fixed budget can be enforced on a supported native target. A fixed-memory resolver reading `/etc/hosts` and querying DNS itself would not resolve as libpq does (nsswitch modules and search domains on Linux, mDNSResponder on macOS). |
