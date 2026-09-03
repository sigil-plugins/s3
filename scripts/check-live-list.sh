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
  echo "Podman or Docker is required for live LIST acceptance" >&2
  exit 2
fi
readonly ENGINE

MINIO_IMAGE="quay.io/minio/minio@sha256:a1ea29fa28355559ef137d71fc570e508a214ec84ff8083e39bc5428980b015e"
MC_IMAGE="quay.io/minio/mc@sha256:aead63c77f9db9107f1696fb08ecb0faeda23729cde94b0f663edf4fe09728e3"
readonly MINIO_IMAGE MC_IMAGE

MINIO_USER="sigillistaccess"
MINIO_PASSWORD="sigil-list-secret-2026"
WRONG_PASSWORD="definitely-wrong-list-secret"
readonly MINIO_USER MINIO_PASSWORD WRONG_PASSWORD
MINIO_ROOT_USER="$MINIO_USER"
MINIO_ROOT_PASSWORD="$MINIO_PASSWORD"
MC_HOST_live="http://$MINIO_USER:$MINIO_PASSWORD@127.0.0.1:9000"
PRESIGN_ACCESS_KEY="$MINIO_USER"
PRESIGN_SECRET_KEY="$MINIO_PASSWORD"
export MINIO_ROOT_USER MINIO_ROOT_PASSWORD MC_HOST_live
export PRESIGN_ACCESS_KEY PRESIGN_SECRET_KEY

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/sigil-s3-list.XXXXXXXX")"
mkdir -p "$ROOT/target/live-list"
EVIDENCE="$(mktemp -d "$ROOT/target/live-list/run.XXXXXXXX")"
container="sigil-s3-list-$$"
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

archive="$ROOT/dist/s3-0.3.0.sigil-plugin.tar.zst"
if [[ ! -f "$archive" ]]; then
  echo "missing candidate archive: $archive" >&2
  exit 2
fi
sha256sum "$ROOT/plugin.wasm" "$archive" >"$EVIDENCE/candidate.sha256"
b3sum "$ROOT/plugin.wasm" "$archive" >"$EVIDENCE/candidate.blake3"

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
printf 'alpha\n' >"$SCRATCH/fixtures/alpha.bin"
printf 'space payload\000tail' >"$SCRATCH/fixtures/space.bin"
printf 'unicode percent payload \377\000' >"$SCRATCH/fixtures/unicode.bin"
printf 'must not appear' >"$SCRATCH/fixtures/outside.bin"

"$ENGINE" run --rm --network "container:$container" \
  --entrypoint /bin/sh \
  -v "$SCRATCH/fixtures:/fixtures:ro" \
  --env MC_HOST_live \
  "$MC_IMAGE" -c '
    set -eu
    mc mb live/public-results live/private-results >/dev/null
    for bucket in public-results private-results; do
      mc cp /fixtures/alpha.bin "live/$bucket/exports/alpha.txt" >/dev/null
      mc cp /fixtures/space.bin "live/$bucket/exports/space name.txt" >/dev/null
      mc cp /fixtures/unicode.bin "live/$bucket/exports/unicode-é%25.bin" >/dev/null
      mc cp /fixtures/outside.bin "live/$bucket/outside/ignored.txt" >/dev/null
    done
    mc anonymous set download live/public-results >/dev/null
  ' >"$EVIDENCE/mc.setup.txt"

PRESIGNED_LIST_QUERY="$(python3 "$ROOT/scripts/presign-list.py" \
  --authority "$authority" \
  --bucket private-results \
  --prefix exports/ \
  --max-keys 100)"
PRESIGNED_AUTHORITY="$authority"
export PRESIGNED_LIST_QUERY PRESIGNED_AUTHORITY

curl --proto '=http' --silent --show-error --fail \
  "$base_url/public-results?list-type=2&max-keys=100&prefix=exports%2F" \
  >"$EVIDENCE/public-list.xml"
curl --proto '=http' --silent --show-error --fail \
  "$base_url/private-results/?$PRESIGNED_LIST_QUERY" \
  >"$EVIDENCE/private-list.xml"

python3 - "$EVIDENCE/public-list.xml" "$EVIDENCE/private-list.xml" \
  "$SCRATCH/expected.env" <<'PY'
from pathlib import Path
import shlex
import sys
import xml.etree.ElementTree as ET


def local(element: ET.Element, name: str) -> str | None:
    child = element.find(f"{{*}}{name}")
    return None if child is None else child.text


