# Sigil S3 plugin

`wasm.s3` is a bounded, read-only S3-compatible object client for Sigil
scenarios. The public 0.3.0 stable release performs one path-style
HTTP `GET`, `HEAD`, or bounded ListObjectsV2 page. GET
returns the exact object bytes as a binary Lua string; HEAD returns optional
size, ETag, and unnormalized Last-Modified metadata without reading an object
body. Both operations support anonymous and presigned requests through the
raw-network path, plus private requests through Sigil's opaque host-owned
SigV4 signing grants. Listing never follows a continuation token itself: it
returns one ordered page and lets the scenario choose whether to continue.

Version 0.3.0 requires Sigil 0.33.2-rc.1 or newer and Host API 1.2.
The stable Sigil 0.33.1 release predates manifest schema 3 and Host API 1.2 and cannot load it.
Add the exact immutable identity with `sigil plugin add s3@0.3.0`.

| Public stable 0.1.0 | Public stable 0.3.0 |
|---|---|
| anonymous or presigned GET with an endpoint field | tagged anonymous, presigned, or opaque host-owned SigV4 auth |
| GET only | GET, HEAD, and one bounded ListObjectsV2 page |
| caller owns credentials and signing | Sigil owns named secrets, signed Host authority, current signing time, and canonical policy |
| no discovery | caller explicitly supplies each returned continuation token; no hidden pagination |

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

HEAD has a separate input record because its fixed 32 KiB response-header cap
is independent of the represented object's size:

```lua
local metadata, err = s3["head-object"]({
  bucket = "results",
  key = "run/output.parquet",
  auth = { tag = "sigv4", value = "object-store-read" },
})
expect(metadata ~= nil, err and err.message)
expect(metadata["content-length"] == 5766)
expect(metadata.etag ~= nil)
```

All three fields are optional. A missing header is `nil`, while a present
empty ETag or Last-Modified value is `""`. Content-Length is an unsigned
64-bit value and preserves present zero separately from absence. ETag text is
returned exactly after HTTP optional whitespace is removed. Last-Modified is
returned the same way and is deliberately not parsed, converted, or
normalized.

A caller-driven two-page listing uses the same tagged authentication shape:

```lua
local first, err = s3["list-objects"]({
  bucket = "results",
  prefix = "exports/",
  ["max-keys"] = 100,
  ["continuation-token"] = nil,
  auth = { tag = "sigv4", value = "results-list" },
})
expect(first ~= nil, err and err.message)

if first["is-truncated"] then
  local second, second_err = s3["list-objects"]({
    bucket = "results",
    prefix = "exports/",
    ["max-keys"] = 100,
    ["continuation-token"] = first["next-continuation-token"],
    auth = { tag = "sigv4", value = "results-list" },
  })
  expect(second ~= nil, second_err and second_err.message)
end
```

Each object has a string `key`, typed unsigned `size`, exact optional `etag`,
and unnormalized optional `last-modified`. `max-keys` is inclusive from 1
through 1000. Prefixes are nonempty and at most 1024 UTF-8 bytes;
caller-supplied continuation tokens are nonempty and at most 2048 UTF-8 bytes.

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
methods = ["GET", "HEAD"]
canonical_uri_prefixes = ["/results/"]
query = {}
header_names = []

[plugins.grants.s3.sigv4.results-list]
endpoint = "object-store"
access_key_secret = "OBJECT_STORE_ACCESS_KEY"
secret_key_secret = "OBJECT_STORE_SECRET_KEY"
region = "us-east-1"
service = "s3"
authority = "minio:9000"
methods = ["GET"]
canonical_uri_prefixes = ["/results/"]
header_names = []

[plugins.grants.s3.sigv4.results-list.query.list-type]
required = true
exact_values = ["2"]

[plugins.grants.s3.sigv4.results-list.query.max-keys]
required = true
decimal_max = 1000

[plugins.grants.s3.sigv4.results-list.query.prefix]
required = true
encoded_prefixes = ["exports%2F"]

[plugins.grants.s3.sigv4.results-list.query.continuation-token]
required = false
opaque_max_encoded_bytes = 6144
```

Sigil resolves the named secrets from the scenario environment allowlist. No
secret name or value crosses the component ABI. The host captures signing time,
constructs and signs the exact wire request, connects only through the grant's
named endpoint, sends once, and returns a read-only bounded response resource.
The plugin has no secret, clock, random, filesystem, write, redirect, retry,
or GET-fallback path.

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

GET enforces `max-bytes` up to 16 MiB. Its endpoint or signing-grant quota must
additionally cover up to 64 KiB of response framing. HEAD instead admits at
most one 32 KiB response header, stops as soon as that header is complete, and
never waits for an error body. Only status 200 succeeds. Duplicate metadata,
any Transfer-Encoding, malformed or non-ASCII headers, an oversized header,
and body bytes received with the header all fail explicitly; no partial or
default metadata is returned. A structured
S3 `RequestTimeTooSkewed` or `RequestExpired` response becomes `clock-skew`;
local signing-lease expiry and all other host transport failures become
sanitized `transport` errors. Raw upstream XML, request IDs, signature data,
and host-internal details are never returned.

LIST reads at most 4 MiB of XML plus 64 KiB of response framing, so its
network grant must allow at least `4160KiB`. It rejects malformed UTF-8, DTDs,
external or unknown entities, unknown elements, duplicate fields, invalid
nesting, count disagreements, out-of-prefix or duplicate keys, and responses
above the requested object count. No error returns a partial page. SigV4 LIST
requires Host API 1.2; its grant fixes every query field and bounds the optional
opaque token without exposing it in diagnostics.

The immutable 0.1.1 and 0.2.0 interfaces are retained byte-for-byte at
`contracts/sigil-s3-client-0.1.1.wit` and
`contracts/sigil-s3-client-0.2.0.wit`, and both are verified by `just check`.
The 0.3 interface adds only `list-objects` and Host API 1.2.

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
