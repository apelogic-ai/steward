#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SLICE="${STEWARD_E2E_SLICE:-s2}"
if [[ "${SLICE}" != "s2" && "${SLICE}" != "s5" ]]; then
  echo "STEWARD_E2E_SLICE must be s2 or s5" >&2
  exit 2
fi
CAPTURE_FORWARD_PID=""
LITELLM_FORWARD_PID=""
POSTGRES_FORWARD_PID=""

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  for pid in "${CAPTURE_FORWARD_PID}" "${LITELLM_FORWARD_PID}" "${POSTGRES_FORWARD_PID}"; do
    if [[ -n "${pid}" ]]; then
      kill "${pid}" >/dev/null 2>&1 || true
      wait "${pid}" >/dev/null 2>&1 || true
    fi
  done
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for variable in STEWARD_OPENSHELL_ENDPOINT STEWARD_RUN_DIR STEWARD_RUN_ID \
  STEWARD_S2_CONTROLLER_IMAGE STEWARD_TEST_KUBE_CONTEXT STEWARD_TEST_KUBECONFIG
do
  if [[ -z "${!variable:-}" ]]; then
    echo "${variable} is required from the ephemeral S2 harness" >&2
    exit 2
  fi
done
if [[ "${SLICE}" == "s5" && -z "${STEWARD_S5_MCP_GW_IMAGE:-}" ]]; then
  echo "STEWARD_S5_MCP_GW_IMAGE is required from the ephemeral S5 harness" >&2
  exit 2
fi
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
  Darwin:arm64) openshell_target="aarch64-apple-darwin" ;;
  Linux:arm64 | Linux:aarch64) openshell_target="aarch64-unknown-linux-musl" ;;
  Linux:x86_64 | Linux:amd64) openshell_target="x86_64-unknown-linux-musl" ;;
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
curl -fsSL "https://github.com/NVIDIA/OpenShell/releases/download/v0.0.90/${openshell_archive}" \
  -o "${STEWARD_RUN_DIR}/${openshell_archive}"
curl -fsSL "https://github.com/NVIDIA/OpenShell/releases/download/v0.0.90/openshell-checksums-sha256.txt" \
  -o "${STEWARD_RUN_DIR}/openshell-checksums-sha256.txt"
(
  cd "${STEWARD_RUN_DIR}"
  grep " ${openshell_archive}$" openshell-checksums-sha256.txt | "${checksum_command[@]}"
  tar -xzf "${openshell_archive}"
)
OPEN_SHELL="${STEWARD_RUN_DIR}/openshell"
if [[ ! -x "${OPEN_SHELL}" ]]; then
  echo "OpenShell archive did not contain an executable CLI" >&2
  exit 1
fi

kind load docker-image steward/mint:s1 --name "${cluster_name}"
kind load docker-image "${STEWARD_S2_CONTROLLER_IMAGE}" --name "${cluster_name}"
if [[ "${SLICE}" == "s5" ]]; then
  kind load docker-image "${STEWARD_S5_MCP_GW_IMAGE}" --name "${cluster_name}"
fi

KUBECTL=(kubectl --kubeconfig "${STEWARD_TEST_KUBECONFIG}" --context "${STEWARD_TEST_KUBE_CONTEXT}")
"${KUBECTL[@]}" apply -f "${ROOT}/manifests/agents.apelogic.ai_agentruntimes.yaml"
"${KUBECTL[@]}" wait --for=condition=Established \
  crd/agentruntimes.agents.apelogic.ai --timeout=120s

signing_key="${STEWARD_RUN_DIR}/s2-signing-key"
introspection_client="${STEWARD_RUN_DIR}/s2-introspection-client"
master_key="${STEWARD_RUN_DIR}/s2-litellm-master-key"
encryption_key="${STEWARD_RUN_DIR}/s5-mcp-encryption-key"
tls_key="${STEWARD_RUN_DIR}/s2-tls-key.pem"
tls_cert="${STEWARD_RUN_DIR}/s2-tls-cert.pem"
tls_key_der="${STEWARD_RUN_DIR}/s2-tls-key.der"
tls_cert_der="${STEWARD_RUN_DIR}/s2-tls-cert.der"
openssl rand 32 >"${signing_key}"
openssl rand -hex 24 | tr -d '\n' >"${introspection_client}"
openssl rand -hex 32 | tr -d '\n' >"${master_key}"
if [[ "${SLICE}" == "s5" ]]; then
  openssl rand -base64 32 | tr -d '\n' >"${encryption_key}"
fi
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "${tls_key}" >/dev/null 2>&1
openssl req -new -x509 -key "${tls_key}" -out "${tls_cert}" -days 1 \
  -subj "/CN=steward-controller.steward-system.svc" \
  -addext "subjectAltName=DNS:steward-controller.steward-system.svc" >/dev/null 2>&1
