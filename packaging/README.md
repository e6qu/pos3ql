# Installing the release archive

The archive contains one Linux executable and its operator files. Verify the
adjacent SHA-256 file before installation.

```sh
sha256sum --check pos3ql-*.tar.gz.sha256
sudo install -m 0755 bin/pos3ql /usr/local/bin/pos3ql
sudo install -d -m 0750 -o pos3ql -g pos3ql /etc/pos3ql /var/lib/pos3ql
sudo install -m 0640 -o root -g pos3ql etc/pos3ql/pos3ql.conf /etc/pos3ql/pos3ql.conf
sudo install -m 0644 lib/systemd/system/pos3ql.service /etc/systemd/system/pos3ql.service
```

Create the `pos3ql` system user before installation. Configure object storage,
TLS, authentication, memory capacities, and an owner-only object-store
credential file before enabling the service. Then run:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now pos3ql
curl --fail http://127.0.0.1:9187/readyz
```

See `share/doc/pos3ql/operations.md` for probes, credential rotation, alerts,
and controlled replacement.
