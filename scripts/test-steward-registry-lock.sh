#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d)"
trap 'rm -rf "${temporary_directory}"' EXIT

mock_directory="${temporary_directory}/bin"
mkdir -p "${mock_directory}"
command_log="${temporary_directory}/docker.log"

cat > "${mock_directory}/docker" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '%q ' "$@" >> "${MOCK_DOCKER_LOG}"
printf '\n' >> "${MOCK_DOCKER_LOG}"

if [[ "$*" == *"imagetools create"* ]]; then
  exit 0
fi

reference="${*: -1}"
if [[ "$*" != *"imagetools inspect"* ]]; then
  echo "unexpected docker command: $*" >&2
  exit 1
fi

if [[ "$*" != *"--raw"* ]]; then
  case "${reference}" in
    registry.example.test/team-a/steward:0.2.2-apiserver)
      printf 'Name: %s\nDigest: sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\n' "${reference}"
      ;;
    registry.example.test/team-a/steward-codex:0.140.0)
      printf 'Name: %s\nDigest: sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff\n' "${reference}"
      ;;
    registry.example.test/team-a/steward:0.2.2-apiserver-amd64)
      printf 'Name: %s\nDigest: sha256:9999999999999999999999999999999999999999999999999999999999999999\n' "${reference}"
      ;;
    registry.example.test/team-a/steward-codex:0.140.0-amd64)
      printf 'Name: %s\nDigest: sha256:8888888888888888888888888888888888888888888888888888888888888888\n' "${reference}"
      ;;
    *)
      echo "unexpected digest inspection: ${reference}" >&2
      exit 1
      ;;
  esac
  exit 0
fi

case "${reference}" in
  ghcr.io/example-org/steward@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|\
  ghcr.io/example-org/steward-codex@sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd)
    cat <<'JSON'
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":101,"platform":{"architecture":"amd64","os":"linux"}},{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222","size":102,"platform":{"architecture":"arm64","os":"linux","variant":"v8"}}]}
JSON
    ;;
  registry.example.test/team-a/steward@sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee|\
  registry.example.test/team-a/steward-codex@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff)
    if [[ "${MOCK_TARGET_MISMATCH:-0}" == 1 ]]; then
      cat <<'JSON'
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":101,"platform":{"architecture":"amd64","os":"linux"}},{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:7777777777777777777777777777777777777777777777777777777777777777","size":102,"platform":{"architecture":"arm64","os":"linux","variant":"v8"}}]}
JSON
    else
      cat <<'JSON'
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":101,"platform":{"architecture":"amd64","os":"linux"}},{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222","size":102,"platform":{"architecture":"arm64","os":"linux","variant":"v8"}}]}
JSON
    fi
    ;;
  ghcr.io/example-org/steward@sha256:1111111111111111111111111111111111111111111111111111111111111111|\
  ghcr.io/example-org/steward-codex@sha256:1111111111111111111111111111111111111111111111111111111111111111|\
  registry.example.test/team-a/steward@sha256:9999999999999999999999999999999999999999999999999999999999999999|\
  registry.example.test/team-a/steward-codex@sha256:8888888888888888888888888888888888888888888888888888888888888888)
    cat <<'JSON'
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:3333333333333333333333333333333333333333333333333333333333333333","size":10},"layers":[]}
JSON
    ;;
  *)
    echo "unexpected raw inspection: ${reference}" >&2
    exit 1
    ;;
esac
MOCK
chmod +x "${mock_directory}/docker"

