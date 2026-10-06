# Installing the release archive

The archive contains one Linux executable and its operator files. Create the
`pos3ql` system user before installation. Replace `VERSION` with the downloaded
release version, verify the adjacent checksum, and enter the extracted archive:

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

Configure object storage,
TLS, authentication, memory capacities, and an owner-only object-store
credential file before enabling the service. Then run:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now pos3ql
curl --fail http://127.0.0.1:9187/readyz
```

See `share/doc/pos3ql/docs/README.md` for the documentation index and
`share/doc/pos3ql/docs/operations.md` for probes, credential rotation, alerts,
and controlled replacement. The previous flat operator paths redirect there.

On one passive candidate for a durable object prefix, copy
`etc/pos3ql/pos3ql-failover.conf.example` to
`/etc/pos3ql/pos3ql-failover.conf`, set the primary and candidate readiness
URLs, and keep `pos3ql.service` disabled. Enable `pos3ql-failover.service`
instead. After the configured consecutive-failure threshold, the monitor
starts the candidate once and waits for fence-validating readiness. Follow the
failover procedure in `share/doc/pos3ql/docs/operations.md`; use one promotion
authority for each prefix.