openssl x509 -in "${tls_cert}" -outform DER -out "${tls_cert_der}"
openssl pkcs8 -topk8 -nocrypt -in "${tls_key}" -outform DER -out "${tls_key_der}"
chmod 600 "${signing_key}" "${introspection_client}" "${master_key}" "${tls_key}" "${tls_key_der}"
if [[ "${SLICE}" == "s5" ]]; then
  chmod 600 "${encryption_key}"
fi

for namespace in steward-system team-a; do
  "${KUBECTL[@]}" create namespace "${namespace}" --dry-run=client -o yaml |
    "${KUBECTL[@]}" apply -f -
  "${KUBECTL[@]}" label namespace "${namespace}" \
    "steward.test/run-id=${STEWARD_RUN_ID}" --overwrite
done
"${KUBECTL[@]}" -n steward-system create secret generic steward-s2-secrets \
  --from-file="signing-key=${signing_key}" \
  --from-file="introspection-client=${introspection_client}" \
  --from-file="litellm-master-key=${master_key}" \
  --from-file="tls-cert.der=${tls_cert_der}" \
  --from-file="tls-key.der=${tls_key_der}" \
  --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" -n steward-system label secret steward-s2-secrets \
  "steward.test/run-id=${STEWARD_RUN_ID}" --overwrite
if [[ "${SLICE}" == "s5" ]]; then
  "${KUBECTL[@]}" -n steward-system create configmap steward-s5-policy \
    --from-file="mcp_tools.rego=${ROOT}/policy/mcp_tools.rego" \
    --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f -
  "${KUBECTL[@]}" -n steward-system create configmap steward-s5-fixtures \
    --from-file="capture-proxy.ts=${ROOT}/config/s5/capture-proxy.ts" \
    --from-file="fake-github-mcp.ts=${ROOT}/config/s1/fake-github-mcp.ts" \
    --from-file="seed-mcp-gw.ts=${ROOT}/config/s1/seed-mcp-gw.ts" \
    --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f -
  "${KUBECTL[@]}" -n steward-system create secret generic steward-s5-mcp-gw \
    --from-file="encryption-key=${encryption_key}" \
    --from-file="introspection-client=${introspection_client}" \
    --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f -
fi

rendered_stack="${STEWARD_RUN_DIR}/s2-stack.yaml"
sed "s#STEWARD_S2_CONTROLLER_IMAGE#${STEWARD_S2_CONTROLLER_IMAGE}#g" \
  "${ROOT}/config/s2/stack.yaml" >"${rendered_stack}"
"${KUBECTL[@]}" apply -f "${rendered_stack}"
if [[ "${SLICE}" == "s5" ]]; then
  rendered_tools_stack="${STEWARD_RUN_DIR}/s5-tools-stack.yaml"
  sed "s#STEWARD_S5_MCP_GW_IMAGE#${STEWARD_S5_MCP_GW_IMAGE}#g" \
    "${ROOT}/config/s5/tools-stack.yaml" >"${rendered_tools_stack}"
  "${KUBECTL[@]}" apply -f "${rendered_tools_stack}"
fi
"${KUBECTL[@]}" -n steward-system rollout status deployment/postgres --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/litellm --timeout=300s
"${KUBECTL[@]}" -n steward-system rollout status deployment/steward-mint --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/steward-controller --timeout=300s
if [[ "${SLICE}" == "s5" ]]; then
  "${KUBECTL[@]}" -n steward-system rollout status deployment/steward-mint-tools --timeout=180s
  "${KUBECTL[@]}" -n steward-system rollout status deployment/steward-opa --timeout=180s
  "${KUBECTL[@]}" -n steward-system rollout status deployment/fake-github-mcp --timeout=180s
  "${KUBECTL[@]}" -n steward-system rollout status deployment/mcp-gw --timeout=180s
  "${KUBECTL[@]}" -n steward-system rollout status deployment/hop1-capture --timeout=180s
  "${KUBECTL[@]}" -n steward-system wait --for=condition=complete job/seed-mcp-gw --timeout=180s
fi

service_subnet="$(
  "${KUBECTL[@]}" -n kube-system get configmap kubeadm-config \
    -o jsonpath='{.data.ClusterConfiguration}' |
    sed -nE 's/^[[:space:]]*serviceSubnet:[[:space:]]*([^[:space:]]+).*$/\1/p'
)"
if [[ -z "${service_subnet}" ]]; then
  echo "could not derive the kind service subnet" >&2
  exit 1
fi
"${OPEN_SHELL}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  settings set --global --key providers_v2_enabled --value true --yes
if [[ "${SLICE}" == "s5" ]]; then
  profile_sources=(
    "${ROOT}/config/s5/tool-provider-profile.yaml"
    "${ROOT}/config/s5/inference-provider-profile.yaml"
  )
