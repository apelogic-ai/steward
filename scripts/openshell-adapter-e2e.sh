#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OPEN_SHELL_RELEASE="v0.0.98"
OPEN_SHELL_HELM_VERSION="${OPEN_SHELL_RELEASE#v}"
RUN_ID="${STEWARD_RUN_ID:-openshell-adapter-$(date -u +%Y%m%d%H%M%S)-$$}"
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
WORKLOAD_EXCHANGE_PID=""
CLUSTER_CREATED=0
OIDC_AUDIENCE="openshell-api"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  if [[ "${status}" != "0" && -f "${workload_exchange_log:-}" ]]; then
    echo "workload exchange mock log:" >&2
    cat "${workload_exchange_log}" >&2
  fi
  if [[ -n "${PORT_FORWARD_PID}" ]]; then
    kill "${PORT_FORWARD_PID}" >/dev/null 2>&1 || true
    wait "${PORT_FORWARD_PID}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${WORKLOAD_EXCHANGE_PID}" ]]; then
    kill "${WORKLOAD_EXCHANGE_PID}" >/dev/null 2>&1 || true
    wait "${WORKLOAD_EXCHANGE_PID}" >/dev/null 2>&1 || true
  fi
  if [[ "${CLUSTER_CREATED}" == "1" && "${STEWARD_DEV_KEEP:-0}" != "1" ]]; then
    KUBECONFIG="${KUBECONFIG_PATH}" kind delete cluster --name "${CLUSTER_NAME}" >/dev/null 2>&1 || true
  fi
  if [[ "${STEWARD_DEV_KEEP:-0}" == "1" ]]; then
    echo "kept run ${RUN_ID}; clean it with: kind delete cluster --name ${CLUSTER_NAME}" >&2
  else
    find "${RUN_DIR}" -depth -delete 2>/dev/null || true
  fi
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for command in cargo curl docker helm kind kubectl openssl python3 tar xxd; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done
docker info >/dev/null

mkdir -p "${RUN_DIR}"
kind create cluster \
  --name "${CLUSTER_NAME}" \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --wait 120s
CLUSTER_CREATED=1

actual_context="$(kubectl --kubeconfig "${KUBECONFIG_PATH}" config current-context)"
if [[ "${actual_context}" != "${KUBE_CONTEXT}" ]]; then
  echo "created context ${actual_context}, expected ${KUBE_CONTEXT}" >&2
  exit 1
fi

kubectl \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --context "${KUBE_CONTEXT}" \
  apply -f - <<YAML
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: openshell-runc
  labels:
    steward.test/run-id: ${RUN_ID}
# Kind cannot provide Kata isolation. This lane verifies only that the
# The generic RuntimeClass contract propagates into the Sandbox pod template.
handler: runc
YAML

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

oidc_issuer="http://oidc.openshell.svc.cluster.local:8000"
oidc_private_key="${RUN_DIR}/oidc-private.pem"
oidc_discovery="${RUN_DIR}/openid-configuration"
oidc_jwks="${RUN_DIR}/jwks.json"
openssl genrsa -out "${oidc_private_key}" 2048 >/dev/null 2>&1
chmod 600 "${oidc_private_key}"
kubectl \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --context "${KUBE_CONTEXT}" \
  create namespace openshell \
  --dry-run=client \
  -o yaml |
  kubectl \
    --kubeconfig "${KUBECONFIG_PATH}" \
    --context "${KUBE_CONTEXT}" \
    apply -f -

base64url() {
  openssl base64 -A | tr '+/' '-_' | tr -d '='
}

oidc_modulus="$(
  openssl rsa -in "${oidc_private_key}" -noout -modulus 2>/dev/null |
    cut -d= -f2 |
    xxd -r -p |
    base64url
)"
printf '{"issuer":"%s","jwks_uri":"%s/jwks.json"}\n' \
  "${oidc_issuer}" "${oidc_issuer}" >"${oidc_discovery}"
printf '{"keys":[{"kty":"RSA","kid":"steward-test","use":"sig","alg":"RS256","n":"%s","e":"AQAB"}]}\n' \
  "${oidc_modulus}" >"${oidc_jwks}"

kubectl \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --context "${KUBE_CONTEXT}" \
  -n openshell \
  create configmap test-oidc-documents \
  --from-file=openid-configuration="${oidc_discovery}" \
  --from-file=jwks.json="${oidc_jwks}" \
  --dry-run=client \
  -o yaml |
  kubectl \
    --kubeconfig "${KUBECONFIG_PATH}" \
    --context "${KUBE_CONTEXT}" \
    apply -f -
