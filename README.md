# Sigil S3 plugin

`wasm.s3` is a bounded, read-only S3-compatible object client for Sigil
scenarios. Version 0.1 performs one path-style HTTP `GET` through an
operator-granted named network endpoint and returns the exact object bytes as a
binary Lua string.

The first contract deliberately supports only anonymous requests and
caller-supplied presigned query strings. It has no credential or clock import,
does not follow redirects, never chooses a host or port, and accepts at most 16
MiB per object. The logical endpoint name is also the HTTP `Host` authority, so
presigned URLs must be generated for that in-environment name.

```lua
local s3 = require("wasm.s3")

local bytes, err = s3["get-object"]({
  endpoint = "minio",
  bucket = "results",
  key = "run/output.parquet",
  ["max-bytes"] = 4 * 1024 * 1024,
})
expect(bytes ~= nil, err and err.message)
```

The project grants the concrete route; plugin code sees only `minio`:

```toml
[plugins.grants.s3.network.minio]
target = "minio:9000"
tls = "disabled"
max_bytes = "4MiB"
```

Build, validate against the pinned SDK host WIT, and pack a local candidate:

```bash
just check
just dist
```

`SDK_CHECKOUT=/path/to/sigil-plugin-sdk just check` verifies against a local
SDK checkout; otherwise the exact commit in `SDK.lock` is fetched and compared.

This repository is a local candidate until its source, exact component/package
bytes, release policy, and official namespace publication receive the separate
human gate required by Sigil's immutable plugin process.
