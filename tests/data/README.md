# Test fixtures

These files let tests reproduce real client and recovery behavior. For
example, the dump archives check that a user can restore data with PostgreSQL's
own tools, including a parallel `pg_restore --clean --if-exists`.
Keep the recorded generator, sources, and checksums with each fixture. Fixture
provenance describes how those bytes were produced, not the current server
version or deployment qualification. [Contributing](../../CONTRIBUTING.md)
defines current validation.

`tls-test-cert.pem` / `tls-test-key.pem` — a self-signed certificate for the
in-process TLS round-trip test (`object_store::http::tests::tls_round_trip`). Test-only trust:
the test passes the certificate as `object_store_tls_ca_file`; nothing outside tests
reads these files. Generated locally (2026-07-24, 20-year validity) with:

    openssl req -x509 -newkey rsa:2048 -keyout tls-test-key.pem \
      -out tls-test-cert.pem -days 7300 -nodes -subj "/CN=localhost" \
      -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
      -addext "basicConstraints=critical,CA:FALSE"

(`CA:FALSE` matters: webpki refuses a CA-flagged certificate presented as the
server's end entity, while a trust anchor needs no CA bit.)

`postgresql-18.4-plain-dump.sql` is the vanilla PostgreSQL 18.4 plain-format
restore fixture. Its header records the generator and the only added
provenance paragraph.

`postgresql-15.18-pgrestore.dump` is the ownerful custom-format companion used
to exercise real `pg_restore`, including parallel workers and
`--clean --if-exists`. PostgreSQL 15's archive format is readable by the
supported CI client versions. Exact source, generation steps and checksums are
in `postgresql-15.18-pgrestore.dump.provenance`.
