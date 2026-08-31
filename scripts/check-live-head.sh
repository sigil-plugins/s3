#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 /path/to/sigil-binary /path/to/sigil-checkout" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
SIGIL="$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")"
SIGIL_CHECKOUT="$(cd "$2" && pwd -P)"
readonly ROOT SIGIL SIGIL_CHECKOUT
if [[ ! -x "$SIGIL" || ! -f "$SIGIL_CHECKOUT/Cargo.toml" ]]; then
  echo "an executable Sigil and its exact source checkout are required" >&2
  exit 2
fi

if [[ -n "${OCI_ENGINE:-}" ]]; then
  ENGINE="$OCI_ENGINE"
elif command -v podman >/dev/null 2>&1; then
  ENGINE=podman
elif command -v docker >/dev/null 2>&1; then
  ENGINE=docker
else
  echo "Podman or Docker is required for live HEAD acceptance" >&2
  exit 2
fi
readonly ENGINE

MINIO_IMAGE="quay.io/minio/minio@sha256:a1ea29fa28355559ef137d71fc570e508a214ec84ff8083e39bc5428980b015e"
MC_IMAGE="quay.io/minio/mc@sha256:aead63c77f9db9107f1696fb08ecb0faeda23729cde94b0f663edf4fe09728e3"
EXPECTED_COMPONENT_SHA256="c64cb28374c112724b1cc0b2a0a1c627ae06ae18cdda61d5918554c3c1b7fa69"
EXPECTED_PACKAGE_SHA256="a655eda263848ab9da88ba19710fb447579ff24ed071cb696f3e80d70be7d7c2"
readonly MINIO_IMAGE MC_IMAGE EXPECTED_COMPONENT_SHA256 EXPECTED_PACKAGE_SHA256

MINIO_USER="sigilheadaccess"
MINIO_PASSWORD="sigil-head-secret-2026"
WRONG_PASSWORD="definitely-wrong-secret"
readonly MINIO_USER MINIO_PASSWORD WRONG_PASSWORD
MINIO_ROOT_USER="$MINIO_USER"
MINIO_ROOT_PASSWORD="$MINIO_PASSWORD"
MC_HOST_live="http://$MINIO_USER:$MINIO_PASSWORD@127.0.0.1:9000"
PRESIGN_ACCESS_KEY="$MINIO_USER"
PRESIGN_SECRET_KEY="$MINIO_PASSWORD"
export MINIO_ROOT_USER MINIO_ROOT_PASSWORD MC_HOST_live
export PRESIGN_ACCESS_KEY PRESIGN_SECRET_KEY

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/sigil-s3-head.XXXXXXXX")"
mkdir -p "$ROOT/target/live-head"
EVIDENCE="$(mktemp -d "$ROOT/target/live-head/run.XXXXXXXX")"
container="sigil-s3-head-$$"
readonly SCRATCH EVIDENCE container

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  set +e
  if "$ENGINE" container inspect "$container" >/dev/null 2>&1; then
    "$ENGINE" logs "$container" >"$EVIDENCE/minio.log" 2>&1
    "$ENGINE" stop --time 15 "$container" >>"$EVIDENCE/teardown.txt" 2>&1
    "$ENGINE" rm "$container" >>"$EVIDENCE/teardown.txt" 2>&1
  fi
  if "$ENGINE" container inspect "$container" >/dev/null 2>&1; then
    echo "managed MinIO container remains" >>"$EVIDENCE/teardown.txt"
    [[ $status -ne 0 ]] || status=1
  else
    echo "managed MinIO container removed" >>"$EVIDENCE/teardown.txt"
  fi
  if [[ "${KEEP_LIVE_SCRATCH:-0}" == 1 ]]; then
    echo "scratch retained: $SCRATCH" >&2
  else
    rm -r -- "$SCRATCH"
  fi
  echo "evidence: $EVIDENCE" >&2
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