kubectl \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --context "${KUBE_CONTEXT}" \
  apply -f - <<'YAML'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-oidc
  namespace: openshell
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test-oidc
  template:
    metadata:
      labels:
        app: test-oidc
    spec:
      containers:
        - name: server
          image: python:3.13.5-alpine3.22@sha256:37b14db89f587f9eaa890e4a442a3fe55db452b69cca1403cc730bd0fbdc8aaf
          args: ["python3", "-m", "http.server", "8000", "--directory", "/srv"]
          ports:
            - { name: http, containerPort: 8000 }
          securityContext:
            allowPrivilegeEscalation: false
            capabilities: { drop: ["ALL"] }
            runAsNonRoot: true
            runAsUser: 65534
          volumeMounts:
            - { name: documents, mountPath: /srv, readOnly: true }
      volumes:
        - name: documents
          configMap:
            name: test-oidc-documents
            items:
              - { key: openid-configuration, path: .well-known/openid-configuration }
              - { key: jwks.json, path: jwks.json }
---
apiVersion: v1
kind: Service
metadata:
  name: oidc
  namespace: openshell
spec:
  selector:
    app: test-oidc
  ports:
    - { name: http, port: 8000, targetPort: http }
YAML
kubectl \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --context "${KUBE_CONTEXT}" \
  -n openshell \
  rollout status deployment/test-oidc \
  --timeout=300s

workload_source_file="${RUN_DIR}/workload-source-credential"
workload_invalid_source_file="${RUN_DIR}/workload-source-credential-invalid"
printf '%s' obviously-fake-workload-source >"${workload_source_file}"
printf '%s' obviously-fake-unmapped-source >"${workload_invalid_source_file}"

workload_exchange_ca_private_key="${RUN_DIR}/workload-exchange-ca.key"
workload_exchange_ca_certificate="${RUN_DIR}/workload-exchange-ca.crt"
workload_exchange_private_key="${RUN_DIR}/workload-exchange.key"
workload_exchange_certificate_request="${RUN_DIR}/workload-exchange.csr"
workload_exchange_certificate="${RUN_DIR}/workload-exchange.crt"
workload_exchange_log="${RUN_DIR}/workload-exchange.log"
openssl req \
  -new \
  -newkey rsa:2048 \
  -x509 \
  -nodes \
  -days 1 \
  -subj /CN=steward-test-workload-exchange-ca \
  -addext basicConstraints=critical,CA:TRUE \
  -addext keyUsage=critical,keyCertSign,cRLSign \
  -keyout "${workload_exchange_ca_private_key}" \
  -out "${workload_exchange_ca_certificate}" >/dev/null 2>&1
openssl req \
  -new \
  -newkey rsa:2048 \
  -nodes \
  -subj /CN=127.0.0.1 \
  -addext subjectAltName=IP:127.0.0.1 \
  -addext basicConstraints=critical,CA:FALSE \
  -addext keyUsage=critical,digitalSignature,keyEncipherment \
  -addext extendedKeyUsage=serverAuth \
  -keyout "${workload_exchange_private_key}" \
  -out "${workload_exchange_certificate_request}" >/dev/null 2>&1
openssl x509 \
  -req \
  -in "${workload_exchange_certificate_request}" \
  -CA "${workload_exchange_ca_certificate}" \
  -CAkey "${workload_exchange_ca_private_key}" \
  -CAcreateserial \
  -days 1 \
  -sha256 \
  -copy_extensions copy \
  -out "${workload_exchange_certificate}" >/dev/null 2>&1
chmod 600 "${workload_exchange_ca_private_key}" "${workload_exchange_private_key}"
python3 "${ROOT}/scripts/test-workload-exchange.py" \
  --certificate "${workload_exchange_certificate}" \
  --private-key "${workload_exchange_private_key}" \
  --source-file "${workload_source_file}" \
  --issuer "${oidc_issuer}" \
  --signing-key "${oidc_private_key}" \
  >"${workload_exchange_log}" 2>&1 &
WORKLOAD_EXCHANGE_PID=$!
for _ in $(seq 1 50); do
  if grep -q '^LISTENING [0-9][0-9]*$' "${workload_exchange_log}"; then
    break
  fi
  if ! kill -0 "${WORKLOAD_EXCHANGE_PID}" >/dev/null 2>&1; then
    echo "workload exchange mock exited before becoming ready" >&2
    cat "${workload_exchange_log}" >&2
    exit 1
  fi
  sleep 0.1
done
workload_exchange_port="$(sed -nE 's/^LISTENING ([0-9]+)$/\1/p' "${workload_exchange_log}")"
if [[ -z "${workload_exchange_port}" ]]; then
  echo "workload exchange mock did not publish its port" >&2
  cat "${workload_exchange_log}" >&2
  exit 1
fi
workload_exchange_endpoint="https://127.0.0.1:${workload_exchange_port}/v1/workload/exchange"