def read(path: str) -> list[dict[str, str]]:
    root = ET.fromstring(Path(path).read_bytes())
    values = []
    for item in root.findall("{*}Contents"):
        values.append(
            {
                "key": local(item, "Key") or "",
                "size": local(item, "Size") or "",
                "etag": local(item, "ETag") or "",
                "last_modified": local(item, "LastModified") or "",
            }
        )
    return values


expected_keys = [
    "exports/alpha.txt",
    "exports/space name.txt",
    "exports/unicode-é%25.bin",
]
lines = []
for label, path in (("PUBLIC", sys.argv[1]), ("PRIVATE", sys.argv[2])):
    objects = read(path)
    if [item["key"] for item in objects] != expected_keys:
        raise SystemExit(f"independent {label} listing differs: {objects!r}")
    for name, item in zip(("ALPHA", "SPACE", "UNICODE"), objects, strict=True):
        if not item["size"].isdigit() or not item["etag"] or not item["last_modified"]:
            raise SystemExit(f"independent {label} metadata is incomplete: {item!r}")
        for field in ("size", "etag", "last_modified"):
            variable = f"{label}_{name}_{field.upper()}"
            lines.append(f"export {variable}={shlex.quote(item[field])}")
Path(sys.argv[3]).write_text("\n".join(lines) + "\n", encoding="utf-8")
PY
# shellcheck source=/dev/null
source "$SCRATCH/expected.env"

mkdir -p "$SCRATCH/package/dist" "$SCRATCH/seeder/src" \
  "$SCRATCH/project/.sigil" "$SCRATCH/project/scenarios" \
  "$SCRATCH/project/probes" "$SCRATCH/data" "$SCRATCH/cache"
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

python3 - "$EVIDENCE/plugin.inspect.json" <<'PY'
import json
from pathlib import Path
import sys

inspect = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
serialized = json.dumps(inspect, sort_keys=True)
for required in ("get-object", "head-object", "list-objects"):
    if required not in serialized:
        raise SystemExit(f"candidate inspection omitted {required}")
for forbidden in ("put-object", "delete-object", "create-bucket", "delete-bucket"):
    if forbidden in serialized:
        raise SystemExit(f"candidate exposes mutation: {forbidden}")
PY

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
  "$SCRATCH/package/dist/s3-0.3.0.sigil-plugin.tar.zst" \
  github:conformance/s3 s3 0.3.0 s3-list-live-0.3.0

python3 - "$ROOT/conformance/sigil-list.toml.in" \
  "$SCRATCH/project/.sigil/sigil.toml" "$authority" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1])
destination = Path(sys.argv[2])
authority = sys.argv[3]
text = source.read_text(encoding="utf-8")
if text.count("@AUTHORITY@") != 3:
    raise SystemExit("SigV4 authority template count differs")
destination.write_text(text.replace("@AUTHORITY@", authority), encoding="utf-8")
PY
cp "$ROOT/conformance/list.sigil.lua" "$SCRATCH/project/scenarios/list.lua"
cp "$ROOT/conformance/list-route-denied.sigil.lua" "$SCRATCH/project/probes/route.lua"

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
grep -F 'list-objects' "$EVIDENCE/s3.lua" >/dev/null
grep -F 'is-truncated' "$EVIDENCE/s3.lua" >/dev/null
grep -F 'next-continuation-token' "$EVIDENCE/s3.lua" >/dev/null

OBJECT_STORE_ACCESS_KEY="$MINIO_USER"
OBJECT_STORE_SECRET_KEY="$MINIO_PASSWORD"
OBJECT_STORE_WRONG_SECRET="$WRONG_PASSWORD"
export OBJECT_STORE_ACCESS_KEY OBJECT_STORE_SECRET_KEY OBJECT_STORE_WRONG_SECRET

env_names=(
  PUBLIC_ALPHA_SIZE PUBLIC_ALPHA_ETAG PUBLIC_ALPHA_LAST_MODIFIED
  PUBLIC_SPACE_SIZE PUBLIC_SPACE_ETAG PUBLIC_SPACE_LAST_MODIFIED
  PUBLIC_UNICODE_SIZE PUBLIC_UNICODE_ETAG PUBLIC_UNICODE_LAST_MODIFIED
  PRIVATE_ALPHA_SIZE PRIVATE_ALPHA_ETAG PRIVATE_ALPHA_LAST_MODIFIED
  PRIVATE_SPACE_SIZE PRIVATE_SPACE_ETAG PRIVATE_SPACE_LAST_MODIFIED
  PRIVATE_UNICODE_SIZE PRIVATE_UNICODE_ETAG PRIVATE_UNICODE_LAST_MODIFIED
  PRESIGNED_LIST_QUERY PRESIGNED_AUTHORITY
)
env_args=()
for name in "${env_names[@]}"; do
  env_args+=(--env "$name")
