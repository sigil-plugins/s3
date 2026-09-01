return {
  title = "S3 list cannot select an undeclared route",
  priority = "P0",
  policy = { capabilities = { "wasm.s3" } },

  run = function()
    local s3 = require("wasm.s3")
    s3["list-objects"]({
      bucket = "public-results",
      prefix = "exports/",
      ["max-keys"] = 2,
      ["continuation-token"] = nil,
      auth = {
        tag = "anonymous",
        value = { endpoint = "undeclared-route" },
      },
    })
    expect(false, "an undeclared route must never return")
  end,
}
