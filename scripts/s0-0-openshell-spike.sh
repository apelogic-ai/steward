#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_ID="${STEWARD_RUN_ID:-$(date -u +%Y%m%d%H%M%S)-$$}"
if [[ ! "${RUN_ID}" =~ ^[a-z0-9-]+$ ]]; then
  echo "STEWARD_RUN_ID must contain only lowercase ASCII letters, digits, and hyphens" >&2
  exit 2
fi

CLUSTER_NAME="steward-${RUN_ID}"
KUBE_CONTEXT="kind-${CLUSTER_NAME}"
RUN_DIR="${ROOT}/.steward-run/${RUN_ID}"
KUBECONFIG_PATH="${RUN_DIR}/kubeconfig"
PORT_FORWARD_LOG="${RUN_DIR}/openshell-port-forward.log"
PORT_FORWARD_PID=""
CLUSTER_CREATED=0

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ -n "${PORT_FORWARD_PID}" ]]; then
    kill "${PORT_FORWARD_PID}" >/dev/null 2>&1 || true
    wait "${PORT_FORWARD_PID}" >/dev/null 2>&1 || true
  fi
  if [[ "${CLUSTER_CREATED}" == "1" && "${STEWARD_DEV_KEEP:-0}" != "1" ]]; then
    kind delete cluster --name "${CLUSTER_NAME}" >/dev/null 2>&1 || true
  fi
  if [[ "${STEWARD_DEV_KEEP:-0}" == "1" ]]; then
    echo "kept run ${RUN_ID}; clean it with: kind delete cluster --name ${CLUSTER_NAME}" >&2
  else
    find "${RUN_DIR}" -depth -delete 2>/dev/null || true
  fi
  exit "${status}"
}
trap cleanup EXIT INT TERM

case "$(uname -s):$(uname -m)" in
  Darwin:arm64)
    openshell_cli_target="aarch64-apple-darwin"
    ;;
  Linux:arm64 | Linux:aarch64)
    openshell_cli_target="aarch64-unknown-linux-musl"
    ;;
  Linux:x86_64 | Linux:amd64)
    openshell_cli_target="x86_64-unknown-linux-musl"
    ;;
  *)
    echo "unsupported OpenShell CLI platform: $(uname -s) $(uname -m)" >&2
    exit 2
    ;;
esac
openshell_cli_archive="openshell-${openshell_cli_target}.tar.gz"
if [[ "$#" -eq 1 && "$1" == "--print-openshell-cli-asset" ]]; then
  echo "${openshell_cli_archive}"
  exit 0
fi

for command in kind kubectl helm cargo curl openssl sed tar; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

if command -v sha256sum >/dev/null 2>&1; then
  checksum_command=(sha256sum -c -)
elif command -v shasum >/dev/null 2>&1; then
  checksum_command=(shasum -a 256 -c -)
else
  echo "required command is missing: sha256sum or shasum" >&2
  exit 2
fi

mkdir -p "${RUN_DIR}"
kind create cluster \
  --name "${CLUSTER_NAME}" \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --wait 120s
CLUSTER_CREATED=1

supervisor_image_args=()
if [[ -n "${STEWARD_OPENSHELL_SUPERVISOR_IMAGE:-}" ]]; then
  if [[ "${STEWARD_OPENSHELL_SUPERVISOR_IMAGE}" != *:* || "${STEWARD_OPENSHELL_SUPERVISOR_IMAGE}" == *@* ]]; then
    echo "STEWARD_OPENSHELL_SUPERVISOR_IMAGE must be an explicit repository:tag reference" >&2
    exit 2
  fi
  supervisor_repository="${STEWARD_OPENSHELL_SUPERVISOR_IMAGE%:*}"
  supervisor_tag="${STEWARD_OPENSHELL_SUPERVISOR_IMAGE##*:}"
  kind load docker-image \
    "${STEWARD_OPENSHELL_SUPERVISOR_IMAGE}" \
    --name "${CLUSTER_NAME}"
  supervisor_image_args=(
    --set-string "supervisor.image.repository=${supervisor_repository}"
    --set-string "supervisor.image.tag=${supervisor_tag}"
    --set-string "supervisor.image.pullPolicy=IfNotPresent"
  )
fi

actual_context="$(
  kubectl --kubeconfig "${KUBECONFIG_PATH}" config current-context
)"
if [[ "${actual_context}" != "${KUBE_CONTEXT}" ]]; then
  echo "created context ${actual_context}, expected ${KUBE_CONTEXT}" >&2
  exit 1
fi

agent_sandbox_base="https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v0.5.0"
kubectl \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --context "${KUBE_CONTEXT}" \
  apply -f "${agent_sandbox_base}/manifest.yaml"
kubectl \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --context "${KUBE_CONTEXT}" \
  -n agent-sandbox-system \
  rollout status deployment/agent-sandbox-controller \
  --timeout=300s

env \
  HELM_CACHE_HOME="${RUN_DIR}/helm/cache" \
  HELM_CONFIG_HOME="${RUN_DIR}/helm/config" \
  HELM_DATA_HOME="${RUN_DIR}/helm/data" \
  helm \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --kube-context "${KUBE_CONTEXT}" \
  install spire-crds spire-crds \
  --repo https://spiffe.github.io/helm-charts-hardened/ \
  --version 0.5.0 \
  --namespace spire \
  --create-namespace \
  --wait \
  --timeout 5m