sha256sum plugin.wasm dist/s3-0.2.0-rc.1.sigil-plugin.tar.zst >"$EVIDENCE/candidate.sha256"
[[ "$(sha256sum plugin.wasm | cut -d' ' -f1)" == "$EXPECTED_COMPONENT_SHA256" ]]
[[ "$(sha256sum dist/s3-0.2.0-rc.1.sigil-plugin.tar.zst | cut -d' ' -f1)" == "$EXPECTED_PACKAGE_SHA256" ]]

"$ENGINE" pull "$MINIO_IMAGE" >"$EVIDENCE/minio.pull.txt"
"$ENGINE" pull "$MC_IMAGE" >"$EVIDENCE/mc.pull.txt"
"$ENGINE" run -d --name "$container" --tmpfs /data:rw,size=64m \
  -p 127.0.0.1::9000 \
  --env MINIO_ROOT_USER \
  --env MINIO_ROOT_PASSWORD \
  "$MINIO_IMAGE" server /data --address :9000 >"$EVIDENCE/minio.container-id"

mapping="$("$ENGINE" port "$container" 9000/tcp | tail -n 1)"
if [[ ! "$mapping" =~ ^127\.0\.0\.1:([0-9]+)$ ]]; then
  echo "unexpected MinIO port mapping: $mapping" >&2
  exit 1
fi
port="${BASH_REMATCH[1]}"
authority="127.0.0.1:$port"
base_url="http://$authority"
readonly port authority base_url
for _attempt in $(seq 1 90); do
  if curl --proto '=http' --silent --show-error --fail \
    "$base_url/minio/health/ready" >/dev/null 2>&1; then
    break
  fi
  if [[ "$("$ENGINE" inspect -f '{{.State.Running}}' "$container" 2>/dev/null || true)" != true ]]; then
    echo "MinIO stopped before becoming ready" >&2
    exit 1
  fi
  sleep 1
done
curl --proto '=http' --silent --show-error --fail \
  "$base_url/minio/health/ready" >/dev/null

mkdir -p "$SCRATCH/fixtures"
: >"$SCRATCH/fixtures/zero.bin"
printf 'PAR1\000sigil-head-nonzero\377' >"$SCRATCH/fixtures/nonzero.bin"
NONZERO_SIZE="$(wc -c <"$SCRATCH/fixtures/nonzero.bin" | tr -d ' ')"
export NONZERO_SIZE

"$ENGINE" run --rm --network "container:$container" \
  --entrypoint /bin/sh \
  -v "$SCRATCH/fixtures:/fixtures:ro" \
  --env MC_HOST_live \
  "$MC_IMAGE" -c '
    set -eu
    mc mb live/public-results live/private-results >/dev/null
    mc cp /fixtures/zero.bin live/public-results/zero.bin >/dev/null
    mc cp /fixtures/nonzero.bin live/public-results/nonzero.bin >/dev/null
    mc cp /fixtures/nonzero.bin live/private-results/nonzero.bin >/dev/null
    mc anonymous set download live/public-results >/dev/null
  ' >"$EVIDENCE/mc.setup.txt"

PRESIGNED_HEAD_QUERY="$(python3 "$ROOT/scripts/presign-head.py" \
  --authority "$authority" \
  --bucket private-results \
  --key nonzero.bin)"
PRESIGNED_AUTHORITY="$authority"
export PRESIGNED_HEAD_QUERY PRESIGNED_AUTHORITY

curl --proto '=http' --silent --show-error --fail --head \
  "$base_url/public-results/zero.bin" >"$EVIDENCE/zero.headers"
curl --proto '=http' --silent --show-error --fail --head \
  "$base_url/public-results/nonzero.bin" >"$EVIDENCE/nonzero.headers"
curl --proto '=http' --silent --show-error --fail --head \
  "$base_url/private-results/nonzero.bin?$PRESIGNED_HEAD_QUERY" \
  >"$EVIDENCE/private-presigned.headers"

header_value() {
  python3 - "$1" "$2" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
name = sys.argv[2].encode("ascii").lower()
values = []
for line in path.read_bytes().split(b"\r\n"):
    if b":" not in line:
        continue
    field, value = line.split(b":", maxsplit=1)
    if field.lower() == name:
        values.append(value.strip(b" \t").decode("ascii"))
if len(values) != 1:
    raise SystemExit(f"expected exactly one {name!r} in {path}: {values!r}")
print(values[0])
PY
}

