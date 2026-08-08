#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MCP_GW_IMAGE="steward/mcp-gw:c2af10d9-claims"
MINT_IMAGE="steward/mint:s1"

for variable in \
  STEWARD_OPENSHELL_ENDPOINT \
  STEWARD_OPENSHELL_CA_CERTIFICATE_FILE \
  STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE \
  STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE \
  STEWARD_WORKLOAD_EXCHANGE_ENDPOINT \
  STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME \
  STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE \
  STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE \
  STEWARD_OPENSHELL_SERVER_NAME \
  STEWARD_OPENSHELL_RUNTIME_CLASS_NAME \
  STEWARD_RUN_DIR \
  STEWARD_TEST_KUBE_CONTEXT \
  STEWARD_TEST_KUBECONFIG
do
  if [[ -z "${!variable:-}" ]]; then
    echo "${variable} is required from the ephemeral OpenShell harness" >&2
    exit 2
  fi
done
for command in cargo curl docker jq kind kubectl openssl sed tar; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

cluster_name="${STEWARD_TEST_KUBE_CONTEXT#kind-}"
if [[ "${cluster_name}" == "${STEWARD_TEST_KUBE_CONTEXT}" || ! "${cluster_name}" =~ ^steward- ]]; then
  echo "refusing non-ephemeral kube context: ${STEWARD_TEST_KUBE_CONTEXT}" >&2
  exit 1
fi

case "$(uname -s):$(uname -m)" in
  Darwin:arm64)
    openshell_target="aarch64-apple-darwin"
    ;;
  Linux:arm64 | Linux:aarch64)
    openshell_target="aarch64-unknown-linux-musl"
    ;;
  Linux:x86_64 | Linux:amd64)
    openshell_target="x86_64-unknown-linux-musl"
    ;;
  *)
    echo "unsupported OpenShell CLI platform: $(uname -s) $(uname -m)" >&2
    exit 2
    ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
  checksum_command=(sha256sum -c -)
elif command -v shasum >/dev/null 2>&1; then
  checksum_command=(shasum -a 256 -c -)
else
  echo "required command is missing: sha256sum or shasum" >&2
  exit 2
fi

openshell_archive="openshell-${openshell_target}.tar.gz"
cli_archive="${STEWARD_RUN_DIR}/${openshell_archive}"
cli_checksums="${STEWARD_RUN_DIR}/openshell-checksums-sha256.txt"
curl -fsSL \
  "https://github.com/NVIDIA/OpenShell/releases/download/v0.0.90/${openshell_archive}" \
  -o "${cli_archive}"
curl -fsSL \
  "https://github.com/NVIDIA/OpenShell/releases/download/v0.0.90/openshell-checksums-sha256.txt" \
  -o "${cli_checksums}"
(
  cd "${STEWARD_RUN_DIR}"
  grep " ${openshell_archive}$" "${cli_checksums}" | "${checksum_command[@]}"
  tar -xzf "${cli_archive}"
)
OPEN_SHELL="${STEWARD_RUN_DIR}/openshell"
if [[ ! -x "${OPEN_SHELL}" ]]; then
  echo "OpenShell archive did not contain an executable CLI" >&2
  exit 1
fi

kind load docker-image "${MCP_GW_IMAGE}" --name "${cluster_name}"
kind load docker-image "${MINT_IMAGE}" --name "${cluster_name}"

KUBECTL=(
  kubectl
  --kubeconfig "${STEWARD_TEST_KUBECONFIG}"
  --context "${STEWARD_TEST_KUBE_CONTEXT}"
)

"${KUBECTL[@]}" apply -f "${ROOT}/manifests/agents.apelogic.ai_agentruntimes.yaml"
"${KUBECTL[@]}" wait \
  --for=condition=Established \
  crd/agentruntimes.agents.apelogic.ai \
  --timeout=120s
for namespace in steward-system team-a team-b; do
  "${KUBECTL[@]}" create namespace "${namespace}" \
    --dry-run=client \
    -o yaml |
    "${KUBECTL[@]}" apply -f -
done
"${KUBECTL[@]}" -n steward-system create configmap steward-s1-policy \
  --from-file="mcp_tools.rego=${ROOT}/policy/mcp_tools.rego" \
  --dry-run=client \
  -o yaml |
  "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" -n steward-system create configmap steward-s1-fixtures \
  --from-file="fake-github-mcp.ts=${ROOT}/config/s1/fake-github-mcp.ts" \
  --from-file="seed-mcp-gw.ts=${ROOT}/config/s1/seed-mcp-gw.ts" \
  --dry-run=client \
  -o yaml |
  "${KUBECTL[@]}" apply -f -

signing_key="${STEWARD_RUN_DIR}/mint-signing-key"
introspection_client="${STEWARD_RUN_DIR}/introspection-client"
encryption_key="${STEWARD_RUN_DIR}/mcp-encryption-key"
openssl rand 32 >"${signing_key}"
openssl rand -hex 24 | tr -d '\n' >"${introspection_client}"
openssl rand -base64 32 | tr -d '\n' >"${encryption_key}"
"${KUBECTL[@]}" -n steward-system create secret generic steward-s1-mint \
  --from-file="signing-key=${signing_key}" \
  --from-file="introspection-client=${introspection_client}" \
  --dry-run=client \
  -o yaml |
  "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" -n steward-system create secret generic steward-s1-mcp-gw \
  --from-file="encryption-key=${encryption_key}" \
  --from-file="introspection-client=${introspection_client}" \
  --dry-run=client \
  -o yaml |
  "${KUBECTL[@]}" apply -f -

"${KUBECTL[@]}" apply -f "${ROOT}/config/s1/stack.yaml"
"${KUBECTL[@]}" -n steward-system rollout status deployment/postgres --timeout=180s
"${KUBECTL[@]}" -n steward-system wait --for=condition=complete job/seed-mcp-gw --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/steward-opa --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/fake-github-mcp --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/steward-mint --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/mcp-gw --timeout=180s

service_subnet="$(
  "${KUBECTL[@]}" \
    -n kube-system \
    get configmap kubeadm-config \
    -o jsonpath='{.data.ClusterConfiguration}' |
    sed -nE 's/^[[:space:]]*serviceSubnet:[[:space:]]*([^[:space:]]+).*$/\1/p'
)"
if [[ -z "${service_subnet}" ]]; then
  echo "could not derive the kind service subnet" >&2
  exit 1
fi
profile="${STEWARD_RUN_DIR}/steward-mcp-gw-profile.yaml"
sed "s#SERVICE_SUBNET#${service_subnet}#g" \
  "${ROOT}/config/s1/provider-profile.yaml" >"${profile}"

"${OPEN_SHELL}" \
  --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  settings set --global --key providers_v2_enabled --value true --yes
"${OPEN_SHELL}" \
  --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  provider profile lint --global -f "${profile}"
"${OPEN_SHELL}" \
  --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  provider profile import --global -f "${profile}"

cargo build -p steward-controller-bin
export STEWARD_AGENTRUNTIME_API_VERSION="agents.apelogic.ai/v1alpha1"
export STEWARD_CONTROLLER_BIN="${ROOT}/target/debug/steward-controller-bin"
export STEWARD_MINT_TEST_SERVICE="steward-mint"
export STEWARD_MINT_TEST_NAMESPACE="steward-system"
export STEWARD_OPENSHELL_CLI="${OPEN_SHELL}"
export STEWARD_S0_BOOTSTRAP=1

cargo test \
  --manifest-path "${ROOT}/e2e/Cargo.toml" \
  --test s1 \
  e2e_s1_tool_call_as_acting_user \
  -- \
  --exact
