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
| B-351 | open | 2026-11-16 | A connection string naming its server by host name fails where libpq resolves the name: a subscription with `22023` (`subscription connection string requires a numeric host address`), a postgres_fdw server with `HV000` (`invalid foreign PostgreSQL endpoint: NonNumericHost`). | `CREATE SUBSCRIPTION s CONNECTION 'host=localhost port=5432 user=postgres dbname=postgres sslmode=disable' PUBLICATION p` | Intractable within the runtime rules: libpq resolves names with the platform resolver (`getaddrinfo`: nsswitch modules and search domains on Linux, mDNSResponder on macOS), which allocates heap memory and blocks on every call, so it breaks the fixed-memory rule; a fixed-memory resolver reading `/etc/hosts` and querying DNS would not resolve as libpq does. |