done

run_sigil run scenarios \
  --endpoint "object-store=$base_url" \
  --env OBJECT_STORE_ACCESS_KEY \
  --env OBJECT_STORE_SECRET_KEY \
  --env OBJECT_STORE_WRONG_SECRET \
  "${env_args[@]}" \
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
    raise SystemExit("live LIST report summary differs")
scenario = report["scenarios"][0]
expects = scenario.get("expects", [])
if scenario.get("status") != "passed" or len(expects) != 68:
    raise SystemExit(f"live LIST scenario or assertion count differs: {len(expects)}")
if not all(expect.get("passed") is True for expect in expects):
    raise SystemExit("live LIST report contains a failed assertion")
for forbidden in (
    "sigil-list-secret-2026",
    "definitely-wrong-list-secret",
    "X-Amz-Signature=",
):
    if forbidden in serialized:
        raise SystemExit(f"live report exposed secret material: {forbidden}")
PY

if run_sigil run probes/route.lua \
  --endpoint "object-store=$base_url" \
  --env OBJECT_STORE_ACCESS_KEY \
  --env OBJECT_STORE_SECRET_KEY \
  --env OBJECT_STORE_WRONG_SECRET \
  --json >"$EVIDENCE/route-denied.json"; then
  echo "undeclared route unexpectedly passed" >&2
  exit 1
fi

if run_sigil run scenarios \
  --endpoint "object-store=$base_url" \
  --env OBJECT_STORE_ACCESS_KEY \
  --env OBJECT_STORE_WRONG_SECRET \
  "${env_args[@]}" \
  --json >"$EVIDENCE/missing-secret.json"; then
  echo "missing secret unexpectedly passed" >&2
  exit 1
fi

python3 - "$EVIDENCE/route-denied.json" "$EVIDENCE/missing-secret.json" <<'PY'
import json
from pathlib import Path
import sys

expected = ("PLUGIN_NETWORK_DENIED", "PLUGIN_SECRET_DENIED")
for path, code in zip(sys.argv[1:], expected, strict=True):
    report = json.loads(Path(path).read_text(encoding="utf-8"))
    scenarios = report.get("scenarios", [])
    if len(scenarios) != 1:
        raise SystemExit(f"negative report has the wrong scenario count: {path}")
    scenario = scenarios[0]
    failure = scenario.get("plugin_failure", {})
    if (
        report.get("status") != "failed"
        or scenario.get("failure_class") != "plugin_infrastructure"
        or failure.get("code") != code
    ):
        raise SystemExit(f"negative report differs for {code}: {scenario!r}")
PY

printf '%s\n' \
  "candidate_commit=$(git -C "$ROOT" rev-parse HEAD)" \
  "candidate_status=$(git -C "$ROOT" status --short | wc -l | tr -d ' ') paths" \
  "component_sha256=$(sha256sum "$ROOT/plugin.wasm" | cut -d' ' -f1)" \
  "component_blake3=$(b3sum "$ROOT/plugin.wasm" | cut -d' ' -f1)" \
  "package_sha256=$(sha256sum "$archive" | cut -d' ' -f1)" \
  "package_blake3=$(b3sum "$archive" | cut -d' ' -f1)" \
  "minio_image=$MINIO_IMAGE" \
  "minio_image_id=$("$ENGINE" image inspect --format '{{.Id}}' "$MINIO_IMAGE")" \
  "mc_image=$MC_IMAGE" \
  "mc_image_id=$("$ENGINE" image inspect --format '{{.Id}}' "$MC_IMAGE")" \
  "sigil_commit=$(git -C "$SIGIL_CHECKOUT" rev-parse HEAD)" \
  "sigil_binary_sha256=$(sha256sum "$SIGIL" | cut -d' ' -f1)" \
  "authority=$authority" \
  >"$EVIDENCE/identities.txt"

echo "pinned MinIO LIST acceptance passed"
