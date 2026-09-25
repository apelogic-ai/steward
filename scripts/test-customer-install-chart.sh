#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
run_dir="$(mktemp -d)"
trap 'rm -f "${run_dir}/default.yaml" "${run_dir}/cert-manager.yaml" "${run_dir}/jira.yaml" "${run_dir}/customer.yaml" "${run_dir}/database-secret.yaml" "${run_dir}/database-configmap.yaml"; rmdir "${run_dir}"' EXIT

# The release validator selects its strict customer contract only for a
# deliberately versioned chart; a missing or stale marker must fail the handoff.
if ! grep -Eq '^version: (0\.1\.23|0\.2\.3)$' "${root}/charts/steward/Chart.yaml"; then
  echo 'installation contract supports only the v0.1.23 transition base or v0.2.4' >&2
  exit 1
fi
grep -Fxq '  steward.apelogic.ai/customer-install-contract: steward.customer-install/v1' \
  "${root}/charts/steward/Chart.yaml"

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
  --set-string images.apiserver.digest=sha256:0000000000000000000000000000000000000000000000000000000000000000 \
  --set-string tls.webhook.caBundlePem=public-test-ca > /dev/null 2>&1; then
  echo 'installation contract must reject an all-zero apiserver image digest' >&2
  exit 1
fi

if helm_template > /dev/null 2>&1; then
  echo 'customer TLS mode must reject a missing webhook CA before install' >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" --namespace steward \
  --set-string tls.webhook.caBundlePem=public-test-ca > /dev/null 2>&1; then
  echo 'default chart must not assume ApeLogic image coordinates' >&2
  exit 1
fi
helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca > "${run_dir}/default.yaml"
if rg -q 'STEWARD_JIRA_|secretName: steward-jira|kind: Certificate|cert-manager.io/inject-ca-from' "${run_dir}/default.yaml"; then
  echo 'default install must not require Jira or cert-manager' >&2
  exit 1
fi

if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-string databaseTls.mode=verify-full > /dev/null 2>&1; then
  echo 'verified database TLS must reject an incomplete CA source' >&2
  exit 1
fi
helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-string databaseTls.mode=verify-full \
  --set-string databaseTls.ca.kind=Secret \
  --set-string databaseTls.ca.name=steward-postgres-ca \
  --set-string databaseTls.ca.key=ca.pem > "${run_dir}/database-secret.yaml"
test "$(rg -c 'mountPath: /run/database-tls, readOnly: true' "${run_dir}/database-secret.yaml")" = 2
test "$(rg -c 'secretName: steward-postgres-ca' "${run_dir}/database-secret.yaml")" = 2
test "$(rg -c 'key: ca.pem, path: ca.crt' "${run_dir}/database-secret.yaml")" = 2

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-string databaseTls.mode=verify-full \
  --set-string databaseTls.ca.kind=ConfigMap \
  --set-string databaseTls.ca.name=steward-postgres-ca \
  --set-string databaseTls.ca.key=ca.pem > "${run_dir}/database-configmap.yaml"
test "$(rg -c 'name: database-tls-ca' "${run_dir}/database-configmap.yaml")" = 4
test "$(rg -c 'name: steward-postgres-ca' "${run_dir}/database-configmap.yaml")" = 2
if rg -q 'jira.example.com|sandbox-vm|cluster-issuer' "${run_dir}/default.yaml"; then
  echo 'default install contains environment-specific placeholders' >&2
  exit 1
fi
if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set jira.enabled=true > /dev/null 2>&1; then
  echo 'enabled Jira without configuration must fail before install' >&2
  exit 1
fi
if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set jira.enabled=true \
  --set config.apiserver.jiraBaseUrl=http://jira.example.test \
  --set config.apiserver.jiraProjectKey=PROJ \
  --set config.apiserver.jiraAccountEmail=alice@example.com > /dev/null 2>&1; then
  echo 'enabled Jira must reject plaintext transport' >&2
  exit 1
fi
if helm_template \
  --set tls.mode=certManager > /dev/null 2>&1; then
  echo 'cert-manager mode requires an explicit issuer' >&2
  exit 1
fi
helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-string tls.api.secretName=customer-api-tls \
  --set-string tls.webhook.secretName=customer-webhook-tls > "${run_dir}/customer.yaml"
rg -q 'name: steward-webhook-ca' "${run_dir}/customer.yaml"
rg -q 'caBundle: "cHVibGljLXRlc3QtY2E="' "${run_dir}/customer.yaml"
rg -q 'secretName: customer-api-tls' "${run_dir}/customer.yaml"
rg -q 'secretName: customer-webhook-tls' "${run_dir}/customer.yaml"

helm_template \
  --set tls.mode=certManager \
  --set tls.issuerRef.name=customer-issuer \
  --set tls.issuerRef.kind=Issuer > "${run_dir}/cert-manager.yaml"
test "$(rg -c 'kind: Certificate' "${run_dir}/cert-manager.yaml")" = 2
rg -q 'cert-manager.io/inject-ca-from' "${run_dir}/cert-manager.yaml"

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set jira.enabled=true \
  --set 'networkPolicy.jiraCidrs[0]=192.0.2.0/24' \
  --set config.apiserver.jiraBaseUrl=https://jira.example.test \
  --set config.apiserver.jiraProjectKey=PROJ \
  --set config.apiserver.jiraAccountEmail=alice@example.com > "${run_dir}/jira.yaml"
rg -q 'STEWARD_JIRA_TOKEN' "${run_dir}/jira.yaml"
rg -q 'name: steward-jira' "${run_dir}/jira.yaml"
controller_policy="$(awk '/name: steward-controller-egress/ { capture = 1 } capture { print } capture && /^---$/ { exit }' "${run_dir}/jira.yaml")"
if ! rg -q '192.0.2.0/24' <<< "${controller_policy}"; then
  echo 'enabled Jira needs explicit controller egress' >&2
  exit 1
fi

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set 'networkPolicy.jiraCidrs[0]=192.0.2.0/24' > "${run_dir}/default.yaml"
if rg -q '192.0.2.0/24' "${run_dir}/default.yaml"; then
  echo 'disabled Jira must not receive an egress allowance' >&2
  exit 1
fi

echo 'customer install chart modes passed'
