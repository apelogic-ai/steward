#!/usr/bin/env bash
set -euo pipefail

profile_bundle="${1:?provider-profile bundle archive is required}"
release="${STEWARD_OPEN_SHELL_RELEASE:?STEWARD_OPEN_SHELL_RELEASE is required}"
cli="${STEWARD_RUN_DIR}/openshell"
workspace="codex-reference"
sandbox="codex-version"

cleanup() {
  status="$?"
  trap - EXIT INT TERM
  if [[ -x "${cli}" ]]; then
    "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
      --workspace "${workspace}" sandbox delete "${sandbox}" >/dev/null 2>&1 || true
    "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
      workspace delete "${workspace}" >/dev/null 2>&1 || true
  fi
  exit "${status}"
}
trap cleanup EXIT INT TERM

print_failure_diagnostics() {
  echo "OpenShell sandbox diagnostics:" >&2
  kubectl \
    --kubeconfig "${STEWARD_TEST_KUBECONFIG}" \
    --context "${STEWARD_TEST_KUBE_CONTEXT}" \
    -n openshell get pods \
    -l openshell.ai/managed-by=openshell \
    -o 'custom-columns=NAME:.metadata.name,PHASE:.status.phase,READY:.status.containerStatuses[*].ready,RESTARTS:.status.containerStatuses[*].restartCount,WAITING:.status.containerStatuses[*].state.waiting.reason,TERMINATED:.status.containerStatuses[*].state.terminated.reason' \
    >&2 || true
  kubectl \
    --kubeconfig "${STEWARD_TEST_KUBECONFIG}" \
    --context "${STEWARD_TEST_KUBE_CONTEXT}" \
    -n openshell get events \
    --sort-by=.lastTimestamp 2>&1 | tail -n 40 >&2 || true

  local pod=""
  while IFS= read -r pod; do
    [[ -n "${pod}" ]] || continue
    echo "Last 80 lines from ${pod} agent container:" >&2
    kubectl \
      --kubeconfig "${STEWARD_TEST_KUBECONFIG}" \
      --context "${STEWARD_TEST_KUBE_CONTEXT}" \
      -n openshell logs "${pod}" -c agent --tail=80 >&2 || true
  done < <(
    kubectl \
      --kubeconfig "${STEWARD_TEST_KUBECONFIG}" \
      --context "${STEWARD_TEST_KUBE_CONTEXT}" \
      -n openshell get pods \
      -l openshell.ai/managed-by=openshell \
      -o name 2>/dev/null || true
  )
}

case "$(uname -s):$(uname -m)" in
  Linux:x86_64 | Linux:amd64) target="x86_64-unknown-linux-musl" ;;
  *) echo "released Codex reference runtime conformance requires linux/amd64" >&2; exit 2 ;;
esac

archive="openshell-${target}.tar.gz"
curl -fsSL --retry 4 --retry-delay 2 --retry-all-errors \
  "https://github.com/NVIDIA/OpenShell/releases/download/${release}/${archive}" \
  -o "${STEWARD_RUN_DIR}/${archive}"
curl -fsSL --retry 4 --retry-delay 2 --retry-all-errors \
  "https://github.com/NVIDIA/OpenShell/releases/download/${release}/openshell-checksums-sha256.txt" \
  -o "${STEWARD_RUN_DIR}/openshell-checksums-sha256.txt"
(
  cd "${STEWARD_RUN_DIR}"
  grep " ${archive}$" openshell-checksums-sha256.txt | sha256sum -c -
  tar -xzf "${archive}"
)

bundle_root="${STEWARD_RUN_DIR}/released-provider-profile"
rendered_profiles="${STEWARD_RUN_DIR}/rendered-provider-profiles"
mkdir -p "${bundle_root}"
tar -xzf "${profile_bundle}" -C "${bundle_root}"
bundle="${bundle_root}/provider-profile-bundle/v1.2.0"
"${bundle}/bin/steward-provider-profile" install \
  --bundle "${bundle}" \
  --inputs "${bundle}/examples/inputs.json" \
  --output "${rendered_profiles}" >/dev/null

"${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  settings set --global --key providers_v2_enabled --value true --yes
for profile in steward-litellm steward-mcp-gw; do
  "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    provider profile lint --global -f "${rendered_profiles}/profiles/${profile}.json"
  "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    provider profile import --global -f "${rendered_profiles}/profiles/${profile}.json"
done

"${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  workspace create --name "${workspace}"
for provider in steward-litellm steward-mcp-gw; do
  "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    --workspace "${workspace}" provider create \
    --name "${provider}" \
    --type "${provider}" \
    --runtime-credentials \
    --global-profile
done

policy="${STEWARD_RUN_DIR}/codex-reference-policy.yaml"
cat >"${policy}" <<'YAML'
version: 1
filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /proc, /dev/urandom, /etc]
  read_write: [/sandbox, /tmp, /dev/null]
landlock:
  compatibility: best_effort
process:
  run_as_user: sandbox
  run_as_group: sandbox
network_policies: {}
YAML

if ! "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  --workspace "${workspace}" sandbox create \
  --name "${sandbox}" \
  --from "${STEWARD_OPENSHELL_SANDBOX_IMAGE:?run-owned sandbox image is required}" \
  --policy "${policy}" \
  --provider steward-litellm \
  --provider steward-mcp-gw \
  --no-auto-providers \
  --no-tty \
  -- /usr/bin/codex --version
then
  # Pinned OpenShell may race its initial command against sandbox readiness.
  :
fi

actual_version=""
last_exec_error="${STEWARD_RUN_DIR}/codex-reference-last-exec.stderr"
for attempt in {1..120}; do
  if actual_version="$(
    "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
      --workspace "${workspace}" sandbox exec --name "${sandbox}" --no-tty -- \
      /usr/bin/codex --version 2>"${last_exec_error}"
  )"
  then
    break
  fi
  if [[ "${attempt}" == 120 ]]; then
    echo "Codex reference runtime did not become ready under its released OpenShell profiles" >&2
    tail -n 40 "${last_exec_error}" >&2 || true
    print_failure_diagnostics
    exit 1
  fi
  sleep 1
done
if [[ "${actual_version}" != "codex-cli 0.140.0" ]]; then
  echo "Codex reference runtime returned unexpected version: ${actual_version}" >&2
  exit 1
fi

echo "Codex reference runtime started under both released OpenShell profiles"