else
  profile_sources=("${ROOT}/config/s2/provider-profile.yaml")
fi
for profile_source in "${profile_sources[@]}"; do
  profile="${STEWARD_RUN_DIR}/$(basename "${profile_source}")"
  sed "s#SERVICE_SUBNET#${service_subnet}#g" "${profile_source}" >"${profile}"
  "${OPEN_SHELL}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    provider profile lint --global -f "${profile}"
  "${OPEN_SHELL}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    provider profile import --global -f "${profile}"
done

ca_bundle="$(base64 <"${tls_cert}" | tr -d '\n')"
cat <<EOF | "${KUBECTL[@]}" apply -f -
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: steward-s2
  labels:
    steward.test/run-id: "${STEWARD_RUN_ID}"
webhooks:
  - name: agentruntime.agents.apelogic.ai
    admissionReviewVersions: ["v1"]
    sideEffects: None
    failurePolicy: Fail
    timeoutSeconds: 10
    clientConfig:
      service:
        namespace: steward-system
        name: steward-controller
        path: /validate-agent-runtime
        port: 443
      caBundle: "${ca_bundle}"
    rules:
      - apiGroups: ["agents.apelogic.ai"]
        apiVersions: ["v1alpha1"]
        operations: ["CREATE", "UPDATE"]
        resources: ["agentruntimes"]
        scope: Namespaced
EOF

litellm_log="${STEWARD_RUN_DIR}/s2-litellm-forward.log"
"${KUBECTL[@]}" -n steward-system port-forward service/litellm :4000 >"${litellm_log}" 2>&1 &
LITELLM_FORWARD_PID=$!
postgres_log="${STEWARD_RUN_DIR}/s2-postgres-forward.log"
"${KUBECTL[@]}" -n steward-system port-forward service/postgres :5432 >"${postgres_log}" 2>&1 &
POSTGRES_FORWARD_PID=$!
if [[ "${SLICE}" == "s5" ]]; then
  capture_log="${STEWARD_RUN_DIR}/s5-capture-forward.log"
  "${KUBECTL[@]}" -n steward-system port-forward service/hop1-capture :8085 >"${capture_log}" 2>&1 &
  CAPTURE_FORWARD_PID=$!
fi

forwarded_port() {
  local log="$1"
  local pid="$2"
  local port=""
  for _attempt in {1..60}; do
    port="$(sed -nE 's/.*127\.0\.0\.1:([0-9]+).*/\1/p' "${log}" | head -1)"
    if [[ -n "${port}" ]]; then
      printf '%s' "${port}"
      return 0
    fi
    if ! kill -0 "${pid}" >/dev/null 2>&1; then
      cat "${log}" >&2
      return 1
    fi
    sleep 1
  done
  return 1
}

litellm_port="$(forwarded_port "${litellm_log}" "${LITELLM_FORWARD_PID}")"
postgres_port="$(forwarded_port "${postgres_log}" "${POSTGRES_FORWARD_PID}")"
if [[ "${SLICE}" == "s5" ]]; then
  capture_port="$(forwarded_port "${capture_log}" "${CAPTURE_FORWARD_PID}")"
  export STEWARD_TEST_CAPTURE_URL="http://127.0.0.1:${capture_port}"
fi
export STEWARD_OPENSHELL_CLI="${OPEN_SHELL}"
export STEWARD_TEST_DATABASE_URL="postgres://steward@127.0.0.1:${postgres_port}/steward"
if [[ "${SLICE}" == "s5" ]]; then
  export STEWARD_TEST_INFERENCE_URL="http://hop1-capture-inference.steward-system.svc.cluster.local:8085/inference"
  export STEWARD_TEST_TOOL_URL="http://hop1-capture-tools.steward-system.svc.cluster.local:8085/mcp"
else
  export STEWARD_TEST_INFERENCE_URL="http://litellm.steward-system.svc.cluster.local:4000/v1/chat/completions"
fi
export STEWARD_TEST_LITELLM_MASTER_KEY_FILE="${master_key}"
export STEWARD_TEST_LITELLM_URL="http://127.0.0.1:${litellm_port}"

if [[ "${SLICE}" == "s5" ]]; then
  cargo test --manifest-path "${ROOT}/e2e/Cargo.toml" --test s5 \
    e2e_s5_terminated_runtime_holds_nothing -- --exact
else
  cargo test --manifest-path "${ROOT}/e2e/Cargo.toml" --test s2_store
  cargo test --manifest-path "${ROOT}/e2e/Cargo.toml" --test s2 \
    e2e_s2_budget_exhaustion_suspends -- --exact
fi
