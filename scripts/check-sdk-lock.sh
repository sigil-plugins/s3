#!/usr/bin/env bash
set -euo pipefail

repository="$(sed -n 's/^repository = "\([^"]*\)"$/\1/p' SDK.lock)"
commit="$(sed -n 's/^commit = "\([0-9a-f]*\)"$/\1/p' SDK.lock)"
expected_host_1_0="$(sed -n 's/^host_1_0_wit_sha256 = "\([0-9a-f]*\)"$/\1/p' SDK.lock)"
expected_host_1_1="$(sed -n 's/^host_1_1_wit_sha256 = "\([0-9a-f]*\)"$/\1/p' SDK.lock)"
expected_host_1_2="$(sed -n 's/^host_1_2_wit_sha256 = "\([0-9a-f]*\)"$/\1/p' SDK.lock)"
expected_s3_0_1_1="$(sed -n 's/^s3_0_1_1_wit_sha256 = "\([0-9a-f]*\)"$/\1/p' SDK.lock)"
expected_s3_0_2_0="$(sed -n 's/^s3_0_2_0_wit_sha256 = "\([0-9a-f]*\)"$/\1/p' SDK.lock)"

if [[ -n "${SDK_CHECKOUT:-}" ]]; then
  checkout="$SDK_CHECKOUT"
  temporary=""
else
  temporary="$(mktemp -d)"
  checkout="$temporary/sdk"
  git clone --quiet "https://github.com/${repository#github:}.git" "$checkout"
  git -C "$checkout" checkout --quiet --detach "$commit"
fi

cleanup() {
  if [[ -n "$temporary" && -d "$temporary" && ! -L "$temporary" ]]; then
    rm -rf -- "$temporary"
  fi
}
trap cleanup EXIT

test "$(git -C "$checkout" rev-parse HEAD)" = "$commit"
echo "$expected_host_1_0  $checkout/wit/sigil-host/1.0.0/host.wit" | sha256sum --check --strict
echo "$expected_host_1_1  $checkout/wit/sigil-host/1.1.0/host.wit" | sha256sum --check --strict
echo "$expected_host_1_2  $checkout/wit/sigil-host/1.2.0/host.wit" | sha256sum --check --strict
cmp --silent "$checkout/wit/sigil-host/1.0.0/host.wit" wit/deps/sigil-host/host.wit
cmp --silent "$checkout/wit/sigil-host/1.2.0/host.wit" wit/deps/sigil-host-sigv4/host.wit
echo "$expected_s3_0_1_1  contracts/sigil-s3-client-0.1.1.wit" | sha256sum --check --strict
echo "$expected_s3_0_2_0  contracts/sigil-s3-client-0.2.0.wit" | sha256sum --check --strict
echo "SDK revision, vendored host 1.0/1.2 WIT, and frozen S3 0.1.1/0.2.0 WIT are exact"
