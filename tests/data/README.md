# Test fixtures

`tls-test-cert.pem` / `tls-test-key.pem` — a self-signed certificate for the
in-process TLS round-trip test (`s3::tests::tls_round_trip`). Test-only trust:
the test passes the certificate as `s3_tls_ca_file`; nothing outside tests
reads these files. Generated locally (2026-07-24, 20-year validity) with:

    openssl req -x509 -newkey rsa:2048 -keyout tls-test-key.pem \
      -out tls-test-cert.pem -days 7300 -nodes -subj "/CN=localhost" \
      -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
      -addext "basicConstraints=critical,CA:FALSE"

(`CA:FALSE` matters: webpki refuses a CA-flagged certificate presented as the
server's end entity, while a trust anchor needs no CA bit.)
