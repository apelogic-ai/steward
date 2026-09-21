#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rendered="$(mktemp)"
trap 'rm -f "${rendered}"' EXIT

customer_images=(
  --set-string images.repository=registry.example.test/customer/steward
  --set-string images.apiserver.tag=test-apiserver
  --set-string images.apiserver.digest=sha256:0000000000000000000000000000000000000000000000000000000000000000
  --set-string images.controller.tag=test-controller
  --set-string images.controller.digest=sha256:1111111111111111111111111111111111111111111111111111111111111111
)
helm_template() {
  helm template steward "${root}/charts/steward" --namespace steward "${customer_images[@]}" "$@"
}

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca > "${rendered}"
for forbidden in \
  'kind: ClusterSPIFFEID' \
  'name: steward-mint' \
  'STEWARD_OPENSHELL_' \
  'STEWARD_WORKLOAD_EXCHANGE_' \
  'STEWARD_LITELLM_' \
  'STEWARD_TASK_INFERENCE_ENDPOINT' \
  'SPIFFE_ENDPOINT_SOCKET' \
  'secretName: steward-openshell-client' \
  'name: steward-litellm'
do
  if rg -q "${forbidden}" "${rendered}"; then
    echo "core-only chart retained an execution prerequisite: ${forbidden}" >&2
    exit 1
  fi
done

if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set execution.enabled=false \
  --set config.taskOrchestrationMode=active > /dev/null 2>&1; then
  echo 'core-only chart must reject active Task orchestration' >&2
  exit 1
fi

if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set execution.enabled=false \
  --set config.apiserver.executionBindingsMode=active > /dev/null 2>&1; then
  echo 'core-only chart must reject active execution bindings' >&2
  exit 1
fi

governed_inputs=(
  --set-string tls.webhook.caBundlePem=public-test-ca
  --set execution.enabled=true
  --set-string images.mint.tag=test-mint
  --set-string images.mint.digest=sha256:2222222222222222222222222222222222222222222222222222222222222222
  --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1
  --set-string config.controller.openshellEndpoint=https://gateway.example.test:8080
  --set-string config.controller.openshellServerName=gateway.example.test
  --set-string config.controller.openshellRuntimeClassName=sandbox-vm
  --set-string config.controller.workloadExchangeEndpoint=https://identity.example.test/v1/workload/exchange
  --set-string config.controller.workloadExchangeServerName=identity.example.test
  --set-string config.controller.litellmUrl=https://litellm.example.test
)
if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set execution.enabled=true \
  --set-string images.mint.tag=test-mint \
  --set-string images.mint.digest=sha256:2222222222222222222222222222222222222222222222222222222222222222 \
  --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1 \
  --set-string config.controller.openshellEndpoint=https://gateway.example.test:8080 \
  --set-string config.controller.openshellServerName=gateway.example.test \
  --set-string config.controller.workloadExchangeEndpoint=https://identity.example.test/v1/workload/exchange \
  --set-string config.controller.workloadExchangeServerName=identity.example.test \
  --set-string config.controller.litellmUrl=https://litellm.example.test \
  --set-string config.mint.issuer=https://mint.example.test \
  --set-string config.mint.spiffeTrustDomain=customer.example.test \
  --set-string config.mint.openshellNamespace=customer-openshell > /dev/null 2> "${rendered}"; then
  echo 'governed execution requires an explicit RuntimeClass' >&2
  exit 1
fi
rg -q 'openshellRuntimeClassName' "${rendered}"
if helm_template "${governed_inputs[@]}" > /dev/null 2> "${rendered}"; then
  echo 'governed mode must not use an assumed Mint issuer or SPIFFE trust domain' >&2
  exit 1
fi
rg -q 'mint.issuer|spiffeTrustDomain' "${rendered}"
helm_template "${governed_inputs[@]}" \
  --set-string config.mint.issuer=https://mint.example.test \
  --set-string config.mint.spiffeTrustDomain=customer.example.test \
  --set-string config.mint.openshellNamespace=customer-openshell > "${rendered}"
rg -q 'kind: ClusterSPIFFEID' "${rendered}"
rg -q 'name: steward-mint' "${rendered}"

echo 'core-only chart dependency boundary passed'
