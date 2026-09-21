#!/usr/bin/env bash
# Disposable customer-style core install: no Jira, inference, Mint, or OpenShell.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
run_id="core-$(date -u +%Y%m%d%H%M%S)-$$"
cluster="steward-${run_id}"
context="kind-${cluster}"
network="steward-${run_id}-net"
registry="steward-${run_id}-registry"
run_dir="$(mktemp -d "/private/tmp/steward-${run_id}.XXXXXX")"
chmod 700 "${run_dir}"
kubeconfig="${run_dir}/kubeconfig"
network_created=0
registry_created=0
cluster_created=0
port_forward_pid=""
api_image=""
controller_image=""
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
  if [[ "${registry_created}" == 1 ]]; then
    docker rm -f "${registry}" >/dev/null 2>&1 || status=1
  fi
  if [[ "${network_created}" == 1 ]]; then
    docker network rm "${network}" >/dev/null 2>&1 || status=1
  fi
  if [[ -n "${api_image}" ]]; then
    docker image rm "${api_image}" >/dev/null 2>&1 || status=1
  fi
  if [[ -n "${controller_image}" ]]; then
    docker image rm "${controller_image}" >/dev/null 2>&1 || status=1
  fi
  find "${run_dir}" -depth -delete 2>/dev/null || status=1
  if kind get clusters 2>/dev/null | grep -Fxq "${cluster}" \
    || docker container inspect "${registry}" >/dev/null 2>&1 \
    || docker network inspect "${network}" >/dev/null 2>&1 \
    || [[ -e "${run_dir}" ]]; then
    echo "owned disposable resources remain; check ${cluster}, ${registry}, ${network}, ${run_dir}" >&2
    status=1
  else
    echo "disposable cleanup verified: ${cluster}, ${registry}, ${network}, ${run_dir}" >&2
  fi
  exit "${status}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for tool in docker git helm kind kubectl openssl jq curl; do
  command -v "${tool}" >/dev/null || { echo "missing ${tool}" >&2; exit 2; }
done
docker info >/dev/null
if [[ -n "$(git -C "${root}" status --porcelain)" ]]; then
  echo 'customer install E2E requires a clean exact-revision checkout' >&2
  exit 2
fi
revision="$(git -C "${root}" rev-parse HEAD)"
printf 'revision=%s\ncluster=%s\ncontext=%s\nkubeconfig=%s\nregistry=%s\nnetwork=%s\nrun_dir=%s\n' \
  "${revision}" "${cluster}" "${context}" "${kubeconfig}" "${registry}" "${network}" "${run_dir}" \
  > "${run_dir}/ownership.txt"
echo "core install revision ${revision}; owned cluster ${cluster}"

stage=registry
docker network create "${network}" >/dev/null
network_created=1
registry_created=1
docker run -d --name "${registry}" --network "${network}" \
  -p 127.0.0.1::5000 \
  registry:2@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373 >/dev/null
registry_port="$(docker port "${registry}" 5000/tcp | awk -F: 'NR == 1 { print $NF }')"
if [[ ! "${registry_port}" =~ ^[0-9]+$ ]]; then
  echo 'registry did not publish a unique host port' >&2
  exit 1
fi
printf 'registry_host_port=%s\n' "${registry_port}" >> "${run_dir}/ownership.txt"
tag="${run_id}"
api_image="localhost:${registry_port}/steward:${tag}-apiserver"
controller_image="localhost:${registry_port}/steward:${tag}-controller"

stage=images
docker build --quiet -f "${root}/build/package.Dockerfile" \
  --build-arg BINARY=steward-apiserver-bin -t "${api_image}" "${root}" >/dev/null
docker build --quiet -f "${root}/build/package.Dockerfile" \
  --build-arg BINARY=steward-controller-bin -t "${controller_image}" "${root}" >/dev/null
