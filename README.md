# Sigil S3 plugin

`wasm.s3` is a bounded, read-only S3-compatible object client for Sigil
scenarios. Version 0.2 performs one path-style HTTP `GET` and returns the exact
object bytes as a binary Lua string. It supports anonymous and presigned reads
through the existing raw-network path, plus private reads through Sigil's
opaque host-owned SigV4 signing grants.

The tagged authentication value makes conflicting modes unrepresentable. Lua
variants use `{ tag = "...", value = ... }`:

```lua
local s3 = require("wasm.s3")

local bytes, err = s3["get-object"]({
  bucket = "results",
  key = "run/output.parquet",
  auth = { tag = "sigv4", value = "object-store-read" },
  ["max-bytes"] = 4 * 1024 * 1024,
})
expect(bytes ~= nil, err and err.message)
```

The `object-store-read` string is an opaque signing-grant name. The component
cannot supply or observe an endpoint, access key, secret key, session token,
region, service, authority, or time for that mode. The operator grant owns all
of those values and is the only route selector:

```toml
[plugins.grants.s3.network.object-store]
target = "minio:9000"
tls = "disabled"
connect_timeout = "5s"
io_timeout = "10s"
max_connections = 1
max_bytes = "17MiB"

[plugins.grants.s3.sigv4.object-store-read]
endpoint = "object-store"
access_key_secret = "OBJECT_STORE_ACCESS_KEY"
secret_key_secret = "OBJECT_STORE_SECRET_KEY"
region = "us-east-1"
service = "s3"
authority = "minio:9000"
methods = ["GET"]
canonical_uri_prefixes = ["/results/"]
query = {}
header_names = []
```

Sigil resolves the named secrets from the scenario environment allowlist. No
secret name or value crosses the component ABI. The host captures signing time,
constructs and signs the exact wire request, connects only through the grant's
named endpoint, sends once, and returns a read-only bounded response resource.
The plugin has no secret, clock, random, filesystem, write, redirect, or retry
path.

Anonymous and presigned reads remain available with their 0.1 behavior:

```lua
local anonymous = {
  tag = "anonymous",
  value = { endpoint = "object-store" },
}

local presigned = {
  tag = "presigned",
  value = {
    endpoint = "object-store",
    query = query,                 -- without the leading `?`
    authority = "127.0.0.1:9000", -- exact signed HTTP Host authority
  },
}
```

For these modes the endpoint still names the operator-granted socket route.
The presigned authority changes only the HTTP `Host` header and cannot redirect
the connection. The raw route uses `Accept-Encoding: identity`; the response
parser accepts only exact fixed-length or chunked framing and never treats a
short or truncated body as complete.

Every mode enforces `max-bytes` up to 16 MiB. The endpoint or signing-grant
quota must additionally cover up to 64 KiB of response framing. A structured
S3 `RequestTimeTooSkewed` or `RequestExpired` response becomes `clock-skew`;
local signing-lease expiry and all other host transport failures become
sanitized `transport` errors. Raw upstream XML, request IDs, signature data,
and host-internal details are never returned.

The immutable 0.1.1 interface is retained byte-for-byte at
`contracts/sigil-s3-client-0.1.1.wit` and verified by `just check`. The 0.2
candidate exports only `get-object`; metadata and object listing are separate
future contract increments.

Build, validate against the pinned SDK host WITs, and pack a local development
archive:

```bash
just check
just dist
```

`SDK_CHECKOUT=/path/to/sigil-plugin-sdk just check` verifies against a local
SDK checkout; otherwise the exact commit in `SDK.lock` is fetched and compared.

Official versions are published once from independently reviewed candidate
artifacts by the repository's keyless GitHub OIDC workflow. Public tags and
release assets are immutable; a conflicting release burns that SemVer.
