# Installing the release archive

**User example:** install a release on a Linux x86-64 host, point it at your
object store, and check readiness before sending application traffic. You
must create the bucket and credentials yourself.

Create the `pos3ql` system user first. Replace `VERSION` with the release
version, verify the checksum, and extract:

```sh
pos3ql_archive=pos3ql-vVERSION-x86_64-unknown-linux-gnu.tar.gz
sha256sum --check "$pos3ql_archive.sha256"
tar -xzf "$pos3ql_archive"
cd "${pos3ql_archive%.tar.gz}"
```

```sh
sudo install -m 0755 bin/pos3ql /usr/local/bin/pos3ql
sudo install -d -m 0755 /usr/local/libexec/pos3ql
sudo install -m 0755 libexec/pos3ql/failover-monitor /usr/local/libexec/pos3ql/failover-monitor
sudo install -d -m 0750 -o pos3ql -g pos3ql /etc/pos3ql /var/lib/pos3ql
sudo install -m 0640 -o root -g pos3ql etc/pos3ql/pos3ql.conf /etc/pos3ql/pos3ql.conf
sudo install -m 0644 lib/systemd/system/pos3ql.service /etc/systemd/system/pos3ql.service
sudo install -m 0644 lib/systemd/system/pos3ql-failover.service /etc/systemd/system/pos3ql-failover.service
```

Before enabling the service, configure object storage, TLS, authentication,
memory capacities, and an owner-only object-store credential file. The
starter file is an example, not a qualified production size; physical limits
are in `share/doc/pos3ql/docs/architecture.md`. Then:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now pos3ql
curl --fail http://127.0.0.1:9187/readyz
```

The documentation index is `share/doc/pos3ql/docs/README.md`. Probes,
credential rotation, alerts, controlled replacement, and the failover
procedure are in `share/doc/pos3ql/docs/operations.md`.

For automatic failover, on one passive candidate per durable prefix copy
`etc/pos3ql/pos3ql-failover.conf.example` to
`/etc/pos3ql/pos3ql-failover.conf`, set the primary and candidate readiness
URLs, keep `pos3ql.service` disabled, and enable `pos3ql-failover.service`.
