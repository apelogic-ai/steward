#!/usr/bin/env bash
# Disposable customer-style core install from released OCI artifacts only.
set -euo pipefail

build_release_value_sets() {
  local receiver_trigger='[{"id":"Jira_read","enabled":true,"source":"jira","workflow":"triage@1","service":"event-worker","ownerUserId":"usr_11111111111111111111111111111111","envelopeDigest":"steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","filters":{"event":"jira:issue_updated","project":"PROJ","issueType":"10001"},"ignoredAccountIds":["acct-1"],"concurrencyLimit":2,"cooldownSeconds":60,"signingSecret":{"name":"steward-jira-webhook","key":"secret"}}]'
  values=(
    --set-string spire.className=spire-spire
    --set-string "images.repository=${image_repository}"
    --set-string "images.apiserver.tag=${release_version}-apiserver"
    --set-string "images.apiserver.digest=${api_digest}"
    --set-string "images.controller.tag=${release_version}-controller"
    --set-string "images.controller.digest=${controller_digest}"
    --set-string "networkPolicy.kubeApiCidrs[0]=${api_ip}/32"
    --set-string "networkPolicy.postgresCidrs[0]=${postgres_ip}/32"
    --set-string secrets.database.name=steward-runtime
    --set-string tls.api.secretName=steward-runtime
    --set-string tls.webhook.secretName=steward-runtime
    --set-file "tls.webhook.caBundlePem=${run_dir}/ca.crt"
  )
  complete_values=(
    "${values[@]}"
    --set-json 'config.apiserver.stewardRunRelease={"manifestSchemaVersion":3,"version":"0.7.0","workflowRepository":"example-org/steward-run","workflowCommit":"3333333333333333333333333333333333333333","actionCommit":"4444444444444444444444444444444444444444"}'
    --set execution.enabled=true
    --set web.enabled=true
    --set browserAuth.enabled=true
    --set-string images.mint.tag="${release_version}-mint"
    --set-string images.mint.digest="${mint_digest}"
    --set triggerReceiver.enabled=true
    --set-string images.triggerReceiver.tag="${release_version}-trigger-receiver"
    --set-string images.triggerReceiver.digest="${trigger_receiver_digest}"
    --set-json "triggerReceiver.triggers=${receiver_trigger}"
    --set-string triggerReceiver.databaseSecret.name=steward-trigger-database
    --set-string triggerReceiver.databaseSecret.key=url
    --set-string triggerReceiver.workloadExchange.endpoint=https://identity.example.test/v1/workload/exchange
    --set-string triggerReceiver.workloadExchange.serverName=identity.example.test
    --set-string triggerReceiver.workloadExchange.audience=apelogic-workload-exchange
    --set-string triggerReceiver.taskApi.ca.kind=ConfigMap
    --set-string triggerReceiver.taskApi.ca.name=steward-apiserver-ca
    --set-string triggerReceiver.taskApi.ca.key=ca.crt
    --set-string networkPolicy.ingressNamespace=edge-system
    --set-string images.web.tag="${release_version}-web"
    --set-string images.web.digest="${web_digest}"
    --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1/responses
    --set-string config.apiserver.jiraBaseUrl=https://jira.example.com
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
    --set-string browserAuth.google.organizationId=org_example
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
    --set-string 'runtimeNamespaces[1]=steward-workflows'
  )
}

if [[ "${1:-}" == --lint-source-chart ]]; then
  source_chart="${2:?source chart path is required}"
  if [[ "$#" != 2 ]]; then
    echo 'usage: customer-core-install-e2e.sh --lint-source-chart <chart>' >&2
    exit 2
  fi
  lint_dir="$(mktemp -d)"
  trap 'find "${lint_dir}" -depth -delete' EXIT
  printf '%s\n' public-release-validation-ca > "${lint_dir}/ca.crt"
  run_dir="${lint_dir}"
  release_version="$(awk '$1 == "version:" { print $2; exit }' "${source_chart}/Chart.yaml")"
  image_repository=registry.example.test/steward
  api_digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  controller_digest=sha256:1111111111111111111111111111111111111111111111111111111111111111
  mint_digest=sha256:2222222222222222222222222222222222222222222222222222222222222222
  trigger_receiver_digest=sha256:3333333333333333333333333333333333333333333333333333333333333333
  bridge_digest=sha256:4444444444444444444444444444444444444444444444444444444444444444
  web_digest=sha256:5555555555555555555555555555555555555555555555555555555555555555
  api_ip=192.0.2.10
  postgres_ip=192.0.2.11
  build_release_value_sets
  helm lint "${source_chart}" "${values[@]}" >/dev/null
  helm lint "${source_chart}" "${complete_values[@]}" >/dev/null
  echo 'released-artifact core and complete source-chart profiles passed'
  exit 0