cat > "${temporary_directory}/handoff.json" <<'JSON'
{
  "schemaVersion": "steward.release-handoff/v1",
  "version": "0.2.2",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "images": {
    "apiserver": {
      "reference": "ghcr.io/example-org/steward:0.2.2-apiserver",
      "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
  },
  "referenceRuntimes": {
    "codex": {
      "reference": "ghcr.io/example-org/steward-codex:0.140.0",
      "digest": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
      "platforms": ["linux/amd64", "linux/arm64"]
    }
  }
}
JSON

export MOCK_DOCKER_LOG="${command_log}"
PATH="${mock_directory}:${PATH}" python3 "${root}/scripts/steward-registry-lock.py" mirror \
  --handoff "${temporary_directory}/handoff.json" \
  --target-prefix registry.example.test/team-a \
  --output "${temporary_directory}/lock-one.json"
PATH="${mock_directory}:${PATH}" python3 "${root}/scripts/steward-registry-lock.py" mirror \
  --handoff "${temporary_directory}/handoff.json" \
  --target-prefix registry.example.test/team-a \
  --output "${temporary_directory}/lock-two.json"
cmp "${temporary_directory}/lock-one.json" "${temporary_directory}/lock-two.json"

python3 - "${temporary_directory}/lock-one.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    lock = json.load(stream)
assert lock["schemaVersion"] == "steward.deployment-lock/v1"
assert lock["release"]["version"] == "0.2.2"
assert sorted(lock["artifacts"]) == ["images.apiserver", "referenceRuntimes.codex"]
image = lock["artifacts"]["images.apiserver"]
assert image["copyMode"] == "index"
assert image["target"]["digest"] == "sha256:" + "e" * 64
assert image["platforms"] == [
    {"architecture": "amd64", "digest": "sha256:" + "1" * 64, "os": "linux"},
    {"architecture": "arm64", "digest": "sha256:" + "2" * 64, "os": "linux", "variant": "v8"},
]
assert lock["chartValues"]["images"]["repository"] == "registry.example.test/team-a/steward"
assert lock["chartValues"]["images"]["apiserver"] == {
    "digest": "sha256:" + "e" * 64,
    "tag": "0.2.2-apiserver",
}
assert lock["executionBindingImages"]["codex"] == (
    "registry.example.test/team-a/steward-codex@sha256:" + "f" * 64
)
PY

if grep -Eiq 'password|token|credential|username' "${temporary_directory}/lock-one.json" "${command_log}"; then
  echo 'mirror flow recorded credential-shaped content' >&2
  exit 1
fi
grep -Fq 'imagetools create --tag registry.example.test/team-a/steward:0.2.2-apiserver ghcr.io/example-org/steward@sha256:aaaaaaaa' "${command_log}"
grep -Fq 'registry.example.test/team-a/steward@sha256:eeeeeeee' "${command_log}"

if MOCK_TARGET_MISMATCH=1 PATH="${mock_directory}:${PATH}" \
  python3 "${root}/scripts/steward-registry-lock.py" mirror \
    --handoff "${temporary_directory}/handoff.json" \
    --target-prefix registry.example.test/team-a \
    --output "${temporary_directory}/mismatch.json" >/dev/null 2>&1; then
  echo 'target index with changed platform descriptor unexpectedly passed verification' >&2
  exit 1
fi

: > "${command_log}"
PATH="${mock_directory}:${PATH}" python3 "${root}/scripts/steward-registry-lock.py" mirror \
  --handoff "${temporary_directory}/handoff.json" \
  --target-prefix registry.example.test/team-a \
  --platform linux/amd64 \
  --target-tag-suffix=-amd64 \
  --output "${temporary_directory}/single.json"
python3 - "${temporary_directory}/single.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    lock = json.load(stream)
assert lock["mode"] == "single-platform"
assert lock["requestedPlatform"] == "linux/amd64"
for artifact in lock["artifacts"].values():
    assert artifact["copyMode"] == "single-platform"
    assert artifact["sourceIndexDigest"].startswith("sha256:")
    assert artifact["platforms"] == [
        {"architecture": "amd64", "digest": "sha256:" + "1" * 64, "os": "linux"}
    ]
PY
grep -Fq -- '--prefer-index=false' "${command_log}"
grep -Fq 'ghcr.io/example-org/steward@sha256:1111111111111111' "${command_log}"

cat > "${temporary_directory}/single-handoff.json" <<'JSON'
{
  "schemaVersion": "steward.release-handoff/v1",
  "version": "0.2.2",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "images": {
    "apiserver": {
      "reference": "ghcr.io/example-org/steward:0.2.2-apiserver",
      "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111"
    }
  }
}
JSON
if PATH="${mock_directory}:${PATH}" python3 "${root}/scripts/steward-registry-lock.py" mirror \
  --handoff "${temporary_directory}/single-handoff.json" \
  --target-prefix registry.example.test/team-a \
  --output "${temporary_directory}/must-fail.json" >/dev/null 2>&1; then
  echo 'single-manifest source silently passed as a complete index' >&2
  exit 1
fi

echo 'registry mirror and deployment-lock contract passed'
