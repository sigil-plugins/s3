local function require_page(value, err)
  expect(value ~= nil, err and err.message)
  return value
end

local function expect_object(object, key, size, etag, last_modified)
  expect(object.key == key)
  expect(type(object.size) == "number")
  expect(object.size == size)
  expect(object.etag == etag)
  expect(object["last-modified"] == last_modified)
end

local function expected(name, field)
  return sigil.env(name .. "_" .. field)
end

local function expect_three(page, name)
  expect(#page.objects == 3)
  expect(page["is-truncated"] == false)
  expect(page["next-continuation-token"] == nil)
  expect_object(
    page.objects[1],
    "exports/alpha.txt",
    tonumber(expected(name, "ALPHA_SIZE")),
    expected(name, "ALPHA_ETAG"),
    expected(name, "ALPHA_LAST_MODIFIED")
  )
  expect_object(
    page.objects[2],
    "exports/space name.txt",
    tonumber(expected(name, "SPACE_SIZE")),
    expected(name, "SPACE_ETAG"),
    expected(name, "SPACE_LAST_MODIFIED")
  )
  expect_object(
    page.objects[3],
    "exports/unicode-é%25.bin",
    tonumber(expected(name, "UNICODE_SIZE")),
    expected(name, "UNICODE_ETAG"),
    expected(name, "UNICODE_LAST_MODIFIED")
  )
end

return {
  title = "S3 listing is one typed caller-driven page",
  priority = "P0",
  policy = { capabilities = { "wasm.s3" } },

  run = function()
    local s3 = require("wasm.s3")
    local anonymous = {
      tag = "anonymous",
      value = { endpoint = "object-store" },
    }
    local private_sigv4 = { tag = "sigv4", value = "private-list" }

    local public, public_err = s3["list-objects"]({
      bucket = "public-results",
      prefix = "exports/",
      ["max-keys"] = 100,
      ["continuation-token"] = nil,
      auth = anonymous,
    })
    expect_three(require_page(public, public_err), "PUBLIC")

    local presigned, presigned_err = s3["list-objects"]({
      bucket = "private-results",
      prefix = "exports/",
      ["max-keys"] = 100,
      ["continuation-token"] = nil,
      auth = {
        tag = "presigned",
        value = {
          endpoint = "object-store",
          query = sigil.env("PRESIGNED_LIST_QUERY"),
          authority = sigil.env("PRESIGNED_AUTHORITY"),
        },
      },
    })
    expect_three(require_page(presigned, presigned_err), "PRIVATE")

    local first, first_err = s3["list-objects"]({
      bucket = "private-results",
      prefix = "exports/",
      ["max-keys"] = 2,
      ["continuation-token"] = nil,
      auth = private_sigv4,
    })
    first = require_page(first, first_err)
    expect(#first.objects == 2)
    expect(first["is-truncated"] == true)
    expect(type(first["next-continuation-token"]) == "string")
    expect(#first["next-continuation-token"] > 0)
    expect_object(
      first.objects[1],
      "exports/alpha.txt",
      tonumber(expected("PRIVATE", "ALPHA_SIZE")),
      expected("PRIVATE", "ALPHA_ETAG"),
      expected("PRIVATE", "ALPHA_LAST_MODIFIED")
    )
    expect_object(
      first.objects[2],
      "exports/space name.txt",
      tonumber(expected("PRIVATE", "SPACE_SIZE")),
      expected("PRIVATE", "SPACE_ETAG"),
      expected("PRIVATE", "SPACE_LAST_MODIFIED")
    )

    local second, second_err = s3["list-objects"]({
      bucket = "private-results",
      prefix = "exports/",
      ["max-keys"] = 2,
      ["continuation-token"] = first["next-continuation-token"],
      auth = private_sigv4,
    })
    second = require_page(second, second_err)
    expect(#second.objects == 1)
    expect(second["is-truncated"] == false)
    expect(second["next-continuation-token"] == nil)
    expect_object(
      second.objects[1],
      "exports/unicode-é%25.bin",
      tonumber(expected("PRIVATE", "UNICODE_SIZE")),
      expected("PRIVATE", "UNICODE_ETAG"),
      expected("PRIVATE", "UNICODE_LAST_MODIFIED")
    )

    local wrong, wrong_err = s3["list-objects"]({
      bucket = "private-results",
      prefix = "exports/",
      ["max-keys"] = 2,
      ["continuation-token"] = nil,
      auth = { tag = "sigv4", value = "wrong-private-list" },
    })
    expect(wrong == nil)
    expect(wrong_err.class == "denied")
    expect(wrong_err.status == 403)

    local missing, missing_err = s3["list-objects"]({
      bucket = "missing-results",
      prefix = "exports/",
      ["max-keys"] = 2,
      ["continuation-token"] = nil,
      auth = { tag = "sigv4", value = "missing-list" },
    })
    expect(missing == nil)
    expect(missing_err.class == "not-found")
    expect(missing_err.status == 404)
  end,
}
