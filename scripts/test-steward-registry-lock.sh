#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary_directory="$(mktemp -d)"
cleanup() {
  status="$?"
  trap - EXIT
  rm -rf "${temporary_directory}"
  exit "${status}"
}
trap cleanup EXIT

mock_directory="${temporary_directory}/bin"
mkdir -p "${mock_directory}"
command_log="${temporary_directory}/oras.log"
copy_state="${temporary_directory}/copied.txt"
: >"${copy_state}"

cat >"${mock_directory}/oras" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
printf '%q ' "$@" >>"${MOCK_ORAS_LOG}"
printf '\n' >>"${MOCK_ORAS_LOG}"

case "${1:-} ${2:-}" in
  'repo tags')
    printf '[]\n'
    ;;
  'manifest fetch')
    reference="${*: -1}"
    platform=''
    previous=''
    for argument in "$@"; do
      if [[ "${previous}" == --platform ]]; then
        platform="${argument}"
      fi
      previous="${argument}"
    done
    case "${reference}" in
      ghcr.io/example-org/charts/steward:0.2.4@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc)
        digest='sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc'
        ;;
      ghcr.io/example-org/steward:0.2.4-apiserver@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)
        if [[ -n "${platform}" ]]; then
          digest='sha256:1111111111111111111111111111111111111111111111111111111111111111'
        else
          digest='sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
        fi
        ;;
      ghcr.io/example-org/steward-codex:0.140.0@sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd)
        if [[ -n "${platform}" ]]; then
          digest='sha256:2222222222222222222222222222222222222222222222222222222222222222'
        else
          digest='sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd'
        fi
        ;;
      *)
        digest="$(awk -F '|' -v reference="${reference}" '$1 == reference {print $2}' "${MOCK_ORAS_STATE}" | tail -n 1)"
        [[ -n "${digest}" ]] || exit 1
        ;;
    esac
    jq -cn --arg digest "${digest}" '{mediaType:"application/vnd.oci.image.manifest.v1+json",digest:$digest,size:100}'
    ;;
  'cp --recursive')
    source_reference="${*: -2:1}"
    target_reference="${*: -1}"
    digest="${source_reference##*@}"
    printf '%s|%s\n' "${target_reference}" "${digest}" >>"${MOCK_ORAS_STATE}"
    ;;
  *)
    printf 'unexpected oras command: %s\n' "$*" >&2
    exit 1
    ;;
esac
MOCK
chmod +x "${mock_directory}/oras"

