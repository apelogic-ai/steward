#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
for variable in \
  STEWARD_CONNECTIONS_TEST_MCP_GW_IMAGE \
  STEWARD_CONNECTIONS_TEST_MINT_IMAGE \
  STEWARD_CONNECTIONS_TEST_BRIDGE_IMAGE \
  STEWARD_CONNECTIONS_TEST_WEBHOOK_IMAGE \
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
for command in awk base64 cargo curl docker kind kubectl openssl sed tar; do
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
run_id="${cluster_name#steward-}"
KUBECTL=(
  kubectl
  --kubeconfig "${STEWARD_TEST_KUBECONFIG}"
  --context "${STEWARD_TEST_KUBE_CONTEXT}"
)
postgres_forward_pid=""
mcp_forward_pid=""
cleanup() {
  status="$1"
  trap - EXIT INT TERM
  for pid in "${postgres_forward_pid}" "${mcp_forward_pid}"; do
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

kind load docker-image "${STEWARD_CONNECTIONS_TEST_MCP_GW_IMAGE}" --name "${cluster_name}"
kind load docker-image "${STEWARD_CONNECTIONS_TEST_MINT_IMAGE}" --name "${cluster_name}"
kind load docker-image "${STEWARD_CONNECTIONS_TEST_BRIDGE_IMAGE}" --name "${cluster_name}"
kind load docker-image "${STEWARD_CONNECTIONS_TEST_WEBHOOK_IMAGE}" --name "${cluster_name}"

bridge_containerd_name="docker.io/${STEWARD_CONNECTIONS_TEST_BRIDGE_IMAGE}"
bridge_digest="$(
  docker exec "${cluster_name}-control-plane" ctr -n k8s.io images list |
    awk -v image="${bridge_containerd_name}" '$1 == image { print $3 }'
)"
if [[ ! "${bridge_digest}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "loaded bridge image has no exact containerd manifest digest" >&2
  exit 1
fi
bridge_digest_image="docker.io/${STEWARD_CONNECTIONS_TEST_BRIDGE_IMAGE%:*}@${bridge_digest}"
docker exec "${cluster_name}-control-plane" \
  ctr -n k8s.io images tag "${bridge_containerd_name}" "${bridge_digest_image}"

"${KUBECTL[@]}" apply -f "${ROOT}/manifests/agents.apelogic.ai_agentruntimes.yaml"
"${KUBECTL[@]}" wait \
  --for=condition=Established \
  crd/agentruntimes.agents.apelogic.ai \
  --timeout=120s
for namespace in steward-system steward-connections team-a team-b; do
  "${KUBECTL[@]}" create namespace "${namespace}" --dry-run=client -o yaml |
    "${KUBECTL[@]}" apply -f -
  "${KUBECTL[@]}" label namespace "${namespace}" \
    "steward.test/run-id=${run_id}" --overwrite
done
"${KUBECTL[@]}" label namespace steward-connections \
  "steward.test/connections-admission=true" --overwrite

webhook_key_pem="${STEWARD_RUN_DIR}/connections-webhook-key.pem"
webhook_key_der="${STEWARD_RUN_DIR}/connections-webhook-key.der"
webhook_cert_pem="${STEWARD_RUN_DIR}/connections-webhook-cert.pem"
webhook_cert_der="${STEWARD_RUN_DIR}/connections-webhook-cert.der"
openssl genpkey \
  -algorithm RSA \
  -pkeyopt rsa_keygen_bits:2048 \
  -out "${webhook_key_pem}" \
  >/dev/null 2>&1
openssl req \
  -new \
  -x509 \
  -key "${webhook_key_pem}" \
  -out "${webhook_cert_pem}" \
  -days 1 \
  -subj "/CN=steward-connections-webhook.steward-system.svc" \
  -addext "subjectAltName=DNS:steward-connections-webhook,DNS:steward-connections-webhook.steward-system.svc,DNS:steward-connections-webhook.steward-system.svc.cluster.local" \
  >/dev/null 2>&1
openssl x509 \
  -in "${webhook_cert_pem}" \
  -outform DER \
  -out "${webhook_cert_der}"
openssl pkcs8 \
  -topk8 \
  -nocrypt \
  -in "${webhook_key_pem}" \
  -outform DER \
  -out "${webhook_key_der}"
chmod 600 "${webhook_key_pem}" "${webhook_key_der}"
"${KUBECTL[@]}" -n steward-system create secret generic steward-connections-webhook-tls \
  --from-file="tls-cert.der=${webhook_cert_der}" \
  --from-file="tls-key.der=${webhook_key_der}" \
  --dry-run=client -o yaml |
  "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" -n steward-system label secret steward-connections-webhook-tls \
  "steward.test/run-id=${run_id}" --overwrite

"${KUBECTL[@]}" -n steward-system create configmap steward-connections-e2e-fixtures \
  --from-file="mcp_tools.rego=${ROOT}/policy/mcp_tools.rego" \
  --from-file="provider-fixture.ts=${ROOT}/config/connections-e2e/provider-fixture.ts" \
  --dry-run=client -o yaml |
  "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" -n steward-system label configmap steward-connections-e2e-fixtures \
  "steward.test/run-id=${run_id}" --overwrite

signing_key="${STEWARD_RUN_DIR}/connections-mint-signing-key"
introspection_client="${STEWARD_RUN_DIR}/connections-introspection-client"
encryption_key="${STEWARD_RUN_DIR}/connections-mcp-encryption-key"
openssl rand 32 >"${signing_key}"
openssl rand -hex 24 | tr -d '\n' >"${introspection_client}"
openssl rand -base64 32 | tr -d '\n' >"${encryption_key}"
chmod 600 "${signing_key}" "${introspection_client}" "${encryption_key}"
"${KUBECTL[@]}" -n steward-system create secret generic steward-connections-e2e-mint \
  --from-file="signing-key=${signing_key}" \
  --from-file="introspection-client=${introspection_client}" \
  --dry-run=client -o yaml |
  "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" -n steward-system create secret generic steward-connections-e2e-mcp-gw \
  --from-file="encryption-key=${encryption_key}" \
  --from-file="introspection-client=${introspection_client}" \
  --dry-run=client -o yaml |
  "${KUBECTL[@]}" apply -f -
for secret in steward-connections-e2e-mint steward-connections-e2e-mcp-gw; do
  "${KUBECTL[@]}" -n steward-system label secret "${secret}" \
    "steward.test/run-id=${run_id}" --overwrite
done

rendered_stack="${STEWARD_RUN_DIR}/governed-connections-stack.yaml"
sed \
  -e "s#RUN_ID_PLACEHOLDER#${run_id}#g" \
  -e "s#MCP_GW_IMAGE_PLACEHOLDER#${STEWARD_CONNECTIONS_TEST_MCP_GW_IMAGE}#g" \
  -e "s#MINT_IMAGE_PLACEHOLDER#${STEWARD_CONNECTIONS_TEST_MINT_IMAGE}#g" \
  "${ROOT}/config/connections-e2e/stack.yaml" >"${rendered_stack}"
"${KUBECTL[@]}" apply -f "${rendered_stack}"
"${KUBECTL[@]}" -n steward-system rollout status deployment/postgres --timeout=180s
controller_username="$(
  "${KUBECTL[@]}" auth whoami -o jsonpath='{.status.userInfo.username}'
)"
if [[ -z "${controller_username}" ]]; then
  echo "could not derive the E2E controller Kubernetes username" >&2
  exit 1
fi
webhook_ca_bundle="$(base64 <"${webhook_cert_pem}" | tr -d '\n')"
cat <<YAML | "${KUBECTL[@]}" apply -f -
apiVersion: apps/v1
kind: Deployment
metadata:
  name: steward-connections-webhook
  namespace: steward-system
  labels: { steward.test/run-id: "${run_id}" }
spec:
  replicas: 1
  selector:
    matchLabels: { app: steward-connections-webhook }
  template:
    metadata:
      labels:
        app: steward-connections-webhook
        steward.test/run-id: "${run_id}"
    spec:
      automountServiceAccountToken: false
      containers:
        - name: webhook
          image: "${STEWARD_CONNECTIONS_TEST_WEBHOOK_IMAGE}"
          imagePullPolicy: Never
          env:
            - { name: STEWARD_TEST_DATABASE_URL, value: "postgres://steward@postgres.steward-system.svc.cluster.local:5432/steward" }
            - { name: STEWARD_TEST_CONTROLLER_USERNAME, value: "${controller_username}" }
            - { name: STEWARD_TEST_TLS_CERT_DER, value: /run/tls/tls-cert.der }
            - { name: STEWARD_TEST_TLS_KEY_DER, value: /run/tls/tls-key.der }
          ports: [{ name: https, containerPort: 8443 }]
          readinessProbe: { tcpSocket: { port: https }, initialDelaySeconds: 2, periodSeconds: 2 }
          volumeMounts: [{ name: tls, mountPath: /run/tls, readOnly: true }]
      volumes:
        - name: tls
          secret: { secretName: steward-connections-webhook-tls, defaultMode: 0444 }
---
apiVersion: v1
kind: Service
metadata:
  name: steward-connections-webhook
  namespace: steward-system
  labels: { steward.test/run-id: "${run_id}" }
spec:
  selector: { app: steward-connections-webhook }
  ports: [{ name: https, port: 443, targetPort: https }]
---
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: steward-connections-${run_id}
  labels: { steward.test/run-id: "${run_id}" }
webhooks:
  - name: connection-agentruntime.steward.agents.apelogic.ai
    admissionReviewVersions: ["v1"]
    sideEffects: None
    failurePolicy: Fail
    matchPolicy: Equivalent
    timeoutSeconds: 10
    namespaceSelector:
      matchLabels:
        steward.test/connections-admission: "true"
    clientConfig:
      service:
        name: steward-connections-webhook
        namespace: steward-system
        path: /validate-agent-runtime
        port: 443
      caBundle: "${webhook_ca_bundle}"
    rules:
      - apiGroups: ["agents.apelogic.ai"]
        apiVersions: ["v1alpha1"]
        operations: ["CREATE"]
        resources: ["agentruntimes"]
        scope: Namespaced
YAML
"${KUBECTL[@]}" -n steward-system rollout status \
  deployment/steward-connections-webhook --timeout=180s
"${KUBECTL[@]}" -n steward-system wait --for=condition=complete job/oauth-migrations --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/steward-opa --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/provider-fixture --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/steward-mint --timeout=180s
"${KUBECTL[@]}" -n steward-system rollout status deployment/mcp-gw --timeout=180s

service_subnet="$(
  "${KUBECTL[@]}" -n kube-system get configmap kubeadm-config \
    -o jsonpath='{.data.ClusterConfiguration}' |
    sed -nE 's/^[[:space:]]*serviceSubnet:[[:space:]]*([^[:space:]]+).*$/\1/p'
)"
if [[ -z "${service_subnet}" ]]; then
  echo "could not derive the kind service subnet" >&2
  exit 1
