#!/usr/bin/env bash
# Disposable customer-style core install from released OCI artifacts only.
set -euo pipefail

kind_node_image="kindest/node:v1.32.1@sha256:6afef2b7f69d627ea7bf27ee6696b6868d18e03bf98167c420df486da4662db6"
release_version="${STEWARD_RELEASE_VERSION:?STEWARD_RELEASE_VERSION is required}"
image_repository="${STEWARD_RELEASE_IMAGE_REPOSITORY:?STEWARD_RELEASE_IMAGE_REPOSITORY is required}"
chart_repository="${STEWARD_RELEASE_CHART_REPOSITORY:?STEWARD_RELEASE_CHART_REPOSITORY is required}"
chart_digest="${STEWARD_RELEASE_CHART_DIGEST:?STEWARD_RELEASE_CHART_DIGEST is required}"
api_digest="${STEWARD_RELEASE_APISERVER_DIGEST:?STEWARD_RELEASE_APISERVER_DIGEST is required}"
controller_digest="${STEWARD_RELEASE_CONTROLLER_DIGEST:?STEWARD_RELEASE_CONTROLLER_DIGEST is required}"
mint_digest="${STEWARD_RELEASE_MINT_DIGEST:?STEWARD_RELEASE_MINT_DIGEST is required}"
bridge_digest="${STEWARD_RELEASE_BRIDGE_DIGEST:?STEWARD_RELEASE_BRIDGE_DIGEST is required}"
web_digest="${STEWARD_RELEASE_WEB_DIGEST:?STEWARD_RELEASE_WEB_DIGEST is required}"
run_id="core-$(date -u +%Y%m%d%H%M%S)-$$"
cluster="steward-${run_id}"
context="kind-${cluster}"
temp_root="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
run_dir="$(mktemp -d "${temp_root%/}/steward-${run_id}.XXXXXX")"
chmod 700 "${run_dir}"
kubeconfig="${run_dir}/kubeconfig"
cluster_created=0
port_forward_pid=""
stage=preflight