ZERO_ETAG="$(header_value "$EVIDENCE/zero.headers" etag)"
ZERO_LAST_MODIFIED="$(header_value "$EVIDENCE/zero.headers" last-modified)"
NONZERO_ETAG="$(header_value "$EVIDENCE/nonzero.headers" etag)"
NONZERO_LAST_MODIFIED="$(header_value "$EVIDENCE/nonzero.headers" last-modified)"
PRIVATE_ETAG="$(header_value "$EVIDENCE/private-presigned.headers" etag)"
PRIVATE_LAST_MODIFIED="$(header_value "$EVIDENCE/private-presigned.headers" last-modified)"
export ZERO_ETAG ZERO_LAST_MODIFIED NONZERO_ETAG NONZERO_LAST_MODIFIED
export PRIVATE_ETAG PRIVATE_LAST_MODIFIED

mkdir -p "$SCRATCH/package/dist" "$SCRATCH/seeder/src" \
  "$SCRATCH/project/.sigil" "$SCRATCH/project/scenarios" \
  "$SCRATCH/data" "$SCRATCH/cache"
cp "$ROOT/plugin.toml" "$SCRATCH/package/plugin.toml"
cp "$ROOT/plugin.wasm" "$SCRATCH/package/plugin.wasm"
python3 - "$SCRATCH/package/plugin.toml" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
old = 'source = "github:sigil-plugins/s3"'
if text.count(old) != 1:
    raise SystemExit("candidate source identity differs")
path.write_text(text.replace(old, 'source = "github:conformance/s3"'), encoding="utf-8")
PY
"$SIGIL" plugin validate "$SCRATCH/package/plugin.toml" >"$EVIDENCE/plugin.validate.txt"
"$SIGIL" plugin inspect "$SCRATCH/package/plugin.toml" --format json \
  >"$EVIDENCE/plugin.inspect.json"
"$SIGIL" plugin pack "$SCRATCH/package/plugin.toml" \
  --output-dir "$SCRATCH/package/dist" >/dev/null

cp "$ROOT/tools/sigil-compat-seed/Cargo.toml.in" "$SCRATCH/seeder/Cargo.toml"
python3 - "$SCRATCH/seeder/Cargo.toml" "$SIGIL_CHECKOUT" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
checkout = sys.argv[2].replace("\\", "\\\\").replace('"', '\\"')
text = path.read_text(encoding="utf-8")
path.write_text(text.replace("@SIGIL_CHECKOUT@", checkout), encoding="utf-8")
PY
cp "$ROOT/tools/sigil-compat-seed/main.rs" "$SCRATCH/seeder/src/main.rs"
cargo generate-lockfile --quiet --offline --manifest-path "$SCRATCH/seeder/Cargo.toml"
CARGO_TARGET_DIR="$ROOT/target/sigil-compat-seed" \
  cargo run --quiet --locked --offline --manifest-path "$SCRATCH/seeder/Cargo.toml" -- \
  "$SCRATCH/data" \
  "$SCRATCH/package/dist/s3-0.2.0-rc.1.sigil-plugin.tar.zst" \
  github:conformance/s3 s3 0.2.0-rc.1 s3-head-live-0.2.0-rc.1

python3 - "$ROOT/conformance/sigil.toml.in" "$SCRATCH/project/.sigil/sigil.toml" "$authority" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1])
destination = Path(sys.argv[2])
authority = sys.argv[3]
text = source.read_text(encoding="utf-8")
if text.count("@AUTHORITY@") != 2:
    raise SystemExit("SigV4 authority template count differs")
destination.write_text(text.replace("@AUTHORITY@", authority), encoding="utf-8")
PY
cp "$ROOT/conformance/head.sigil.lua" "$SCRATCH/project/scenarios/head.lua"