env \
  HELM_CACHE_HOME="${RUN_DIR}/helm/cache" \
  HELM_CONFIG_HOME="${RUN_DIR}/helm/config" \
  HELM_DATA_HOME="${RUN_DIR}/helm/data" \
  helm \
  --kubeconfig "${KUBECONFIG_PATH}" \
  --kube-context "${KUBE_CONTEXT}" \
  install openshell oci://ghcr.io/nvidia/openshell/helm-chart \
  --version "${OPEN_SHELL_HELM_VERSION}" \
  --namespace openshell \
  --create-namespace \
  --set-string server.defaultRuntimeClassName=openshell-runc \
  --set server.auth.allowUnauthenticatedUsers=false \
  --set-string "server.oidc.issuer=${oidc_issuer}" \
  --set-string "server.oidc.audience=${OIDC_AUDIENCE}" \
  --set-string server.oidc.rolesClaim=roles \
  --set-string server.oidc.adminRole=openshell-admin \
  --set-string server.oidc.userRole=openshell-user \
  --wait \
  --timeout 10m

extract_secret_key() {
  secret_name="$1"
  secret_key="$2"
  destination="$3"
  encoded="$(
    kubectl \
      --kubeconfig "${KUBECONFIG_PATH}" \
      --context "${KUBE_CONTEXT}" \
      -n openshell \
      get secret "${secret_name}" \
      -o "go-template={{ index .data \"${secret_key}\" }}"
  )"
  if [[ -z "${encoded}" ]]; then
    echo "OpenShell Secret ${secret_name} has no ${secret_key}" >&2
    exit 1
  fi
  printf '%s' "${encoded}" | openssl base64 -d -A >"${destination}"
}

gateway_ca="${RUN_DIR}/gateway-ca.crt"
client_certificate="${RUN_DIR}/client.crt"
client_private_key="${RUN_DIR}/client.key"
extract_secret_key openshell-client-tls ca.crt "${gateway_ca}"
extract_secret_key openshell-client-tls tls.crt "${client_certificate}"
extract_secret_key openshell-client-tls tls.key "${client_private_key}"
chmod 600 "${client_private_key}"

invalid_ca="${RUN_DIR}/invalid-ca.crt"
invalid_client_certificate="${RUN_DIR}/invalid-client.crt"
invalid_client_private_key="${RUN_DIR}/invalid-client.key"
openssl req \
  -new \
  -newkey rsa:2048 \
  -x509 \
  -nodes \
  -days 1 \
  -subj /CN=untrusted-test-client \
  -keyout "${invalid_client_private_key}" \
  -out "${invalid_client_certificate}" >/dev/null 2>&1
cp "${invalid_client_certificate}" "${invalid_ca}"
chmod 600 "${invalid_client_private_key}"

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
  if [[ -n "${port}" ]] && curl \
    --silent \
    --show-error \
    --connect-timeout 1 \
    --cacert "${gateway_ca}" \
    --cert "${client_certificate}" \
    --key "${client_private_key}" \
    "https://127.0.0.1:${port}" >/dev/null
  then
    endpoint="https://127.0.0.1:${port}"
    break
  fi
  sleep 1
done
if [[ -z "${endpoint}" ]]; then
  echo "OpenShell authenticated TLS gateway did not become reachable" >&2
  cat "${PORT_FORWARD_LOG}" >&2
  exit 1
fi

STEWARD_OPEN_SHELL_RELEASE="${OPEN_SHELL_RELEASE}" \
STEWARD_OPENSHELL_ENDPOINT="${endpoint}" \
STEWARD_OPENSHELL_CA_CERTIFICATE_FILE="${gateway_ca}" \
STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE="${client_certificate}" \
STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE="${client_private_key}" \
STEWARD_OPENSHELL_UNTRUSTED_CA_FILE="${invalid_ca}" \
STEWARD_OPENSHELL_SERVER_NAME=localhost \
STEWARD_OPENSHELL_RUNTIME_CLASS_NAME=openshell-runc \
STEWARD_WORKLOAD_EXCHANGE_ENDPOINT="${workload_exchange_endpoint}" \
STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME=127.0.0.1 \
STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE="${workload_exchange_ca_certificate}" \
STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE="${workload_source_file}" \
STEWARD_WORKLOAD_INVALID_SOURCE_CREDENTIAL_FILE="${workload_invalid_source_file}" \
STEWARD_TEST_KUBE_CONTEXT="${KUBE_CONTEXT}" \
STEWARD_TEST_KUBECONFIG="${KUBECONFIG_PATH}" \
STEWARD_RUN_DIR="${RUN_DIR}" \
cargo test \
  --manifest-path "${ROOT}/e2e/Cargo.toml" \
  --test openshell_adapter_v0098 \
  adapter_round_trip_is_authenticated_with_runtime_class_propagation_and_cleanup \
  -- \
  --exact