env \
  HELM_CACHE_HOME="${RUN_DIR}/helm/cache" \
  HELM_CONFIG_HOME="${RUN_DIR}/helm/config" \
  HELM_DATA_HOME="${RUN_DIR}/helm/data" \
  helm \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --kube-context "${KUBE_CONTEXT}" \
  install spire spire \
  --repo https://spiffe.github.io/helm-charts-hardened/ \
  --version 0.29.0 \
  --namespace spire \
  --create-namespace \
  --values "${ROOT}/config/openshell/spire-values.yaml" \
  --wait \
  --timeout 10m

env \
  HELM_CACHE_HOME="${RUN_DIR}/helm/cache" \
  HELM_CONFIG_HOME="${RUN_DIR}/helm/config" \
  HELM_DATA_HOME="${RUN_DIR}/helm/data" \
  helm \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --kube-context "${KUBE_CONTEXT}" \
  install openshell oci://ghcr.io/nvidia/openshell/helm-chart \
  --version 0.0.90 \
  --namespace openshell \
  --create-namespace \
  --values "${ROOT}/config/openshell/provider-token-grants.yaml" \
  --set server.disableTls=true \
  --set server.auth.allowUnauthenticatedUsers=true \
  "${supervisor_image_args[@]}" \
  --wait \
  --timeout 5m

kubectl \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --context "${KUBE_CONTEXT}" \
  -n openshell \
  port-forward svc/openshell :8080 >"${PORT_FORWARD_LOG}" 2>&1 &
PORT_FORWARD_PID=$!

endpoint=""
for _ in $(seq 1 60); do
  if ! kill -0 "${PORT_FORWARD_PID}" >/dev/null 2>&1; then
    echo "OpenShell port-forward exited before becoming ready" >&2
    cat "${PORT_FORWARD_LOG}" >&2
    exit 1
  fi
  port="$(sed -nE 's/.*127\.0\.0\.1:([0-9]+).*/\1/p' "${PORT_FORWARD_LOG}" | head -1)"
  if [[ -n "${port}" ]] && curl -sS --connect-timeout 1 "http://127.0.0.1:${port}" >/dev/null; then
    endpoint="http://127.0.0.1:${port}"
    break
  fi
  sleep 1
done
if [[ -z "${endpoint}" ]]; then
  echo "OpenShell gateway did not become reachable" >&2
  cat "${PORT_FORWARD_LOG}" >&2
  exit 1
fi

export STEWARD_OPENSHELL_ENDPOINT="${endpoint}"
export STEWARD_TEST_KUBE_CONTEXT="${KUBE_CONTEXT}"
export STEWARD_TEST_KUBECONFIG="${KUBECONFIG_PATH}"
export KUBECONFIG="${KUBECONFIG_PATH}"

if [[ "$#" -eq 0 ]]; then
  cargo run \
    -p steward-adapter-openshell \
    --features s0-spike \
    --example workspace_contract
  cli_archive="${RUN_DIR}/${openshell_cli_archive}"
  cli_checksums="${RUN_DIR}/openshell-checksums-sha256.txt"
  curl -fsSL \
    "https://github.com/NVIDIA/OpenShell/releases/download/v0.0.90/${openshell_cli_archive}" \
    -o "${cli_archive}"
  curl -fsSL \
    "https://github.com/NVIDIA/OpenShell/releases/download/v0.0.90/openshell-checksums-sha256.txt" \
    -o "${cli_checksums}"
  (
    cd "${RUN_DIR}"
    grep " ${openshell_cli_archive}$" "${cli_checksums}" | "${checksum_command[@]}"
    tar -xzf "${cli_archive}"
  )
  source_archive="${RUN_DIR}/openshell-v0.0.90.tar.gz"
  source_directory="${RUN_DIR}/openshell-source"
  curl -fsSL \
    "https://github.com/NVIDIA/OpenShell/archive/refs/tags/v0.0.90.tar.gz" \
    -o "${source_archive}"
  mkdir -p "${source_directory}"
  tar -xzf "${source_archive}" -C "${source_directory}" --strip-components=1
  service_subnet="$(
    kubectl \
      --kubeconfig "${KUBECONFIG_PATH}" \
      --context "${KUBE_CONTEXT}" \
      -n kube-system \
      get configmap kubeadm-config \
      -o jsonpath='{.data.ClusterConfiguration}' |
      sed -nE 's/^[[:space:]]*serviceSubnet:[[:space:]]*([^[:space:]]+).*$/\1/p'
  )"
  if [[ -z "${service_subnet}" ]]; then
    echo "could not derive the kind service subnet for the OpenShell demo" >&2
    exit 1
  fi
  demo_profile="${source_directory}/examples/spiffe-token-grant-demo/provider-profile.yaml"
  if ! grep -q "10\\.43\\.0\\.0/16" "${demo_profile}"; then
    echo "OpenShell demo no longer carries its expected k3s service subnet" >&2
    exit 1
  fi
  sed -i.bak "s#10\\.43\\.0\\.0/16#${service_subnet}#g" "${demo_profile}"
  PATH="${RUN_DIR}:${PATH}" \
    XDG_CONFIG_HOME="${RUN_DIR}/openshell-config" \
    GATEWAY_ENDPOINT="${endpoint}" \
    bash "${source_directory}/examples/spiffe-token-grant-demo/demo.sh"
else
  "$@"
fi