run_sigil() {
  (
    cd "$SCRATCH/project"
    SIGIL_DATA_DIR="$SCRATCH/data" SIGIL_CACHE_DIR="$SCRATCH/cache" \
      "$SIGIL" "$@"
  )
}

run_sigil plugin lock >"$EVIDENCE/plugin.lock.txt"
run_sigil generate-types >"$EVIDENCE/generate-types.txt"
cp "$SCRATCH/project/.sigil/types/wasm/s3.lua" "$EVIDENCE/s3.lua"
grep -F 'head-object' "$EVIDENCE/s3.lua" >/dev/null
grep -F 'content-length' "$EVIDENCE/s3.lua" >/dev/null
grep -F 'last-modified' "$EVIDENCE/s3.lua" >/dev/null

OBJECT_STORE_ACCESS_KEY="$MINIO_USER"
OBJECT_STORE_SECRET_KEY="$MINIO_PASSWORD"
OBJECT_STORE_WRONG_SECRET="$WRONG_PASSWORD"
export OBJECT_STORE_ACCESS_KEY OBJECT_STORE_SECRET_KEY OBJECT_STORE_WRONG_SECRET
run_sigil run scenarios \
  --endpoint "object-store=$base_url" \
  --env OBJECT_STORE_ACCESS_KEY \
  --env OBJECT_STORE_SECRET_KEY \
  --env OBJECT_STORE_WRONG_SECRET \
  --env ZERO_ETAG \
  --env ZERO_LAST_MODIFIED \
  --env NONZERO_SIZE \
  --env NONZERO_ETAG \
  --env NONZERO_LAST_MODIFIED \
  --env PRIVATE_ETAG \
  --env PRIVATE_LAST_MODIFIED \
  --env PRESIGNED_HEAD_QUERY \
  --env PRESIGNED_AUTHORITY \
  --json >"$EVIDENCE/report.json"

python3 - "$EVIDENCE/report.json" <<'PY'
import json
from pathlib import Path
import sys

report = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
serialized = json.dumps(report, sort_keys=True)
if not (
    report.get("status") == "passed"
    and report.get("total") == 1
    and report.get("passed") == 1
    and report.get("failed") == 0
    and len(report.get("scenarios", [])) == 1
):
    raise SystemExit("live HEAD report summary differs")
scenario = report["scenarios"][0]
expects = scenario.get("expects", [])
if scenario.get("status") != "passed" or len(expects) != 29:
    raise SystemExit("live HEAD scenario or assertion count differs")
if not all(expect.get("passed") is True for expect in expects):
    raise SystemExit("live HEAD report contains a failed assertion")
for forbidden in (
    "sigil-head-secret-2026",
    "definitely-wrong-secret",
    "X-Amz-Signature=",
):
    if forbidden in serialized:
        raise SystemExit(f"live report exposed secret material: {forbidden}")
PY

printf '%s\n' \
  "candidate_commit=$(git rev-parse HEAD)" \
  "component_sha256=$EXPECTED_COMPONENT_SHA256" \
  "component_blake3=$(b3sum plugin.wasm | cut -d' ' -f1)" \
  "package_sha256=$EXPECTED_PACKAGE_SHA256" \
  "package_blake3=$(b3sum dist/s3-0.2.0-rc.1.sigil-plugin.tar.zst | cut -d' ' -f1)" \
  "minio_image=$MINIO_IMAGE" \
  "minio_image_id=$("$ENGINE" image inspect --format '{{.Id}}' "$MINIO_IMAGE")" \
  "mc_image=$MC_IMAGE" \
  "mc_image_id=$("$ENGINE" image inspect --format '{{.Id}}' "$MC_IMAGE")" \
  "sigil_commit=$(git -C "$SIGIL_CHECKOUT" rev-parse HEAD)" \
  "sigil_binary_sha256=$(sha256sum "$SIGIL" | cut -d' ' -f1)" \
  "authority=$authority" \
  "nonzero_size=$NONZERO_SIZE" \
  >"$EVIDENCE/identities.txt"

echo "pinned MinIO HEAD acceptance passed"
