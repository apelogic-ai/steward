#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rendered="$(mktemp)"
missing_namespace_error="$(mktemp)"
trap 'rm -f "${rendered}" "${missing_namespace_error}"' EXIT

customer_images=(
  --set-string images.repository=registry.example.test/customer/steward
  --set-string images.apiserver.tag=test-apiserver
  --set-string images.apiserver.digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  --set-string images.controller.tag=test-controller
  --set-string images.controller.digest=sha256:1111111111111111111111111111111111111111111111111111111111111111
)
helm_template() {
  helm template steward "${root}/charts/steward" --namespace steward "${customer_images[@]}" "$@"
}

if helm_template \
  --set-string images.controller.digest=sha256:0000000000000000000000000000000000000000000000000000000000000000 \
  --set-string tls.webhook.caBundlePem=public-test-ca > /dev/null 2>&1; then
  echo 'core install must reject an all-zero controller image digest' >&2
  exit 1
fi

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca > "${rendered}"
if rg -q 'STEWARD_STARTER_TASK_JSON' "${rendered}"; then
  echo 'unset starter task must use the apiserver built-in without an environment override' >&2
  exit 1
fi
bash "${root}/scripts/customer-core-install-e2e.sh" \
  --lint-source-chart "${root}/charts/steward"

starter_task_json='{"taskDefinition":{"schemaVersion":"steward.task-definition/v2","name":"hello-world","version":2,"runtime":{"agentRef":"codex@0.140.0"},"promptText":"Write hello world to out/hello.txt.","outputs":[{"path":"out/hello.txt","kind":"file","required":true}]},"inputs":{"greeting":"hello"},"executionLog":"full","packagePath":".steward/tasks/hello-world/task-definition.json","title":"Hello world","description":"A deployment-owned starter task.","git":{"repository":"https://github.com/example-org/agentic-ops.git","revision":"git:ref:main","path":"catalog/hello/task-definition.json"},"publishedWorkflow":"repo-summary@2"}'
helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-json "config.apiserver.starterTask=${starter_task_json}" > "${rendered}"
rg -q 'STEWARD_STARTER_TASK_JSON' "${rendered}"
rg -q '\.steward/tasks/hello-world/task-definition\.json' "${rendered}"
rg -q 'Write hello world to out/hello\.txt' "${rendered}"

if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-json "config.apiserver.starterTask=${starter_task_json}" \
  --set-string config.apiserver.starterTask.packagePath=catalog/hello/task-definition.json > /dev/null 2>&1; then
  echo 'starter task must reject a non-canonical repository package path' >&2
  exit 1
fi

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
  --set-string 'runtimeNamespaces[0]=steward-workflows'
  --set-string images.mint.tag=test-mint
  --set-string images.mint.digest=sha256:2222222222222222222222222222222222222222222222222222222222222222
  --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1/responses
  --set-string config.controller.openshellEndpoint=https://gateway.example.test:8080
  --set-string config.controller.openshellServerName=gateway.example.test
  --set-string config.controller.openshellRuntimeClassName=sandbox-vm
  --set-string config.controller.workloadExchangeEndpoint=https://identity.example.test/v1/workload/exchange
  --set-string config.controller.workloadExchangeServerName=identity.example.test
  --set-string config.controller.litellmUrl=https://litellm.example.test
  --set-string spire.className=steward
)
default_runtime_inputs=(
  --set-string tls.webhook.caBundlePem=public-test-ca
  --set execution.enabled=true
  --set-string images.mint.tag=test-mint
  --set-string images.mint.digest=sha256:2222222222222222222222222222222222222222222222222222222222222222
  --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1/responses
  --set-string config.controller.openshellEndpoint=https://gateway.example.test:8080
  --set-string config.controller.openshellServerName=gateway.example.test
  --set-string config.controller.workloadExchangeEndpoint=https://identity.example.test/v1/workload/exchange
  --set-string config.controller.workloadExchangeServerName=identity.example.test
  --set-string config.controller.litellmUrl=https://litellm.example.test
  --set-string config.mint.issuer=https://mint.example.test
  --set-string config.mint.spiffeTrustDomain=customer.example.test
  --set-string config.mint.openshellNamespace=customer-openshell
  --set-string spire.className=steward
)
if helm_template "${default_runtime_inputs[@]}" > /dev/null 2>"${missing_namespace_error}"; then
  echo 'governed execution must reject a missing steward-workflows runtime namespace' >&2
  exit 1
fi
grep -Fq 'execution.enabled requires steward-workflows in runtimeNamespaces' \
  "${missing_namespace_error}"
helm_template "${default_runtime_inputs[@]}" \
  --set-string 'runtimeNamespaces[0]=steward-workflows' > "${rendered}"
if rg -q 'STEWARD_OPENSHELL_RUNTIME_CLASS_NAME' "${rendered}"; then
  echo 'default runtime render must not request a RuntimeClass' >&2
  exit 1
fi
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