cleanup() {
  local status="$?"
  trap - EXIT INT TERM
  set +e
  if [[ "${status}" != 0 ]]; then
    echo "core install failed at ${stage} (exit ${status})" >&2
    if [[ "${cluster_created}" == 1 ]]; then
      kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
        -n steward get pods -o wide 2>/dev/null >&2
      kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
        -n steward get events --sort-by=.lastTimestamp 2>/dev/null | tail -30 >&2
    fi
  fi
  if [[ -n "${port_forward_pid}" ]]; then
    kill "${port_forward_pid}" >/dev/null 2>&1 || true
    wait "${port_forward_pid}" >/dev/null 2>&1 || true
  fi
  if [[ "${cluster_created}" == 1 ]]; then
    kind delete cluster --name "${cluster}" >/dev/null 2>&1 || status=1
  fi
  find "${run_dir}" -depth -delete 2>/dev/null || status=1
  if kind get clusters 2>/dev/null | grep -Fxq "${cluster}" \
    || [[ -e "${run_dir}" ]]; then
    echo "owned disposable resources remain; check ${cluster} and ${run_dir}" >&2
    status=1
  else
    echo "disposable cleanup verified: ${cluster}, ${run_dir}" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for tool in docker helm kind kubectl openssl jq curl tar; do
  command -v "${tool}" >/dev/null || { echo "missing ${tool}" >&2; exit 2; }
done
docker info >/dev/null
for digest in "${chart_digest}" "${api_digest}" "${controller_digest}" "${mint_digest}" "${bridge_digest}" "${web_digest}"; do
  if [[ ! "${digest}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
    echo "release artifact has invalid digest: ${digest}" >&2
    exit 2
  fi
done
printf 'release_version=%s\ncluster=%s\ncontext=%s\nkubeconfig=%s\nrun_dir=%s\n' \
  "${release_version}" "${cluster}" "${context}" "${kubeconfig}" "${run_dir}" \
  > "${run_dir}/ownership.txt"
echo "core install release ${release_version}; owned cluster ${cluster}"

stage=release-artifacts
for digest in "${api_digest}" "${controller_digest}" "${mint_digest}" "${bridge_digest}" "${web_digest}"; do
  docker pull "${image_repository}@${digest}" >/dev/null
done
helm pull "${chart_repository}@${chart_digest}" --destination "${run_dir}"
chart_archive="$(find "${run_dir}" -maxdepth 1 -type f -name 'steward*.tgz' -print -quit)"
test -s "${chart_archive}"
tar -xOf "${chart_archive}" steward/Chart.yaml > "${run_dir}/Chart.yaml"
grep -Fxq "version: ${release_version}" "${run_dir}/Chart.yaml"
grep -Fxq "appVersion: ${release_version}" "${run_dir}/Chart.yaml"

stage=cluster
printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\n' > "${run_dir}/kind.yaml"
cluster_created=1
kind create cluster \
  --name "${cluster}" --kubeconfig "${kubeconfig}" \
  --config "${run_dir}/kind.yaml" --image "${kind_node_image}" --wait 120s
chmod 600 "${kubeconfig}"
actual_context="$(kubectl --kubeconfig "${kubeconfig}" config current-context)"
if [[ "${actual_context}" != "${context}" ]]; then
  echo "unexpected disposable context ${actual_context}" >&2
  exit 1
fi
kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
  create namespace steward >/dev/null
kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
  label namespace steward "steward.test/run-id=${run_id}" >/dev/null

stage=credentials
umask 077
openssl rand -hex 24 > "${run_dir}/postgres-password"
{
  printf 'postgres://steward:'
  tr -d '\n' < "${run_dir}/postgres-password"
  printf '@core-test-postgres.steward.svc.cluster.local:5432/steward?sslmode=disable'
} > "${run_dir}/database-url"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -keyout "${run_dir}/ca.key" -out "${run_dir}/ca.crt" \
  -subj '/CN=Steward disposable core install CA' >/dev/null 2>&1
for service in steward-apiserver steward-webhook; do
  openssl req -newkey rsa:2048 -nodes \
    -keyout "${run_dir}/${service}.key" -out "${run_dir}/${service}.csr" \
    -subj "/CN=${service}.steward.svc" >/dev/null 2>&1
  printf 'subjectAltName=DNS:%s,DNS:%s.steward.svc,DNS:%s.steward.svc.cluster.local\n' \
    "${service}" "${service}" "${service}" > "${run_dir}/${service}.ext"
  openssl x509 -req -in "${run_dir}/${service}.csr" \
    -CA "${run_dir}/ca.crt" -CAkey "${run_dir}/ca.key" -CAcreateserial \
    -out "${run_dir}/${service}.crt" -days 1 -sha256 \
    -extfile "${run_dir}/${service}.ext" >/dev/null 2>&1
  openssl verify -CAfile "${run_dir}/ca.crt" "${run_dir}/${service}.crt" >/dev/null
done
K=(kubectl --kubeconfig "${kubeconfig}" --context "${context}" -n steward)
"${K[@]}" create secret generic steward-database \
  --from-file="url=${run_dir}/database-url" >/dev/null
"${K[@]}" create secret generic core-test-postgres \
  --from-file="password=${run_dir}/postgres-password" >/dev/null
"${K[@]}" create secret tls steward-apiserver-tls \
  --cert="${run_dir}/steward-apiserver.crt" \
  --key="${run_dir}/steward-apiserver.key" >/dev/null
"${K[@]}" create secret tls steward-webhook-tls \
  --cert="${run_dir}/steward-webhook.crt" \
  --key="${run_dir}/steward-webhook.key" >/dev/null
"${K[@]}" label secret steward-database core-test-postgres \
  steward-apiserver-tls steward-webhook-tls "steward.test/run-id=${run_id}" >/dev/null

stage=postgres
"${K[@]}" apply -f - >/dev/null <<YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: core-test-postgres
  labels: {steward.test/run-id: ${run_id}}
spec:
  replicas: 1
  selector: {matchLabels: {app: core-test-postgres}}
  template:
    metadata:
      labels: {app: core-test-postgres, steward.test/run-id: ${run_id}}
    spec:
      containers:
        - name: postgres
          image: postgres:16-alpine@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777
          env:
            - {name: POSTGRES_USER, value: steward}
            - {name: POSTGRES_DB, value: steward}
            - {name: POSTGRES_PASSWORD_FILE, value: /run/postgres/password}
          ports: [{containerPort: 5432}]
          volumeMounts: [{name: password, mountPath: /run/postgres, readOnly: true}]
          readinessProbe:
            exec: {command: [pg_isready, -U, steward]}
            periodSeconds: 3
      volumes:
        - name: password
          secret: {secretName: core-test-postgres}
---
apiVersion: v1
kind: Service
metadata:
  name: core-test-postgres
  labels: {steward.test/run-id: ${run_id}}
spec:
  selector: {app: core-test-postgres}
  ports: [{port: 5432, targetPort: 5432}]
YAML
"${K[@]}" rollout status deployment/core-test-postgres --timeout=180s
postgres_ip="$("${K[@]}" get pod -l app=core-test-postgres -o jsonpath='{.items[0].status.podIP}')"
api_ip="$(kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
  -n default get service kubernetes -o jsonpath='{.spec.clusterIP}')"

stage=install
values=(
  --set-string "images.repository=${image_repository}"
  --set-string "images.apiserver.tag=${release_version}-apiserver"
  --set-string "images.apiserver.digest=${api_digest}"
  --set-string "images.controller.tag=${release_version}-controller"
  --set-string "images.controller.digest=${controller_digest}"
  --set-string "networkPolicy.kubeApiCidrs[0]=${api_ip}/32"
  --set-string "networkPolicy.postgresCidrs[0]=${postgres_ip}/32"
  --set-file "tls.webhook.caBundlePem=${run_dir}/ca.crt"
)
complete_values=(
  "${values[@]}"
  --set execution.enabled=true
  --set web.enabled=true
  --set browserAuth.enabled=true
  --set-string images.mint.tag="${release_version}-mint"
  --set-string images.mint.digest="${mint_digest}"
  --set-string images.web.tag="${release_version}-web"
  --set-string images.web.digest="${web_digest}"
  --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1
  --set-string config.controller.openshellEndpoint=https://openshell.example.test
  --set-string config.controller.openshellServerName=openshell.example.test
  --set-string config.controller.workloadExchangeEndpoint=https://identity.example.test/v1/workload/exchange
  --set-string config.controller.workloadExchangeServerName=identity.example.test
  --set-string config.controller.litellmUrl=https://litellm.example.test
  --set-string config.mint.issuer=https://mint.example.test
  --set-string config.mint.spiffeTrustDomain=example.test
  --set-string config.mint.openshellNamespace=openshell
  --set-string browserAuth.google.clientId=obviously-fake-client-id
  --set-string browserAuth.google.origin=https://steward.example.test
  --set-string browserAuth.google.workspaceDomain=example.test
  --set-string browserAuth.google.organizationId=example-org
  --set-string browserAuth.google.clientSecret.name=steward-browser-auth
  --set-string browserAuth.google.clientSecret.key=client-secret
  --set-string 'networkPolicy.browserAuthEgressCidrs[0]=192.0.2.0/24'
  --set connectionsBridge.enabled=true
  --set-string connectionsBridge.artifactTrust.mode=operator-pinned
  --set-string connectionsBridge.image="${image_repository}@${bridge_digest}"
  --set-string connectionsBridge.mcpGatewayOrigin=https://mcp-gw.example.test
  --set-string connectionsBridge.mcpGatewayVersion=0.4.9
  --set-string connectionsBridge.runtimeNamespace=steward-runtimes
  --set-string 'runtimeNamespaces[0]=steward-runtimes'
)
helm lint "${chart_archive}" "${complete_values[@]}" >/dev/null
helm template steward "${chart_archive}" --namespace steward \
  "${complete_values[@]}" > "${run_dir}/complete-rendered.yaml"
for image in \
  "${image_repository}:${release_version}-apiserver@${api_digest}" \
  "${image_repository}:${release_version}-controller@${controller_digest}" \
  "${image_repository}:${release_version}-mint@${mint_digest}" \
  "${image_repository}:${release_version}-web@${web_digest}" \
  "${image_repository}@${bridge_digest}"
do
  grep -Fq "${image}" "${run_dir}/complete-rendered.yaml" || {
    echo "complete chart render omitted released artifact ${image}" >&2
    exit 1
  }
done

helm lint "${chart_archive}" "${values[@]}" >/dev/null
helm template steward "${chart_archive}" --namespace steward \
  "${values[@]}" > "${run_dir}/rendered.yaml"
if grep -Eq 'STEWARD_JIRA_|STEWARD_LITELLM_|STEWARD_OPENSHELL_|kind: ClusterSPIFFEID|name: steward-mint' \
  "${run_dir}/rendered.yaml"; then
  echo 'core render retained optional governed/Jira dependencies' >&2
  exit 1
fi
helm --kubeconfig "${kubeconfig}" --kube-context "${context}" \
  upgrade --install steward "${chart_archive}" --namespace steward \
  --atomic --wait --timeout 10m "${values[@]}" >/dev/null
"${K[@]}" rollout status deployment/steward-apiserver --timeout=180s
"${K[@]}" rollout status deployment/steward-controller --timeout=180s

stage=delivery
kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
  get crd agentruntimes.agents.apelogic.ai -o json |
  jq -e 'any(.status.conditions[]?; .type == "Established" and .status == "True")' >/dev/null
kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
  get validatingwebhookconfiguration steward-agentruntime -o json |
  jq -e 'all(.webhooks[]; .failurePolicy == "Fail" and
      .clientConfig.service.name == "steward-webhook" and
      ((.clientConfig.caBundle // "") | length > 0))' >/dev/null
if "${K[@]}" get deployment/steward-mint >/dev/null 2>&1 \
  || "${K[@]}" get secret/steward-jira >/dev/null 2>&1; then
  echo 'core installation unexpectedly requires Mint or Jira' >&2
  exit 1
fi
"${K[@]}" get deployment steward-apiserver steward-controller -o json |
  jq -e 'all(.items[].spec.template.spec.containers[].env[]?;
      .name != "STEWARD_JIRA_TOKEN" and
      .name != "STEWARD_TASK_INFERENCE_ENDPOINT" and
      .name != "STEWARD_LITELLM_MASTER_KEY")' >/dev/null
"${K[@]}" port-forward service/steward-apiserver :443 \
  --address 127.0.0.1 > "${run_dir}/port-forward.log" 2>&1 &
port_forward_pid="$!"
forward_port=""
for _attempt in {1..30}; do
  forward_port="$(sed -nE 's/.*127\.0\.0\.1:([0-9]+) ->.*/\1/p' "${run_dir}/port-forward.log" | head -1)"
  [[ -n "${forward_port}" ]] && break
  sleep 1
done
if [[ ! "${forward_port}" =~ ^[0-9]+$ ]]; then
  echo 'API port-forward did not become ready' >&2
  exit 1
fi
api_status="$(curl --silent --show-error --noproxy '*' \
  --cacert "${run_dir}/ca.crt" \
  --connect-to "steward-apiserver.steward.svc.cluster.local:443:127.0.0.1:${forward_port}" \
  --output /dev/null --write-out '%{http_code}' \
  https://steward-apiserver.steward.svc.cluster.local/admin/api/v1/runs)"
if [[ "${api_status}" != 401 ]]; then
  echo "protected API returned ${api_status}, expected unauthenticated 401 over verified TLS" >&2
  exit 1
fi
kill "${port_forward_pid}" >/dev/null 2>&1 || true
wait "${port_forward_pid}" >/dev/null 2>&1 || true
port_forward_pid=""

if "${K[@]}" create --dry-run=server -f - \
  > "${run_dir}/admission-check.log" 2>&1 <<'YAML'
apiVersion: agents.apelogic.ai/v1alpha1
kind: AgentRuntime
metadata:
  name: core-invalid-admission-probe
  namespace: steward
spec:
  principal: {kind: service, name: core-install-probe}
  owner: probe@example.test
  agentType: {name: core-install-probe}
  llms: []
  tools: []
  budget: {monthlyLimit: "0", currency: USD}
  ttl: forever
YAML
then
  echo 'invalid AgentRuntime unexpectedly passed server-side dry-run admission' >&2
  exit 1
fi
if ! grep -Fq 'admission webhook "agentruntime.steward.agents.apelogic.ai" denied the request' \
  "${run_dir}/admission-check.log"; then
  echo 'invalid AgentRuntime was not demonstrably rejected by Steward admission' >&2
  exit 1
fi
echo "released core install passed: ${release_version}, ${chart_digest}, ${api_digest}, ${controller_digest}"