fi
if [[ "$#" != 0 ]]; then
  echo 'usage: customer-core-install-e2e.sh' >&2
  exit 2
fi

kind_node_image="kindest/node:v1.32.1@sha256:6afef2b7f69d627ea7bf27ee6696b6868d18e03bf98167c420df486da4662db6"
release_version="${STEWARD_RELEASE_VERSION:?STEWARD_RELEASE_VERSION is required}"
image_repository="${STEWARD_RELEASE_IMAGE_REPOSITORY:?STEWARD_RELEASE_IMAGE_REPOSITORY is required}"
image_pull_repository="${STEWARD_RELEASE_IMAGE_PULL_REPOSITORY:-${image_repository}}"
chart_repository="${STEWARD_RELEASE_CHART_REPOSITORY:?STEWARD_RELEASE_CHART_REPOSITORY is required}"
registry_mirror="${STEWARD_RELEASE_REGISTRY_MIRROR:-}"
plain_http="${STEWARD_RELEASE_PLAIN_HTTP:-false}"
chart_digest="${STEWARD_RELEASE_CHART_DIGEST:?STEWARD_RELEASE_CHART_DIGEST is required}"
api_digest="${STEWARD_RELEASE_APISERVER_DIGEST:?STEWARD_RELEASE_APISERVER_DIGEST is required}"
controller_digest="${STEWARD_RELEASE_CONTROLLER_DIGEST:?STEWARD_RELEASE_CONTROLLER_DIGEST is required}"
mint_digest="${STEWARD_RELEASE_MINT_DIGEST:?STEWARD_RELEASE_MINT_DIGEST is required}"
trigger_receiver_digest="${STEWARD_RELEASE_TRIGGER_RECEIVER_DIGEST:?STEWARD_RELEASE_TRIGGER_RECEIVER_DIGEST is required}"
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
  if [[ -n "${registry_mirror}" ]]; then
    docker network disconnect kind "${registry_mirror%%:*}" >/dev/null 2>&1 || status=1
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
for digest in "${chart_digest}" "${api_digest}" "${controller_digest}" "${mint_digest}" "${trigger_receiver_digest}" "${bridge_digest}" "${web_digest}"; do
  if [[ ! "${digest}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
    echo "release artifact has invalid digest: ${digest}" >&2
    exit 2
  fi
done
if [[ "${plain_http}" != false && "${plain_http}" != true ]]; then
  echo 'STEWARD_RELEASE_PLAIN_HTTP must be true or false' >&2
  exit 2
fi
if [[ -n "${registry_mirror}" && ! "${registry_mirror}" =~ ^[a-z0-9][a-z0-9.-]*:[0-9]+$ ]]; then
  echo 'STEWARD_RELEASE_REGISTRY_MIRROR must be a container hostname and port' >&2
  exit 2
fi
printf 'release_version=%s\ncluster=%s\ncontext=%s\nkubeconfig=%s\nrun_dir=%s\n' \
  "${release_version}" "${cluster}" "${context}" "${kubeconfig}" "${run_dir}" \
  > "${run_dir}/ownership.txt"
echo "core install release ${release_version}; owned cluster ${cluster}"

stage=release-artifacts
for digest in "${api_digest}" "${controller_digest}" "${mint_digest}" "${trigger_receiver_digest}" "${bridge_digest}" "${web_digest}"; do
  docker pull "${image_pull_repository}@${digest}" >/dev/null
done
helm_pull=(pull "${chart_repository}@${chart_digest}" --destination "${run_dir}")
if [[ "${plain_http}" == true ]]; then
  helm_pull+=(--plain-http)
fi
helm "${helm_pull[@]}"
chart_archive="$(find "${run_dir}" -maxdepth 1 -type f -name 'steward*.tgz' -print -quit)"
test -s "${chart_archive}"
tar -xOf "${chart_archive}" steward/Chart.yaml > "${run_dir}/Chart.yaml"
grep -Fxq "version: ${release_version}" "${run_dir}/Chart.yaml"
grep -Fxq "appVersion: ${release_version}" "${run_dir}/Chart.yaml"

stage=cluster
if [[ -n "${registry_mirror}" ]]; then
  printf '%s\n' \
    'kind: Cluster' \
    'apiVersion: kind.x-k8s.io/v1alpha4' \
    'containerdConfigPatches:' \
    '- |-' \
    "  [plugins.\"io.containerd.grpc.v1.cri\".registry.mirrors.\"${registry_mirror}\"]" \
    "    endpoint = [\"http://${registry_mirror}\"]" > "${run_dir}/kind.yaml"
else
  printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\n' > "${run_dir}/kind.yaml"
fi
cluster_created=1
kind create cluster \
  --name "${cluster}" --kubeconfig "${kubeconfig}" \
  --config "${run_dir}/kind.yaml" --image "${kind_node_image}" --wait 120s
if [[ -n "${registry_mirror}" ]]; then
  docker network connect kind "${registry_mirror%%:*}"
fi
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
openssl req -newkey rsa:2048 -nodes \
  -keyout "${run_dir}/steward-runtime.key" -out "${run_dir}/steward-runtime.csr" \
  -subj '/CN=steward-apiserver.steward.svc' >/dev/null 2>&1
cat > "${run_dir}/steward-runtime.ext" <<'EOF'
subjectAltName=DNS:steward-apiserver,DNS:steward-apiserver.steward.svc,DNS:steward-apiserver.steward.svc.cluster.local,DNS:steward-webhook,DNS:steward-webhook.steward.svc,DNS:steward-webhook.steward.svc.cluster.local
EOF
openssl x509 -req -in "${run_dir}/steward-runtime.csr" \
  -CA "${run_dir}/ca.crt" -CAkey "${run_dir}/ca.key" -CAcreateserial \
  -out "${run_dir}/steward-runtime.crt" -days 1 -sha256 \
  -extfile "${run_dir}/steward-runtime.ext" >/dev/null 2>&1
openssl verify -CAfile "${run_dir}/ca.crt" "${run_dir}/steward-runtime.crt" >/dev/null
K=(kubectl --kubeconfig "${kubeconfig}" --context "${context}" -n steward)
"${K[@]}" create secret generic steward-runtime \
  --from-file="url=${run_dir}/database-url" \
  --from-file="tls.crt=${run_dir}/steward-runtime.crt" \
  --from-file="tls.key=${run_dir}/steward-runtime.key" >/dev/null
"${K[@]}" create secret generic core-test-postgres \
  --from-file="password=${run_dir}/postgres-password" >/dev/null
"${K[@]}" label secret steward-runtime core-test-postgres \
  "steward.test/run-id=${run_id}" >/dev/null

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
build_release_value_sets
helm lint "${chart_archive}" "${complete_values[@]}" >/dev/null
helm template steward "${chart_archive}" --namespace steward \
  "${complete_values[@]}" > "${run_dir}/complete-rendered.yaml"
for image in \
  "${image_repository}:${release_version}-apiserver@${api_digest}" \
  "${image_repository}:${release_version}-controller@${controller_digest}" \
  "${image_repository}:${release_version}-mint@${mint_digest}" \
  "${image_repository}:${release_version}-trigger-receiver@${trigger_receiver_digest}" \
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
migration_counts="$("${K[@]}" exec deployment/core-test-postgres -- \
  psql -U steward -d steward -Atc \
  "SELECT count(*)::text || ':' || (count(*) FILTER (WHERE success))::text FROM _sqlx_migrations")"
if [[ ! "${migration_counts}" =~ ^([0-9]+):([0-9]+)$ ]] \
  || [[ "${BASH_REMATCH[1]}" == 0 ]] \
  || [[ "${BASH_REMATCH[1]}" != "${BASH_REMATCH[2]}" ]]; then
  echo "database migrations are incomplete: ${migration_counts}" >&2
  exit 1
fi

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
