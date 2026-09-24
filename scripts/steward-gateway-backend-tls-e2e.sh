#!/usr/bin/env bash
# Exercise the chart's Gateway API backend-TLS contract against a real controller.
set -euo pipefail

kind_node_image="kindest/node:v1.32.1@sha256:6afef2b7f69d627ea7bf27ee6696b6868d18e03bf98167c420df486da4662db6"
envoy_gateway_chart="oci://docker.io/envoyproxy/gateway-helm"
envoy_gateway_version="v1.9.1"
envoy_gateway_chart_digest="sha256:91bae9aedb91ab34731e987afe01a3ccf454393015abeca705eea8ee15553e86"
envoy_gateway_release="eg"
nginx_image="nginx:1.27.5-alpine@sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10"
curl_image="curlimages/curl:8.12.1@sha256:94e9e444bcba979c2ea12e27ae39bee4cd10bc7041a472c4727a558e213744e6"
run_id="gateway-tls-$(date -u +%Y%m%d%H%M%S)-$$"
cluster="steward-${run_id}"
context="kind-${cluster}"
temporary_root="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
run_dir="$(mktemp -d "${temporary_root%/}/steward-${run_id}.XXXXXX")"
kubeconfig="${run_dir}/kubeconfig"
namespace="steward"
cluster_created=0
stage="preflight"

