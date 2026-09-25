#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OPEN_SHELL_RELEASE="${STEWARD_OPEN_SHELL_RELEASE:-v0.0.90}"
if [[ ! "${OPEN_SHELL_RELEASE}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "STEWARD_OPEN_SHELL_RELEASE must be a semantic release tag" >&2
  exit 2
fi
OPEN_SHELL_SUPERVISOR_TOPOLOGY="${STEWARD_OPENSHELL_SUPERVISOR_TOPOLOGY:-}"
case "${OPEN_SHELL_SUPERVISOR_TOPOLOGY}" in
  "" | combined | sidecar) ;;
  *)
    echo "STEWARD_OPENSHELL_SUPERVISOR_TOPOLOGY must be combined or sidecar" >&2
    exit 2
    ;;
esac
OPEN_SHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY="${STEWARD_OPENSHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY:-}"
case "${OPEN_SHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY}" in
  "" | true | false) ;;
  *)
    echo "STEWARD_OPENSHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY must be true or false" >&2
    exit 2
    ;;
esac
if [[ -n "${OPEN_SHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY}" \
  && "${OPEN_SHELL_SUPERVISOR_TOPOLOGY}" != "sidecar" ]]
then
  echo "STEWARD_OPENSHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY requires sidecar topology" >&2
  exit 2
fi
OPEN_SHELL_HELM_VERSION="${OPEN_SHELL_RELEASE#v}"
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
WORKLOAD_EXCHANGE_PID=""
CLUSTER_CREATED=0
OIDC_AUDIENCE="openshell-api"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
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

for command in kind kubectl helm cargo curl jq openssl python3 sed tar xxd; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command is missing: ${command}" >&2
    exit 2
  fi
done

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

sandbox_image_args=()
if [[ -n "${STEWARD_OPENSHELL_SANDBOX_IMAGE:-}" ]]; then
  if [[ "${STEWARD_OPENSHELL_SANDBOX_IMAGE}" != *:* || "${STEWARD_OPENSHELL_SANDBOX_IMAGE}" == *@* ]]; then
    echo "STEWARD_OPENSHELL_SANDBOX_IMAGE must be an explicit repository:tag reference" >&2
    exit 2
  fi
  kind load docker-image \
    "${STEWARD_OPENSHELL_SANDBOX_IMAGE}" \
    --name "${CLUSTER_NAME}"
  sandbox_image_args=(
    --set-string "server.sandboxImage=${STEWARD_OPENSHELL_SANDBOX_IMAGE}"
    --set-string "server.sandboxImagePullPolicy=IfNotPresent"
  )
fi

actual_context="$(
  kubectl --kubeconfig "${KUBECONFIG_PATH}" config current-context
)"
if [[ "${actual_context}" != "${KUBE_CONTEXT}" ]]; then
  echo "created context ${actual_context}, expected ${KUBE_CONTEXT}" >&2
  exit 1
fi

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

issued_at="$(date +%s)"
expires_at="$((issued_at + 3600))"
jwt_header="$(printf '%s' '{"alg":"RS256","kid":"steward-test","typ":"JWT"}' | base64url)"
jwt_payload="$(
  printf '{"iss":"%s","sub":"steward-e2e","preferred_username":"alice","aud":"%s","roles":["openshell-admin","openshell-user"],"iat":%s,"exp":%s}' \
    "${oidc_issuer}" "${OIDC_AUDIENCE}" "${issued_at}" "${expires_at}" |
    base64url
)"
jwt_signature="$(
  printf '%s.%s' "${jwt_header}" "${jwt_payload}" |
    openssl dgst -sha256 -sign "${oidc_private_key}" |
    base64url
)"
oidc_token="${jwt_header}.${jwt_payload}.${jwt_signature}"

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

openshell_helm_args=(
  --kubeconfig "${KUBECONFIG_PATH}"
  --kube-context "${KUBE_CONTEXT}"
  install openshell oci://ghcr.io/nvidia/openshell/helm-chart
  --version "${OPEN_SHELL_HELM_VERSION}"
  --namespace openshell
  --create-namespace
)
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
openshell_helm_args+=(--values "${ROOT}/config/openshell/provider-token-grants.yaml")
if [[ -n "${OPEN_SHELL_SUPERVISOR_TOPOLOGY}" ]]; then
  openshell_helm_args+=(
    --set-string "supervisor.topology=${OPEN_SHELL_SUPERVISOR_TOPOLOGY}"
  )
fi
if [[ -n "${OPEN_SHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY}" ]]; then
  openshell_helm_args+=(
    --set "supervisor.sidecar.processBinaryAwareNetworkPolicy=${OPEN_SHELL_PROCESS_BINARY_AWARE_NETWORK_POLICY}"
  )
fi
openshell_helm_args+=(
  --set-string server.defaultRuntimeClassName=
  --set server.auth.allowUnauthenticatedUsers=false
  --set-string "server.oidc.issuer=${oidc_issuer}"
  --set-string "server.oidc.audience=${OIDC_AUDIENCE}"
  --set-string server.oidc.rolesClaim=roles
  --set-string server.oidc.adminRole=openshell-admin
  --set-string server.oidc.userRole=openshell-user
)
if [[ -n "${STEWARD_OPENSHELL_SANDBOX_IMAGE:-}" ]]; then
  openshell_helm_args+=("${sandbox_image_args[@]}")