fi
profile="${STEWARD_RUN_DIR}/governed-connections-provider-profile.yaml"
sed "s#SERVICE_SUBNET#${service_subnet}#g" \
  "${ROOT}/config/connections-e2e/provider-profile.yaml" >"${profile}"

case "$(uname -s):$(uname -m)" in
  Darwin:arm64) openshell_target="aarch64-apple-darwin" ;;
  Linux:arm64 | Linux:aarch64) openshell_target="aarch64-unknown-linux-musl" ;;
  Linux:x86_64 | Linux:amd64) openshell_target="x86_64-unknown-linux-musl" ;;
  *) echo "unsupported OpenShell CLI platform" >&2; exit 2 ;;
esac
if command -v sha256sum >/dev/null 2>&1; then
  checksum_command=(sha256sum -c -)
else
  checksum_command=(shasum -a 256 -c -)
fi
openshell_archive="openshell-${openshell_target}.tar.gz"
curl -fsSL --retry 4 --retry-delay 2 --retry-all-errors \
  "https://github.com/NVIDIA/OpenShell/releases/download/v0.0.98/${openshell_archive}" \
  -o "${STEWARD_RUN_DIR}/${openshell_archive}"
curl -fsSL --retry 4 --retry-delay 2 --retry-all-errors \
  "https://github.com/NVIDIA/OpenShell/releases/download/v0.0.98/openshell-checksums-sha256.txt" \
  -o "${STEWARD_RUN_DIR}/openshell-checksums-sha256.txt"
