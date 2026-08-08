# Object storage contract

pos3ql treats object storage as the only durable tier. It speaks one generic
gateway contract; provider APIs and SDKs are outside the process.

| Operation | Required semantic |
|---|---|
| PUT with `If-None-Match: *` | Create immutable data or fail without replacing an existing object. |
| PUT with `If-Match: <strong ETag>` | Replace precisely that generation or fail. |
| GET | Return the requested object and a strong, quoted ETag. |
| Ranged GET | Honor inclusive byte ranges. |
| LIST | Return every key under a prefix. |
| DELETE | Delete a key; deleting an absent key is idempotent. |

The gateway exposes `PUT`, `GET`, `DELETE`, and `GET ?prefix=` at
`/v1/objects/<namespace>[/<key>]`. It returns strong quoted ETags and a
newline-delimited LIST body. Optional authentication is a bearer token.

`object_store_endpoint` is an authority (`host:port` or `[ipv6]:port`), never
a URL. It is parsed once at startup and supplies the TCP address, HTTP Host
header, and TLS server name.

## Qualification

Run the same integration suite against each intended endpoint. Supplying an
endpoint requires all identity inputs; the test deliberately has no provider
defaults:

```sh
POS3QL_OBJECT_STORE_ENDPOINT=objects.example:443 \
POS3QL_OBJECT_STORE_NAMESPACE=pos3ql-qualification \
POS3QL_OBJECT_STORE_TOKEN=... \
POS3QL_OBJECT_STORE_TLS=on \
cargo test --locked --test object_store_it
```

Set `POS3QL_OBJECT_STORE_TLS_CA_FILE` when the endpoint uses a private CA.
The suite proves PUT/GET/range/LIST/DELETE/CAS and cold-start durability; a
passing run is the admission criterion for a gateway.
