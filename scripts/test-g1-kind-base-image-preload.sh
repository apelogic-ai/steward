#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'find "${fixture}" -depth -delete' EXIT

image="ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
cluster="steward-g1-test-1"
node="${cluster}-control-plane"
export MOCK_CLUSTER="${cluster}" MOCK_NODE="${node}" MOCK_IMAGE="${image}"
export MOCK_LOG="${fixture}/calls" MOCK_REPO_DIGEST="${image}" MOCK_PULL_FAIL=0

cat >"${fixture}/kind" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$#" -ne 4 || "$1" != get || "$2" != nodes || "$3" != --name || "$4" != "${MOCK_CLUSTER}" ]]; then
  exit 9
fi
printf '%s\n' "${MOCK_NODE}"
SH
cat >"${fixture}/docker" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${MOCK_LOG}"
if [[ "$#" -lt 5 || "$1" != exec || "$2" != "${MOCK_CLUSTER}-control-plane" || "$3" != crictl ]]; then
  exit 9
fi
case "$4" in
  pull)
    [[ "$#" -eq 5 && "$5" == "${MOCK_IMAGE}" && "${MOCK_PULL_FAIL}" == 0 ]]
    ;;
  inspecti)
    [[ "$#" -eq 7 && "$5" == --output && "$6" == json && "$7" == "${MOCK_IMAGE}" ]]
    printf '{"status":{"repoDigests":["%s"]}}\n' "${MOCK_REPO_DIGEST}"
    ;;
  *) exit 9 ;;
esac
SH
chmod +x "${fixture}/kind" "${fixture}/docker"
export PATH="${fixture}:${PATH}"

bash "${root}/scripts/g1-preload-kind-base-image.sh" "${cluster}" "${image}"
[[ "$(wc -l <"${MOCK_LOG}" | tr -d ' ')" == 2 ]]

: >"${MOCK_LOG}"
if bash "${root}/scripts/g1-preload-kind-base-image.sh" "${cluster}" "${image%@*}:latest" >/dev/null 2>&1; then
  echo "G-1 preload accepted a mutable image reference" >&2
  exit 1
fi
[[ ! -s "${MOCK_LOG}" ]]

export MOCK_NODE="steward-g1-other-control-plane"
if bash "${root}/scripts/g1-preload-kind-base-image.sh" "${cluster}" "${image}" >/dev/null 2>&1; then
  echo "G-1 preload accepted a different Kind node" >&2
  exit 1
fi
[[ ! -s "${MOCK_LOG}" ]]
export MOCK_NODE="${node}"

export MOCK_REPO_DIGEST="ghcr.io/nvidia/openshell-community/sandboxes/base@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
if bash "${root}/scripts/g1-preload-kind-base-image.sh" "${cluster}" "${image}" >/dev/null 2>&1; then
  echo "G-1 preload accepted a mismatched cached digest" >&2
  exit 1
fi
[[ "$(wc -l <"${MOCK_LOG}" | tr -d ' ')" == 2 ]]

: >"${MOCK_LOG}"
export MOCK_PULL_FAIL=1
if bash "${root}/scripts/g1-preload-kind-base-image.sh" "${cluster}" "${image}" >/dev/null 2>&1; then
  echo "G-1 preload ignored a failed node-local pull" >&2
  exit 1
fi
[[ "$(wc -l <"${MOCK_LOG}" | tr -d ' ')" == 1 ]]

echo "G-1 owned Kind-node preload regression checks passed"