docker push "${api_image}" >/dev/null
docker push "${controller_image}" >/dev/null
api_digest="$(docker image inspect "${api_image}" --format '{{index .RepoDigests 0}}')"
controller_digest="$(docker image inspect "${controller_image}" --format '{{index .RepoDigests 0}}')"
api_digest="${api_digest##*@}"
controller_digest="${controller_digest##*@}"
if [[ ! "${api_digest}" =~ ^sha256:[0-9a-f]{64}$ || ! "${controller_digest}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo 'local release images lack immutable OCI manifest digests' >&2
  exit 1
fi
printf 'api_digest=%s\ncontroller_digest=%s\n' "${api_digest}" "${controller_digest}" >> "${run_dir}/ownership.txt"
echo "published local immutable images ${api_digest} and ${controller_digest}"

stage=cluster
printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\n' > "${run_dir}/kind.yaml"
cluster_created=1
KIND_EXPERIMENTAL_DOCKER_NETWORK="${network}" kind create cluster \
  --name "${cluster}" --kubeconfig "${kubeconfig}" \
  --config "${run_dir}/kind.yaml" --wait 120s
chmod 600 "${kubeconfig}"
actual_context="$(kubectl --kubeconfig "${kubeconfig}" config current-context)"
if [[ "${actual_context}" != "${context}" ]]; then
  echo "unexpected disposable context ${actual_context}" >&2
  exit 1
fi
stage=registry-binding
node="${cluster}-control-plane"
# New Kind/containerd nodes already set registry.config_path. Adding a legacy
# registry.mirrors patch disables CRI, so use the supported per-host file.
docker exec "${node}" crictl info >/dev/null
printf 'server = "http://%s:5000"\n[host."http://%s:5000"]\n  capabilities = ["pull", "resolve"]\n' \
  "${registry}" "${registry}" |
  docker exec -i "${node}" sh -c 'mkdir -p "$1" && cat > "$1/hosts.toml"' \
    _ "/etc/containerd/certs.d/${registry}:5000"
docker exec "${node}" crictl pull "${registry}:5000/steward:${tag}-apiserver@${api_digest}" >/dev/null
docker exec "${node}" crictl pull "${registry}:5000/steward:${tag}-controller@${controller_digest}" >/dev/null
kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
  create namespace steward >/dev/null
kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
  label namespace steward "steward.test/run-id=${run_id}" >/dev/null

stage=credentials
umask 077
openssl rand -hex 24 > "${run_dir}/postgres-password"
postgres_password="$(<"${run_dir}/postgres-password")"
printf 'postgres://steward:%s@core-test-postgres.steward.svc.cluster.local:5432/steward?sslmode=disable' \
  "${postgres_password}" > "${run_dir}/database-url"
unset postgres_password
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
  --set-string "images.repository=${registry}:5000/steward"
  --set-string "images.apiserver.tag=${tag}-apiserver"
  --set-string "images.apiserver.digest=${api_digest}"
  --set-string "images.controller.tag=${tag}-controller"
  --set-string "images.controller.digest=${controller_digest}"
  --set-string "networkPolicy.kubeApiCidrs[0]=${api_ip}/32"
  --set-string "networkPolicy.postgresCidrs[0]=${postgres_ip}/32"
  --set-file "tls.webhook.caBundlePem=${run_dir}/ca.crt"
)
helm lint "${root}/charts/steward" "${values[@]}" >/dev/null
helm template steward "${root}/charts/steward" --namespace steward \
  "${values[@]}" > "${run_dir}/rendered.yaml"
if grep -Eq 'STEWARD_JIRA_|STEWARD_LITELLM_|STEWARD_OPENSHELL_|kind: ClusterSPIFFEID|name: steward-mint' \
  "${run_dir}/rendered.yaml"; then
  echo 'core render retained optional governed/Jira dependencies' >&2
  exit 1
fi
helm --kubeconfig "${kubeconfig}" --kube-context "${context}" \
  upgrade --install steward "${root}/charts/steward" --namespace steward \
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
curl --silent --show-error --fail --noproxy '*' \
  --cacert "${run_dir}/ca.crt" \
  --connect-to "steward-apiserver.steward.svc.cluster.local:443:127.0.0.1:${forward_port}" \
  https://steward-apiserver.steward.svc.cluster.local/health/ready >/dev/null
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
echo "core install delivery passed: ${revision}, ${api_digest}, ${controller_digest}"