cat >"${temporary_directory}/handoff.json" <<'JSON'
{
  "schemaVersion": "steward.release-handoff/v1",
  "version": "0.2.4",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "chart": {
    "reference": "oci://ghcr.io/example-org/charts/steward:0.2.4",
    "digest": "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
  },
  "images": {
    "apiserver": {
      "reference": "ghcr.io/example-org/steward:0.2.4-apiserver",
      "digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
  },
  "referenceRuntimes": {
    "codex": {
      "reference": "ghcr.io/example-org/steward-codex:0.140.0",
      "digest": "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
    }
  }
}
JSON

cat >"${temporary_directory}/mappings.json" <<'JSON'
{
  "schemaVersion": "steward.registry-mappings/v1",
  "artifacts": {
    "chart": "registry.example.test/team-a/charts/steward:0.2.4",
    "images.apiserver": "registry.example.test/team-a/steward:0.2.4-apiserver",
    "referenceRuntimes.codex": "registry.example.test/team-a/steward-codex:0.140.0"
  }
}
JSON

export MOCK_ORAS_LOG="${command_log}"
export MOCK_ORAS_STATE="${copy_state}"
PATH="${mock_directory}:${PATH}" "${root}/scripts/steward-registry-lock.sh" plan \
  --handoff "${temporary_directory}/handoff.json" \
  --mappings "${temporary_directory}/mappings.json" \
  --output "${temporary_directory}/plan-one.json"
PATH="${mock_directory}:${PATH}" "${root}/scripts/steward-registry-lock.sh" plan \
  --handoff "${temporary_directory}/handoff.json" \
  --mappings "${temporary_directory}/mappings.json" \
  --output "${temporary_directory}/plan-two.json"
cmp "${temporary_directory}/plan-one.json" "${temporary_directory}/plan-two.json"

jq -e '
  .schemaVersion == "steward.registry-plan/v1" and
  .platform == null and
  (.artifacts | keys) == ["chart", "images.apiserver", "referenceRuntimes.codex"] and
  .artifacts.chart.target.reference == "registry.example.test/team-a/charts/steward:0.2.4"
' "${temporary_directory}/plan-one.json" >/dev/null
if grep -Fq 'cp --recursive' "${command_log}"; then
  echo 'no-write plan unexpectedly copied an artifact' >&2
  exit 1
fi

PATH="${mock_directory}:${PATH}" "${root}/scripts/steward-registry-lock.sh" mirror \
  --plan "${temporary_directory}/plan-one.json" \
  --output "${temporary_directory}/lock-one.json" >"${temporary_directory}/result-one.jsonl"
PATH="${mock_directory}:${PATH}" "${root}/scripts/steward-registry-lock.sh" mirror \
  --plan "${temporary_directory}/plan-one.json" \
  --output "${temporary_directory}/lock-two.json" >"${temporary_directory}/result-two.jsonl"
cmp "${temporary_directory}/lock-one.json" "${temporary_directory}/lock-two.json"

jq -s -e 'length == 3 and all(.status == "copied")' "${temporary_directory}/result-one.jsonl" >/dev/null
jq -s -e 'length == 3 and all(.status == "already-present")' "${temporary_directory}/result-two.jsonl" >/dev/null
jq -e '
  .schemaVersion == "steward.deployment-lock/v1" and
  .artifacts.chart.source.digest == .artifacts.chart.target.digest and
  .artifacts.chart.flux.ociRepository.url == "oci://registry.example.test/team-a/charts/steward" and
  .artifacts.chart.flux.ociRepository.ref.digest == "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc" and
  .chartValues.images.repository == "registry.example.test/team-a/steward" and
  .chartValues.images.apiserver.tag == "0.2.4-apiserver" and
  .executionBindingImages.codex == "registry.example.test/team-a/steward-codex@sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
' "${temporary_directory}/lock-one.json" >/dev/null

: >"${command_log}"
: >"${copy_state}"
PATH="${mock_directory}:${PATH}" "${root}/scripts/steward-registry-lock.sh" plan \
  --handoff "${temporary_directory}/handoff.json" \
  --mappings "${temporary_directory}/mappings.json" \
  --platform linux/amd64 \
  --output "${temporary_directory}/platform-plan.json"
jq -e '
  .platform == "linux/amd64" and
  .artifacts.chart.source.digest == .artifacts.chart.source.releaseDigest and
  .artifacts["images.apiserver"].source.digest != .artifacts["images.apiserver"].source.releaseDigest and
  .artifacts["referenceRuntimes.codex"].source.digest != .artifacts["referenceRuntimes.codex"].source.releaseDigest
' "${temporary_directory}/platform-plan.json" >/dev/null
grep -Fq -- '--platform linux/amd64' "${command_log}"

cat >"${temporary_directory}/bad-mappings.json" <<'JSON'
{"schemaVersion":"steward.registry-mappings/v1","artifacts":{"chart":"registry.example.test/team-a/charts/steward:0.2.4"}}
JSON
if PATH="${mock_directory}:${PATH}" "${root}/scripts/steward-registry-lock.sh" plan \
  --handoff "${temporary_directory}/handoff.json" \
  --mappings "${temporary_directory}/bad-mappings.json" \
  --output "${temporary_directory}/bad-plan.json" >/dev/null 2>&1; then
  echo 'incomplete explicit mappings unexpectedly passed' >&2
  exit 1
fi

if grep -Eiq 'password|token|credential|username' \
  "${temporary_directory}/plan-one.json" "${temporary_directory}/lock-one.json"; then
  echo 'mirror plan or deployment lock recorded credential-shaped content' >&2
  exit 1
fi

echo 'daemonless registry plan, mirror, and deployment-lock contract passed'