fi
if [[ -n "${STEWARD_OPENSHELL_SUPERVISOR_IMAGE:-}" ]]; then
  openshell_helm_args+=("${supervisor_image_args[@]}")
fi
openshell_helm_args+=(--wait --timeout 5m)

env \
  HELM_CACHE_HOME="${RUN_DIR}/helm/cache" \
  HELM_CONFIG_HOME="${RUN_DIR}/helm/config" \
  HELM_DATA_HOME="${RUN_DIR}/helm/data" \
  helm "${openshell_helm_args[@]}"

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
bearer_token="${RUN_DIR}/openshell-bearer-token"
extract_secret_key openshell-client-tls ca.crt "${gateway_ca}"
extract_secret_key openshell-client-tls tls.crt "${client_certificate}"
extract_secret_key openshell-client-tls tls.key "${client_private_key}"
printf '%s' "${oidc_token}" >"${bearer_token}"
chmod 600 "${client_private_key}" "${bearer_token}"

workload_source_file="${RUN_DIR}/workload-source-credential"
workload_exchange_ca_private_key="${RUN_DIR}/workload-exchange-ca.key"
workload_exchange_ca_certificate="${RUN_DIR}/workload-exchange-ca.crt"
workload_exchange_private_key="${RUN_DIR}/workload-exchange.key"
workload_exchange_certificate_request="${RUN_DIR}/workload-exchange.csr"
workload_exchange_certificate="${RUN_DIR}/workload-exchange.crt"
workload_exchange_extensions="${RUN_DIR}/workload-exchange-extensions.cnf"
workload_exchange_log="${RUN_DIR}/workload-exchange.log"
printf '%s' obviously-fake-workload-source >"${workload_source_file}"
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
printf '%s\n' \
  'subjectAltName=IP:127.0.0.1' \
  'basicConstraints=critical,CA:FALSE' \
  'keyUsage=critical,digitalSignature,keyEncipherment' \
  'extendedKeyUsage=serverAuth' \
  >"${workload_exchange_extensions}"
openssl x509 \
  -req \
  -in "${workload_exchange_certificate_request}" \
  -CA "${workload_exchange_ca_certificate}" \
  -CAkey "${workload_exchange_ca_private_key}" \
  -CAcreateserial \
  -days 1 \
  -sha256 \
  -extfile "${workload_exchange_extensions}" \
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
    "https://localhost:${port}" >/dev/null
  then
    endpoint="https://localhost:${port}"
    break
  fi
  sleep 1
done
if [[ -z "${endpoint}" ]]; then
  echo "OpenShell authenticated TLS gateway did not become reachable" >&2
  cat "${PORT_FORWARD_LOG}" >&2
  exit 1
fi

export XDG_CONFIG_HOME="${RUN_DIR}/config"
gateway_directory="${XDG_CONFIG_HOME}/openshell/gateways/openshell"
mkdir -p "${gateway_directory}/mtls"
cp "${gateway_ca}" "${gateway_directory}/mtls/ca.crt"
cp "${client_certificate}" "${gateway_directory}/mtls/tls.crt"
cp "${client_private_key}" "${gateway_directory}/mtls/tls.key"
printf '%s\n' openshell >"${XDG_CONFIG_HOME}/openshell/active_gateway"
printf '{"name":"openshell","gateway_endpoint":"%s","is_remote":false,"gateway_port":%s,"auth_mode":"oidc","oidc_issuer":"%s","oidc_client_id":"openshell-cli","oidc_audience":"%s"}\n' \
  "${endpoint}" "${port}" "${oidc_issuer}" "${OIDC_AUDIENCE}" \
  >"${gateway_directory}/metadata.json"
printf '{"access_token":"%s","expires_at":%s,"issuer":"%s","client_id":"openshell-cli"}\n' \
  "${oidc_token}" "${expires_at}" "${oidc_issuer}" \
  >"${gateway_directory}/oidc_token.json"
chmod 600 \
  "${gateway_directory}/mtls/tls.key" \
  "${gateway_directory}/oidc_token.json"

export STEWARD_OPENSHELL_ENDPOINT="${endpoint}"
export STEWARD_OPENSHELL_CA_CERTIFICATE_FILE="${gateway_ca}"
export STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE="${client_certificate}"
export STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE="${client_private_key}"
export STEWARD_WORKLOAD_EXCHANGE_ENDPOINT="${workload_exchange_endpoint}"
export STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME="127.0.0.1"
export STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE="${workload_exchange_ca_certificate}"
export STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE="${workload_source_file}"
export STEWARD_TEST_OPENSHELL_ACCESS_TOKEN_FILE="${bearer_token}"
export STEWARD_OPENSHELL_SERVER_NAME="localhost"
export STEWARD_OPENSHELL_RUNTIME_CLASS_NAME=""
export STEWARD_TEST_KUBE_CONTEXT="${KUBE_CONTEXT}"
export STEWARD_TEST_KUBECONFIG="${KUBECONFIG_PATH}"
export STEWARD_RUN_DIR="${RUN_DIR}"
export KUBECONFIG="${KUBECONFIG_PATH}"

if [[ "$#" -eq 0 ]]; then
  echo "an in-cluster test command is required" >&2
  exit 2
fi

"$@"
