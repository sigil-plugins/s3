local function require_metadata(value, err)
  expect(value ~= nil, err and err.message)
  return value
end

local function expect_exact(metadata, size, etag, last_modified)
  expect(type(metadata["content-length"]) == "number")
  expect(metadata["content-length"] == size)
  expect(metadata.etag == etag)
  expect(metadata["last-modified"] == last_modified)
end

return {
  title = "S3 HEAD is bounded and auth-compatible",
  priority = "P0",
  policy = { capabilities = { "wasm.s3" } },

  run = function()
    local s3 = require("wasm.s3")
    local anonymous = {
      tag = "anonymous",
      value = { endpoint = "object-store" },
    }
    local private_sigv4 = { tag = "sigv4", value = "private-read" }

    local zero, zero_err = s3["head-object"]({
      bucket = "public-results",
      key = "zero.bin",
      auth = anonymous,
    })
    zero = require_metadata(zero, zero_err)
    expect_exact(
      zero,
      0,
      sigil.env("ZERO_ETAG"),
      sigil.env("ZERO_LAST_MODIFIED")
    )

    local public, public_err = s3["head-object"]({
      bucket = "public-results",
      key = "nonzero.bin",
      auth = anonymous,
    })
    public = require_metadata(public, public_err)
    expect_exact(
      public,
      tonumber(sigil.env("NONZERO_SIZE")),
      sigil.env("NONZERO_ETAG"),
      sigil.env("NONZERO_LAST_MODIFIED")
    )

    local presigned, presigned_err = s3["head-object"]({
      bucket = "private-results",
      key = "nonzero.bin",
      auth = {
        tag = "presigned",
        value = {
          endpoint = "object-store",
          query = sigil.env("PRESIGNED_HEAD_QUERY"),
          authority = sigil.env("PRESIGNED_AUTHORITY"),
        },
      },
    })
    presigned = require_metadata(presigned, presigned_err)
    expect_exact(
      presigned,
      tonumber(sigil.env("NONZERO_SIZE")),
      sigil.env("PRIVATE_ETAG"),
      sigil.env("PRIVATE_LAST_MODIFIED")
    )

    local private, private_err = s3["head-object"]({
      bucket = "private-results",
      key = "nonzero.bin",
      auth = private_sigv4,
    })
    private = require_metadata(private, private_err)
    expect_exact(
      private,
      tonumber(sigil.env("NONZERO_SIZE")),
      sigil.env("PRIVATE_ETAG"),
      sigil.env("PRIVATE_LAST_MODIFIED")
    )

    local anonymous_private, anonymous_private_err = s3["head-object"]({
      bucket = "private-results",
      key = "nonzero.bin",
      auth = anonymous,
    })
    expect(anonymous_private == nil)
    expect(anonymous_private_err.class == "denied")
    expect(anonymous_private_err.status == 403)

    local missing, missing_err = s3["head-object"]({
      bucket = "private-results",
      key = "missing.bin",
      auth = private_sigv4,
    })
    expect(missing == nil)
    expect(missing_err.class == "not-found")
    expect(missing_err.status == 404)

    local wrong, wrong_err = s3["head-object"]({
      bucket = "private-results",
      key = "nonzero.bin",
      auth = { tag = "sigv4", value = "wrong-private-read" },
    })
    expect(wrong == nil)
    expect(wrong_err.class == "denied")
    expect(wrong_err.status == 403)
  end,
}