(
  cd "${STEWARD_RUN_DIR}"
  grep " ${openshell_archive}$" openshell-checksums-sha256.txt | "${checksum_command[@]}"
  tar -xzf "${openshell_archive}"
)
OPEN_SHELL="${STEWARD_RUN_DIR}/openshell"
"${OPEN_SHELL}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  settings set --global --key providers_v2_enabled --value true --yes
"${OPEN_SHELL}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  provider profile lint --global -f "${profile}"
"${OPEN_SHELL}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  provider profile import --global -f "${profile}"

postgres_forward_log="${STEWARD_RUN_DIR}/connections-postgres-forward.log"
mcp_forward_log="${STEWARD_RUN_DIR}/connections-mcp-forward.log"
"${KUBECTL[@]}" -n steward-system port-forward svc/postgres :5432 \
  >"${postgres_forward_log}" 2>&1 &
postgres_forward_pid=$!
"${KUBECTL[@]}" -n steward-system port-forward svc/mcp-gw :8080 \
  >"${mcp_forward_log}" 2>&1 &
mcp_forward_pid=$!
for log in "${postgres_forward_log}" "${mcp_forward_log}"; do
  for _ in $(seq 1 100); do
    if grep -q 'Forwarding from 127.0.0.1:' "${log}"; then
      break
    fi
    sleep 0.1
  done
  if ! grep -q 'Forwarding from 127.0.0.1:' "${log}"; then
    echo "port-forward did not become ready: ${log}" >&2
    exit 1
  fi
done
postgres_port="$(sed -nE 's/.*127\.0\.0\.1:([0-9]+).*/\1/p' "${postgres_forward_log}" | head -1)"
mcp_port="$(sed -nE 's/.*127\.0\.0\.1:([0-9]+).*/\1/p' "${mcp_forward_log}" | head -1)"

STEWARD_OPEN_SHELL_RELEASE=v0.0.98 \
STEWARD_OPENSHELL_CLI="${OPEN_SHELL}" \
STEWARD_CONNECTIONS_TEST_DATABASE_URL="postgres://steward@127.0.0.1:${postgres_port}/steward" \
STEWARD_CONNECTIONS_TEST_MCP_FORWARD="127.0.0.1:${mcp_port}" \
STEWARD_CONNECTIONS_TEST_BRIDGE_DIGEST_IMAGE="${bridge_digest_image}" \
STEWARD_AGENTRUNTIME_API_VERSION="agents.apelogic.ai/v1alpha1" \
cargo test \
  --manifest-path "${ROOT}/e2e/Cargo.toml" \
  --test governed_connections \
  governed_connections_share_the_runtime_credential_owner_and_cleanup_exactly \
  -- \
  --exact --nocapture
