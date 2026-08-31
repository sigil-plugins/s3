# Pinned S3 HEAD acceptance

This gate proves the unpublished S3 0.2 component through Sigil's real plugin
store, lock, generated Lua types, component host, raw network route, and opaque
SigV4 host signer. It publishes nothing.

The matrix starts these exact Linux/amd64 images with rootless Podman or
Docker:

- `quay.io/minio/minio@sha256:a1ea29fa28355559ef137d71fc570e508a214ec84ff8083e39bc5428980b015e`
- `quay.io/minio/mc@sha256:aead63c77f9db9107f1696fb08ecb0faeda23729cde94b0f663edf4fe09728e3`

It covers anonymous zero- and nonzero-length objects, a method-specific
presigned HEAD, private host-signed HEAD, anonymous access denial, signed
not-found, and wrong-credential denial. Direct HEAD responses provide the
exact expected size, ETag, and Last-Modified values. The SigV4 grant permits
HEAD only, uses the same named loopback service as the raw route, and has no
GET authority.

Run it against the exact Sigil source and binary under test:

```sh
SIGIL=/home/bob/src/sigil/target/debug/sigil \
SIGIL_CHECKOUT=/home/bob/src/sigil \
just live-head
```

The runner uses a tmpfs for MinIO data, gives the container a unique name,
and verifies removal in its exit trap. Ignored evidence is retained under
`target/live-head/run.*`, including the generated `wasm.s3` Lua stub, exact
image and candidate identities, direct response headers, the JSON scenario
report, and teardown status.