cleanup() {
  local status="$?"
  trap - EXIT INT TERM
  set +e
  if [[ "${status}" != 0 && "${cluster_created}" == 1 ]]; then
    kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
      -n "${namespace}" get gateway,httproute,backendtlspolicy,pods 2>/dev/null >&2
    kubectl --kubeconfig "${kubeconfig}" --context "${context}" \
      -n "${namespace}" get events --sort-by=.lastTimestamp 2>/dev/null | tail -30 >&2
  fi
  if [[ "${cluster_created}" == 1 ]]; then
    kind delete cluster --name "${cluster}" >/dev/null 2>&1 || status=1
  fi
  find "${run_dir}" -depth -delete 2>/dev/null || status=1
  if kind get clusters 2>/dev/null | grep -Fxq "${cluster}" || [[ -e "${run_dir}" ]]; then
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

set_stage() {
  stage="$1"
  printf 'Gateway backend TLS E2E stage: %s\n' "$stage"
}

for command in curl docker helm kind kubectl openssl; do
  command -v "${command}" >/dev/null || { echo "missing ${command}" >&2; exit 2; }
done
docker info >/dev/null
printf 'run_id=%s\ncluster=%s\ncontext=%s\nkubeconfig=%s\nrun_dir=%s\n' \
  "${run_id}" "${cluster}" "${context}" "${kubeconfig}" "${run_dir}" > "${run_dir}/ownership.txt"

set_stage cluster
printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\n' > "${run_dir}/kind.yaml"
cluster_created=1
kind create cluster --name "${cluster}" --kubeconfig "${kubeconfig}" \
  --config "${run_dir}/kind.yaml" --image "${kind_node_image}" --wait 120s
chmod 600 "${kubeconfig}"
if [[ "$(kubectl --kubeconfig "${kubeconfig}" config current-context)" != "${context}" ]]; then
  echo "unexpected disposable context" >&2
  exit 1
fi
K=(kubectl --kubeconfig "${kubeconfig}" --context "${context}")
"${K[@]}" create namespace "${namespace}" >/dev/null
"${K[@]}" label namespace "${namespace}" "steward.test/run-id=${run_id}" >/dev/null

set_stage gateway-controller
gateway_chart_pull="$(helm pull "${envoy_gateway_chart}" --version "${envoy_gateway_version}" \
  --destination "${run_dir}" 2>&1)"
if ! grep -Fxq "Digest: ${envoy_gateway_chart_digest}" <<<"${gateway_chart_pull}"; then
  echo "Envoy Gateway chart ${envoy_gateway_version} did not resolve to ${envoy_gateway_chart_digest}" >&2
  exit 1
fi
gateway_chart_archive="${run_dir}/gateway-helm-${envoy_gateway_version}.tgz"
if [[ ! -s "${gateway_chart_archive}" ]]; then
  echo "Envoy Gateway chart archive is missing: ${gateway_chart_archive}" >&2
  exit 1
fi
helm upgrade --install "${envoy_gateway_release}" "${gateway_chart_archive}" \
  --namespace envoy-gateway-system \
  --create-namespace --wait --timeout 5m >/dev/null
"${K[@]}" -n envoy-gateway-system rollout status deployment/envoy-gateway --timeout=300s

set_stage certificates
umask 077
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=steward gateway TLS test CA' \
  -keyout "${run_dir}/ca.key" -out "${run_dir}/ca.crt" >/dev/null 2>&1
for certificate_name in apiserver edge; do
  if [[ "${certificate_name}" == apiserver ]]; then
    dns_name="steward-apiserver.${namespace}.svc.cluster.local"
  else
    dns_name="steward.example.test"
  fi
  openssl req -newkey rsa:2048 -nodes -subj "/CN=${dns_name}" \
    -addext "subjectAltName=DNS:${dns_name}" \
    -keyout "${run_dir}/${certificate_name}.key" \
    -out "${run_dir}/${certificate_name}.csr" >/dev/null 2>&1
  openssl x509 -req -days 1 -in "${run_dir}/${certificate_name}.csr" \
    -CA "${run_dir}/ca.crt" -CAkey "${run_dir}/ca.key" -CAcreateserial \
    -copy_extensions copy -out "${run_dir}/${certificate_name}.crt" >/dev/null 2>&1
done
"${K[@]}" -n "${namespace}" create configmap steward-apiserver-ca \
  --from-file=ca.crt="${run_dir}/ca.crt" >/dev/null
"${K[@]}" -n "${namespace}" create configmap steward-edge-ca \
  --from-file=ca.crt="${run_dir}/ca.crt" >/dev/null
"${K[@]}" -n "${namespace}" create secret tls steward-apiserver-tls \
  --cert="${run_dir}/apiserver.crt" --key="${run_dir}/apiserver.key" >/dev/null
"${K[@]}" -n "${namespace}" create secret tls steward-edge-tls \
  --cert="${run_dir}/edge.crt" --key="${run_dir}/edge.key" >/dev/null

set_stage tls-backend
"${K[@]}" -n "${namespace}" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: gateway-tls-backend-nginx
  labels: {steward.test/run-id: ${run_id}}
data:
  nginx.conf: |
    events {}
    http {
      server {
        listen 8443 ssl;
        ssl_certificate /etc/nginx/tls/tls.crt;
        ssl_certificate_key /etc/nginx/tls/tls.key;
        location = /admin/api/v1/session { return 401; }
        location / { return 404; }
      }
    }
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: steward-apiserver-tls-backend
  labels: {steward.test/run-id: ${run_id}}
spec:
  replicas: 1
  selector:
    matchLabels: {app.kubernetes.io/name: steward, app.kubernetes.io/component: apiserver}
  template:
    metadata:
      labels: {app.kubernetes.io/name: steward, app.kubernetes.io/component: apiserver, steward.test/run-id: ${run_id}}
    spec:
      containers:
        - name: apiserver-tls-backend
          image: ${nginx_image}
          ports: [{name: https, containerPort: 8443}]
          volumeMounts:
            - {name: configuration, mountPath: /etc/nginx/nginx.conf, subPath: nginx.conf, readOnly: true}
            - {name: tls, mountPath: /etc/nginx/tls, readOnly: true}
      volumes:
        - {name: configuration, configMap: {name: gateway-tls-backend-nginx}}
        - {name: tls, secret: {secretName: steward-apiserver-tls}}
---
apiVersion: v1
kind: Service
metadata:
  name: steward-apiserver
  labels: {steward.test/run-id: ${run_id}}
spec:
  selector: {app.kubernetes.io/name: steward, app.kubernetes.io/component: apiserver}
  ports: [{name: https, appProtocol: https, port: 443, targetPort: https}]
YAML
"${K[@]}" -n "${namespace}" rollout status deployment/steward-apiserver-tls-backend --timeout=180s

set_stage route
"${K[@]}" -n "${namespace}" apply -f - >/dev/null <<YAML
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: steward-edge
  labels: {steward.test/run-id: ${run_id}}
spec:
  gatewayClassName: eg
  listeners:
    - name: https
      protocol: HTTPS
      port: 443
      hostname: steward.example.test
      tls:
        mode: Terminate
        certificateRefs: [{kind: Secret, name: steward-edge-tls}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata:
  name: steward-apiserver
  labels: {steward.test/run-id: ${run_id}}
spec:
  targetRefs:
    - group: ""
      kind: Service
      name: steward-apiserver
      sectionName: https
  validation:
    hostname: steward-apiserver.${namespace}.svc.cluster.local
    caCertificateRefs:
      - group: ""
        kind: ConfigMap
        name: steward-apiserver-ca
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: steward-api
  labels: {steward.test/run-id: ${run_id}}
spec:
  parentRefs: [{name: steward-edge, sectionName: https}]
  hostnames: [steward.example.test]
  rules:
    - matches: [{path: {type: PathPrefix, value: /admin/api}}]
      backendRefs: [{name: steward-apiserver, port: 443}]
YAML

wait_condition() {
  local resource="$1"
  local condition="$2"
  local attempt
  for ((attempt = 1; attempt <= 60; attempt += 1)); do
    if "${K[@]}" -n "${namespace}" get "${resource}" \
      -o "jsonpath={range .status.conditions[?(@.type=='${condition}')]}{.status}{end}" 2>/dev/null | grep -Fxq True; then
      return 0
    fi
    sleep 2
  done
  echo "${resource} did not report ${condition}=True" >&2
  return 1
}
wait_condition gateway/steward-edge Programmed
wait_route_condition() {
  local condition="$1"
  local attempt
  for ((attempt = 1; attempt <= 60; attempt += 1)); do
    if "${K[@]}" -n "${namespace}" get httproute/steward-api \
      -o "jsonpath={range .status.parents[*].conditions[?(@.type=='${condition}')]}{.status}{end}" 2>/dev/null | grep -Fxq True; then
      return 0
    fi
    sleep 2
  done
  echo "HTTPRoute/steward-api did not report ${condition}=True" >&2
  return 1
}
wait_route_condition Accepted
wait_route_condition ResolvedRefs
wait_policy_condition() {
  local condition="$1"
  local attempt
  for ((attempt = 1; attempt <= 60; attempt += 1)); do
    if "${K[@]}" -n "${namespace}" get backendtlspolicy/steward-apiserver \
      -o "jsonpath={range .status.ancestors[*].conditions[?(@.type=='${condition}')]}{.status}{end}" 2>/dev/null | grep -Fxq True; then
      return 0
    fi
    sleep 2
  done
  echo "BackendTLSPolicy/steward-apiserver did not report ${condition}=True" >&2
  return 1
}
wait_policy_condition Accepted
wait_policy_condition ResolvedRefs

set_stage public-session
gateway_service="$(${K[@]} get service -A \
  -l gateway.envoyproxy.io/owning-gateway-namespace=${namespace},gateway.envoyproxy.io/owning-gateway-name=steward-edge \
  -o jsonpath='{.items[0].metadata.namespace}{"/"}{.items[0].metadata.name}')"
if [[ ! "${gateway_service}" =~ ^[^/]+/[^/]+$ ]]; then
  echo 'Envoy Gateway did not publish an owned data-plane Service' >&2
  exit 1
fi
gateway_namespace="${gateway_service%/*}"
gateway_service_name="${gateway_service#*/}"
gateway_ip="$(${K[@]} -n "${gateway_namespace}" get service "${gateway_service_name}" -o jsonpath='{.spec.clusterIP}')"
if [[ ! "${gateway_ip}" =~ ^[0-9a-fA-F:.]+$ ]]; then
  echo 'Envoy Gateway data-plane Service has no usable ClusterIP' >&2
  exit 1
fi
"${K[@]}" -n "${namespace}" apply -f - >/dev/null <<YAML
apiVersion: batch/v1
kind: Job
metadata:
  name: steward-gateway-session
  labels: {steward.test/run-id: ${run_id}}
spec:
  backoffLimit: 0
  template:
    metadata:
      labels: {steward.test/run-id: ${run_id}}
    spec:
      restartPolicy: Never
      containers:
        - name: session
          image: ${curl_image}
          command:
            - /bin/sh
            - -ec
            - >-
              status=\$(curl --silent --show-error --output /dev/null --write-out '%{http_code}'
              --cacert /run/edge-ca/ca.crt --resolve steward.example.test:443:${gateway_ip}
              https://steward.example.test/admin/api/v1/session);
              test "\${status}" = 401
          volumeMounts:
            - {name: edge-ca, mountPath: /run/edge-ca, readOnly: true}
      volumes:
        - {name: edge-ca, configMap: {name: steward-edge-ca}}
YAML
"${K[@]}" -n "${namespace}" wait --for=condition=complete job/steward-gateway-session --timeout=180s
echo "Envoy Gateway ${envoy_gateway_version} returned Steward session status 401 through verified backend TLS"
