# Sigil S3 plugin

`wasm.s3` is a bounded, read-only S3-compatible object client for Sigil
scenarios. Version 0.1 performs one path-style HTTP `GET` through an
operator-granted named network endpoint and returns the exact object bytes as a
binary Lua string.

The first contract deliberately supports only anonymous requests and
caller-supplied presigned query strings. It has no credential or clock import,
does not follow redirects, never chooses a socket destination, and accepts at
most 16 MiB per object. For a presigned request, `presigned-authority` carries
the exact URL authority covered by the signature while `endpoint` still names
the operator-granted socket route. Without it, the logical endpoint name remains
the HTTP `Host` authority.

```lua
local s3 = require("wasm.s3")

local bytes, err = s3["get-object"]({
  endpoint = "object-store",
  bucket = "results",
  key = "run/output.parquet",
  ["presigned-query"] = query,
  ["presigned-authority"] = "127.0.0.1:9000",
  ["max-bytes"] = 4 * 1024 * 1024,
})
expect(bytes ~= nil, err and err.message)
```

The project grants the concrete route; plugin code sees only `object-store`:

```toml
[plugins.grants.s3.network.object-store]
target = "minio:9000"
tls = "disabled"
max_bytes = "4172KiB"
```

`presigned-authority` is accepted only alongside `presigned-query`. It may be a
DNS name, IPv4 address, or bracketed IPv6 address with an optional nonzero port;
header injection, user-info, paths, and malformed ports are rejected before the
network route is opened. The field changes the HTTP `Host` header only. It
cannot redirect the TCP connection away from the route selected by the grant.
Because some HTTP servers use `Host` for tenant selection, grant a dedicated
route when the upstream does not isolate tenants independently.

The endpoint quota counts both request and response wire bytes, including HTTP
framing. Budget at least the requested object limit plus 64 KiB of response
framing and up to 12 KiB for the request (an 8 KiB presigned query,
percent-expanded key, endpoint, and HTTP framing). Reads follow `Content-Length`
or chunked framing so a small final body fragment does not reserve another large
block of quota.

The currently published immutable release is 0.1.0. It does not yet have the
separate `presigned-authority` field; build the 0.1.1 candidate from this source
tree when validating that behavior before publication.

Install the published release and add it to the current project:

```bash
sigil plugin install s3@0.1.0
sigil plugin add s3@0.1.0
```

Build, validate against the pinned SDK host WIT, and pack a local development
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
