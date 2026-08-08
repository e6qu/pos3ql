# Object storage contract

pos3ql treats object storage as the only durable tier. An adapter or gateway
qualifies only when it provides these semantics for one namespace:

| Operation | Required semantic |
|---|---|
| PUT with `If-None-Match: *` | Create immutable data or fail without replacing an existing object. |
| PUT with `If-Match: <strong ETag>` | Replace precisely that generation or fail. |
| GET | Return the requested object and a strong, quoted ETag. |
| Ranged GET | Honor inclusive byte ranges. |
| LIST | Return every key under a prefix, following continuation tokens. |
| DELETE | Delete a key; deleting an absent key is idempotent. |

The adapter sends path-style S3 requests and SigV4 authentication. S3 and
MinIO work directly. GCS and Azure are supported when placed behind a gateway
that preserves the contract above; native provider APIs are not silently
substituted.

`object_store_endpoint` is an authority (`host:port` or `[ipv6]:port`), never
a URL. It is parsed once at startup and supplies the TCP address, HTTP Host
header, and TLS server name.

## Qualification

Run the same integration suite against each intended endpoint. Supplying an
endpoint requires all identity inputs; the test deliberately has no provider
defaults:

```sh
POS3QL_OBJECT_STORE_ENDPOINT=objects.example:443 \
POS3QL_OBJECT_STORE_BUCKET=pos3ql-qualification \
POS3QL_OBJECT_STORE_REGION=us-east-1 \
POS3QL_OBJECT_STORE_ACCESS_KEY=... \
POS3QL_OBJECT_STORE_SECRET_KEY=... \
POS3QL_OBJECT_STORE_TLS=on \
cargo test --locked --test object_store_it
```

Set `POS3QL_OBJECT_STORE_TLS_CA_FILE` when the endpoint uses a private CA.
The suite proves PUT/GET/range/LIST/DELETE/CAS and cold-start durability; a
passing run is the admission criterion for a gateway.
