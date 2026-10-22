#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rendered="$(mktemp)"
component_rendered="$(mktemp)"
http_route_default_rendered="$(mktemp)"
http_route_timeout_rendered="$(mktemp)"
compatibility_directory="$(mktemp -d)"
inline_config_directory="$(mktemp -d)"
missing_namespace_error="$(mktemp)"
receiver_startup_config="$(mktemp)"
receiver_startup_output="$(mktemp)"
receiver_stub_port="$(mktemp)"
receiver_stub_kubeconfig="$(mktemp)"
receiver_stub_pid=""
receiver_process_pid=""
cleanup() {
  status="$?"
  trap - EXIT INT TERM
  if [[ -n "${receiver_process_pid}" ]]; then
    kill "${receiver_process_pid}" 2>/dev/null || true
    wait "${receiver_process_pid}" 2>/dev/null || true
  fi
  if [[ -n "${receiver_stub_pid}" ]]; then
    kill "${receiver_stub_pid}" 2>/dev/null || true
    wait "${receiver_stub_pid}" 2>/dev/null || true
  fi
  rm -f "${rendered}" "${component_rendered}" "${http_route_default_rendered}" \
    "${http_route_timeout_rendered}" "${missing_namespace_error}" "${receiver_startup_config}" \
    "${receiver_startup_output}" "${receiver_stub_port}" "${receiver_stub_kubeconfig}"
  find "${compatibility_directory}" -depth -delete
  find "${inline_config_directory}" -depth -delete
  exit "${status}"
}
trap cleanup EXIT INT TERM

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

tag_only_images=(
  --set images.requireDigest=false
  --set-string images.repository=
  --set-string images.apiserver.repository=registry.example.test/platform/steward-apiserver
  --set-string images.apiserver.tag=release-apiserver
  --set-string images.apiserver.digest=
  --set-string images.controller.repository=registry.example.test/platform/steward-controller
  --set-string images.controller.tag=release-controller
  --set-string images.controller.digest=
  --set-string images.mint.repository=registry.example.test/platform/steward-mint
  --set-string images.mint.tag=release-mint
  --set-string images.mint.digest=
  --set-string images.web.repository=registry.example.test/platform/steward-web
  --set-string images.web.tag=release-web
  --set-string images.web.digest=
  --set-string images.triggerReceiver.repository=registry.example.test/platform/steward-trigger-receiver
  --set-string images.triggerReceiver.tag=release-trigger-receiver
  --set-string images.triggerReceiver.digest=
)
legacy_complete_images=(
  --set-string images.mint.tag=release-mint
  --set-string images.mint.digest=sha256:2222222222222222222222222222222222222222222222222222222222222222
  --set-string images.web.tag=release-web
  --set-string images.web.digest=sha256:3333333333333333333333333333333333333333333333333333333333333333
  --set-string images.triggerReceiver.tag=release-trigger-receiver
  --set-string images.triggerReceiver.digest=sha256:4444444444444444444444444444444444444444444444444444444444444444
)
receiver_trigger='[{"id":"Jira_read","enabled":true,"source":"jira","workflow":"triage@1","service":"event-worker","ownerUserId":"usr_11111111111111111111111111111111","envelopeDigest":"steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","filters":{"event":"jira:issue_updated","project":"PROJ","issueType":"10001"},"ignoredAccountIds":["acct-1"],"concurrencyLimit":2,"cooldownSeconds":60,"signingSecret":{"name":"steward-jira-webhook","key":"secret"}}]'
complete_profile=(
  --set execution.enabled=true
  --set web.enabled=true
  --set browserAuth.enabled=true
  --set triggerReceiver.enabled=true
  --set-json "triggerReceiver.triggers=${receiver_trigger}"
  --set-string triggerReceiver.databaseSecret.name=steward-trigger-database
  --set-string triggerReceiver.databaseSecret.key=url
  --set-string triggerReceiver.workloadExchange.endpoint=https://identity.example.test/v1/workload/exchange
  --set-string triggerReceiver.workloadExchange.serverName=identity.example.test
  --set-string triggerReceiver.workloadExchange.audience=steward-workload-exchange
  --set-string triggerReceiver.taskApi.ca.kind=ConfigMap
  --set-string triggerReceiver.taskApi.ca.name=steward-apiserver-ca
  --set-string triggerReceiver.taskApi.ca.key=ca.crt
  --set-string spire.className=spire-spire
  --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1/responses
  --set-json 'config.apiserver.stewardRunRelease={"manifestSchemaVersion":3,"version":"0.7.0","workflowRepository":"example-org/steward-run","workflowCommit":"3333333333333333333333333333333333333333","actionCommit":"4444444444444444444444444444444444444444"}'
  --set-string config.controller.openshellEndpoint=https://openshell.example.test
  --set-string config.controller.openshellServerName=openshell.example.test
  --set-string config.controller.workloadExchangeEndpoint=https://identity.example.test/v1/workload/exchange
  --set-string config.controller.workloadExchangeServerName=identity.example.test
  --set-string config.controller.litellmUrl=https://inference.example.test
  --set-string config.mint.issuer=https://mint.example.test
  --set-string config.mint.spiffeTrustDomain=example.test
  --set-string config.mint.openshellNamespace=openshell
  --set-string 'runtimeNamespaces[0]=steward-workflows'
  --set-string browserAuth.google.clientId=obviously-fake-client-id
  --set-string browserAuth.google.origin=https://steward.example.test
  --set-string browserAuth.google.workspaceDomain=example.test
  --set-string browserAuth.google.organizationId=org_example
  --set-string browserAuth.google.clientSecret.name=steward-browser-auth
  --set-string browserAuth.google.clientSecret.key=client-secret
  --set-string networkPolicy.ingressNamespace=edge-system
  --set-string 'networkPolicy.kubeApiCidrs[0]=192.0.2.10/32'
  --set-string 'networkPolicy.postgresCidrs[0]=192.0.2.11/32'
  --set-string 'networkPolicy.browserAuthEgressCidrs[0]=192.0.2.12/32'
)
http_route_profile=(
  --set-string tls.webhook.caBundlePem=public-test-ca
  --set-string web.host=steward.example.test
  --set web.httpRoute.enabled=true
  --set-string web.httpRoute.hostname=steward.example.test
  --set-string 'web.httpRoute.parentRefs[0].name=shared-gateway'
  --set-string 'web.httpRoute.parentRefs[0].namespace=edge-system'
  --set-string 'web.httpRoute.parentRefs[0].sectionName=https'
  --set-string 'web.httpRoute.apiPaths[0].type=Exact'
  --set-string 'web.httpRoute.apiPaths[0].value=/.well-known/oauth-protected-resource'
  --set-string 'web.httpRoute.apiPaths[1].type=PathPrefix'
  --set-string 'web.httpRoute.apiPaths[1].value=/admin/api'
  --set-string 'web.httpRoute.apiPaths[2].type=PathPrefix'
  --set-string 'web.httpRoute.apiPaths[2].value=/admin/auth'
  --set-string 'web.httpRoute.apiPaths[3].type=Exact'
  --set-string 'web.httpRoute.apiPaths[3].value=/admin/connections/github/callback'
  --set-string 'web.httpRoute.apiPaths[4].type=PathPrefix'
  --set-string 'web.httpRoute.apiPaths[4].value=/admin/operator'
  --set-string 'web.httpRoute.apiPaths[5].type=PathPrefix'
  --set-string 'web.httpRoute.apiPaths[5].value=/app/api'
  --set-string 'web.httpRoute.apiPaths[6].type=PathPrefix'
  --set-string 'web.httpRoute.apiPaths[6].value=/v1'
  --set-string 'web.httpRoute.webPaths[0].type=PathPrefix'
  --set-string 'web.httpRoute.webPaths[0].value=/'
  --set-string web.httpRoute.backendTls.hostname=steward-apiserver.steward.svc.cluster.local
  --set-string web.httpRoute.backendTls.caConfigMap.name=steward-apiserver-ca
  --set-string web.httpRoute.backendTls.caConfigMap.key=ca.crt
)

helm_template \
  "${legacy_complete_images[@]}" \
  "${complete_profile[@]}" \
  "${http_route_profile[@]}" > "${http_route_default_rendered}"
if grep -Fq '      timeouts:' "${http_route_default_rendered}"; then
  echo 'HTTPRoute timeouts must remain absent unless explicitly configured' >&2
  exit 1
fi

helm_template \
  "${legacy_complete_images[@]}" \
  "${complete_profile[@]}" \
  "${http_route_profile[@]}" \
  --set-string web.httpRoute.apiTimeouts.request=90s \
  --set-string web.httpRoute.apiTimeouts.backendRequest=85s \
  --set-string web.httpRoute.webTimeouts.request=30s \
  --set-string web.httpRoute.webTimeouts.backendRequest=25s > "${http_route_timeout_rendered}"
api_route="$(awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: HTTPRoute/ && $0 ~ /name: steward-api/ { print; exit }
' "${http_route_timeout_rendered}")"
web_route="$(awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: HTTPRoute/ && $0 ~ /name: steward-web/ { print; exit }
' "${http_route_timeout_rendered}")"
grep -Fq 'request: "90s"' <<<"${api_route}"
grep -Fq 'backendRequest: "85s"' <<<"${api_route}"
grep -Fq 'request: "30s"' <<<"${web_route}"
grep -Fq 'backendRequest: "25s"' <<<"${web_route}"

for timeout_value in \
  web.httpRoute.apiTimeouts.request \
  web.httpRoute.apiTimeouts.backendRequest \
  web.httpRoute.webTimeouts.request \
  web.httpRoute.webTimeouts.backendRequest
do
  if helm_template \
    "${legacy_complete_images[@]}" \
    "${complete_profile[@]}" \
    "${http_route_profile[@]}" \
    --set-string "${timeout_value}=90seconds" >/dev/null 2>&1
  then
    echo "HTTPRoute ${timeout_value} must reject malformed durations" >&2
    exit 1
  fi
done

helm_template \
  "${tag_only_images[@]}" \
  "${complete_profile[@]}" \
  --set-string tls.webhook.caBundlePem=public-test-ca > "${component_rendered}"
for image in \
  registry.example.test/platform/steward-apiserver:release-apiserver \
  registry.example.test/platform/steward-controller:release-controller \
  registry.example.test/platform/steward-mint:release-mint \
  registry.example.test/platform/steward-web:release-web \
  registry.example.test/platform/steward-trigger-receiver:release-trigger-receiver
do
  if [[ "$(grep -cF "image: ${image}" "${component_rendered}")" != 1 ]]; then
    echo "tag-only component image must appear in exactly one Deployment: ${image}" >&2
    exit 1
  fi
done
if grep -Eq 'image: registry\.example\.test/platform/steward-[^[:space:]]+@' \
  "${component_rendered}"
then
  echo 'tag-only component images must not render a digest separator' >&2
  exit 1
fi

if helm_template \
  --set-string images.apiserver.repository=registry.example.test/platform/steward-apiserver \
  --set-string images.apiserver.digest= \
  --set-string tls.webhook.caBundlePem=public-test-ca >/dev/null 2>&1
then
  echo 'digest-required images must reject a tag-only component image' >&2
  exit 1
fi

if helm_template \
  "${tag_only_images[@]}" \
  --set-string images.apiserver.repository=Registry.example.test/platform/steward-apiserver \
  --set-string tls.webhook.caBundlePem=public-test-ca >/dev/null 2>&1
then
  echo 'component images must reject an invalid repository' >&2
  exit 1
fi

if helm_template \
  "${tag_only_images[@]}" \
  --set-string images.apiserver.tag=release/apiserver \
  --set-string tls.webhook.caBundlePem=public-test-ca >/dev/null 2>&1
then
  echo 'component images must reject an invalid tag' >&2
  exit 1
fi

app_version="$(awk '$1 == "appVersion:" { gsub(/"/, "", $2); print $2; exit }' "${root}/charts/steward/Chart.yaml")"
helm_template \
  --set images.requireDigest=false \
  --set-string images.apiserver.repository=registry.example.test/platform/steward-apiserver \
  --set-string images.apiserver.tag= \
  --set-string images.apiserver.digest= \
  --set-string images.controller.repository=registry.example.test/platform/steward-controller \
  --set-string images.controller.tag= \
  --set-string images.controller.digest= \
  --set-string tls.webhook.caBundlePem=public-test-ca > "${component_rendered}"
grep -Fq "image: registry.example.test/platform/steward-apiserver:${app_version}" \
  "${component_rendered}"
grep -Fq "image: registry.example.test/platform/steward-controller:${app_version}" \
  "${component_rendered}"

compatibility_base="${STEWARD_CHART_COMPAT_BASE:-origin/main}"
if git -C "${root}" cat-file -e "${compatibility_base}:charts/steward/Chart.yaml" 2>/dev/null; then
  git -C "${root}" archive "${compatibility_base}" charts/steward | \
    tar -x -C "${compatibility_directory}"
  helm template steward "${compatibility_directory}/charts/steward" --namespace steward \
    "${customer_images[@]}" \
    --set-string tls.webhook.caBundlePem=public-test-ca > "${compatibility_directory}/base-core.yaml"
  helm_template \
    --set-string tls.webhook.caBundlePem=public-test-ca > "${compatibility_directory}/head-core.yaml"
  if ! cmp -s "${compatibility_directory}/base-core.yaml" "${compatibility_directory}/head-core.yaml"; then
    diff -u "${compatibility_directory}/base-core.yaml" "${compatibility_directory}/head-core.yaml" >&2 || true
    echo 'existing core profile render changed' >&2
    exit 1
  fi
  helm template steward "${compatibility_directory}/charts/steward" --namespace steward \
    "${customer_images[@]}" \
    "${legacy_complete_images[@]}" \
    "${complete_profile[@]}" \
    --set-string tls.webhook.caBundlePem=public-test-ca > "${compatibility_directory}/base-complete.yaml"
  helm_template \
    "${legacy_complete_images[@]}" \
    "${complete_profile[@]}" \
    --set-string tls.webhook.caBundlePem=public-test-ca > "${compatibility_directory}/head-complete.yaml"
  if ! cmp -s "${compatibility_directory}/base-complete.yaml" "${compatibility_directory}/head-complete.yaml"; then
    diff -u "${compatibility_directory}/base-complete.yaml" \
      "${compatibility_directory}/head-complete.yaml" >&2 || true
    echo 'existing complete profile render changed' >&2
    exit 1
  fi
else
  echo "chart compatibility base is unavailable: ${compatibility_base}" >&2
  exit 1
fi

printf '%s\n' \
  '{"keys":[{"kty":"EC","use":"sig","alg":"ES256","kid":"test-key","crv":"P-256","x":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","y":"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"}]}' \
  > "${inline_config_directory}/jwks.json"
printf '%s\n' \
  '{"keys":[{"kty":"EC","use":"sig","alg":"ES256","kid":"rotated-key","crv":"P-256","x":"CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC","y":"DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD"}]}' \
  > "${inline_config_directory}/rotated-jwks.json"
printf '%s\n' '{"keys":[' > "${inline_config_directory}/malformed-jwks.json"
printf '%s\n' '{"keys":[{}]}' > "${inline_config_directory}/incomplete-jwks.json"
printf '%s\n' \
  '{"keys":[{"kty":"EC","use":"sig","alg":"ES256","kid":"test-key","crv":"P-256","x":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","y":"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB","d":"private"}]}' \
  > "${inline_config_directory}/private-jwks.json"

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set taskIdentity.enabled=true \
  --set-string taskIdentity.issuer=https://identity.example.test \
  --set-string taskIdentity.audience=steward-task-api \
  --set-string taskIdentity.publicJwksConfigMap.name=steward-task-jwks \
  --set-string taskIdentity.publicJwksConfigMap.key=jwks.json \
  --set-file taskIdentity.publicJwksConfigMap.data="${inline_config_directory}/jwks.json" \
  > "${component_rendered}"
grep -Fq 'metadata: { name: steward-task-jwks }' "${component_rendered}"
grep -Fq '"jwks.json":' "${component_rendered}"
grep -Fq '"alg":"ES256"' "${component_rendered}"
jwks_config_map="$(awk '/^metadata: \{ name: steward-task-jwks \}/ { found = 1 } found { print } found && /^---$/ { exit }' "${component_rendered}")"
if grep -Fq 'immutable: true' <<< "${jwks_config_map}"; then
  echo 'inline public JWKS ConfigMap must support key rotation' >&2
  exit 1
fi
jwks_checksum="$(awk '$1 == "checksum/task-jwks:" { gsub(/"/, "", $2); print $2; exit }' "${component_rendered}")"
helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set taskIdentity.enabled=true \
  --set-string taskIdentity.issuer=https://identity.example.test \
  --set-string taskIdentity.audience=steward-task-api \
  --set-string taskIdentity.publicJwksConfigMap.name=steward-task-jwks \
  --set-string taskIdentity.publicJwksConfigMap.key=jwks.json \
  --set-file taskIdentity.publicJwksConfigMap.data="${inline_config_directory}/rotated-jwks.json" \
  > "${component_rendered}"
rotated_jwks_checksum="$(awk '$1 == "checksum/task-jwks:" { gsub(/"/, "", $2); print $2; exit }' "${component_rendered}")"
if [[ -z "${jwks_checksum}" || -z "${rotated_jwks_checksum}" || "${jwks_checksum}" == "${rotated_jwks_checksum}" ]]; then
  echo 'inline public JWKS rotation must change the apiserver pod checksum' >&2
  exit 1
fi

for invalid_jwks in malformed-jwks.json incomplete-jwks.json private-jwks.json; do
  if helm_template \
    --set-string tls.webhook.caBundlePem=public-test-ca \
    --set taskIdentity.enabled=true \
    --set-string taskIdentity.issuer=https://identity.example.test \
    --set-string taskIdentity.audience=steward-task-api \
    --set-string taskIdentity.publicJwksConfigMap.name=steward-task-jwks \
    --set-string taskIdentity.publicJwksConfigMap.key=jwks.json \
    --set-file taskIdentity.publicJwksConfigMap.data="${inline_config_directory}/${invalid_jwks}" \
    >/dev/null 2>&1
  then
    echo "inline public JWKS must reject ${invalid_jwks}" >&2
    exit 1
  fi
done

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -keyout "${inline_config_directory}/private.key" \
  -out "${inline_config_directory}/ca.crt" \
  -subj '/CN=Steward test CA' >/dev/null 2>&1
printf '%s\n' '-----BEGIN CERTIFICATE-----' 'not-base64' '-----END CERTIFICATE-----' \
  > "${inline_config_directory}/malformed-ca.crt"

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-string web.httpRoute.backendTls.caConfigMap.name=steward-apiserver-ca \
  --set-string web.httpRoute.backendTls.caConfigMap.key=ca.crt \
  --set-file web.httpRoute.backendTls.caConfigMap.data="${inline_config_directory}/ca.crt" \
  > "${component_rendered}"
grep -Fq 'metadata: { name: steward-apiserver-ca }' "${component_rendered}"
grep -Fq -- '-----BEGIN CERTIFICATE-----' "${component_rendered}"
ca_config_map="$(awk '/^metadata: \{ name: steward-apiserver-ca \}/ { found = 1 } found { print } found && /^---$/ { exit }' "${component_rendered}")"
if grep -Fq 'immutable: true' <<< "${ca_config_map}"; then
  echo 'inline public CA ConfigMap must support certificate rotation' >&2
  exit 1
fi

if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-file web.httpRoute.backendTls.caConfigMap.data="${inline_config_directory}/ca.crt" \
  >/dev/null 2>&1
then
  echo 'inline public CA must require a ConfigMap name' >&2
  exit 1
fi

for invalid_ca in malformed-ca.crt private.key; do
  if helm_template \
    --set-string tls.webhook.caBundlePem=public-test-ca \
    --set-string web.httpRoute.backendTls.caConfigMap.name=steward-apiserver-ca \
    --set-string web.httpRoute.backendTls.caConfigMap.key=ca.crt \
    --set-file web.httpRoute.backendTls.caConfigMap.data="${inline_config_directory}/${invalid_ca}" \
    >/dev/null 2>&1
  then
    echo "inline public CA must reject ${invalid_ca}" >&2
    exit 1
  fi
done

helm_template \
  --set-string secrets.database.name=steward-runtime \
  --set-string tls.api.secretName=steward-runtime \
  --set-string tls.webhook.secretName=steward-runtime \
  --set-string tls.webhook.caBundlePem=public-test-ca > "${component_rendered}"
if [[ "$(grep -oE 'secretName: [A-Za-z0-9.-]+' "${component_rendered}" | awk '{print $2}' | sort -u)" != steward-runtime ]]; then
  echo 'core chart must support one existing Secret for database and TLS material' >&2
  exit 1
fi

if helm_template \
  --set-string images.controller.digest=sha256:0000000000000000000000000000000000000000000000000000000000000000 \
  --set-string tls.webhook.caBundlePem=public-test-ca > /dev/null 2>&1; then
  echo 'core install must reject an all-zero controller image digest' >&2
  exit 1
fi

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca > "${rendered}"
if grep -qF 'steward-trigger-receiver' "${rendered}"; then
  echo 'core install must not render the Jira Trigger receiver by default' >&2
  exit 1
fi
if grep -qF 'STEWARD_STARTER_TASK_JSON' "${rendered}"; then
  echo 'unset starter task must use the apiserver built-in without an environment override' >&2
  exit 1
fi
bash "${root}/scripts/customer-core-install-e2e.sh" \
  --lint-source-chart "${root}/charts/steward"

browser_release='{"actionCommit":"4444444444444444444444444444444444444444","manifestSchemaVersion":3,"version":"0.7.0","workflowCommit":"3333333333333333333333333333333333333333","workflowRepository":"example-org/steward-run"}'
browser_values=(
  --set-string tls.webhook.caBundlePem=public-test-ca
  --set browserAuth.enabled=true
  --set-string browserAuth.google.clientId=google-client-id
  --set-string browserAuth.google.origin=https://steward.example.test
  --set-string browserAuth.google.workspaceDomain=example.test
  --set-string browserAuth.google.organizationId=org_example
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc
  --set-string browserAuth.google.clientSecret.key=client-secret
  --set-json "config.apiserver.stewardRunRelease=${browser_release}"
  --set-string 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24'
)
user_token_values=(
  --set-string browserAuth.userTokens.issuer=https://issuer.example.test
  --set-string browserAuth.userTokens.hyperShellAudience=steward-api
  --set-string 'browserAuth.userTokens.frontendClientIds[0]=portal-client'
)
helm_template "${browser_values[@]}" "${user_token_values[@]}" >/dev/null
if helm_template "${browser_values[@]}" "${user_token_values[@]}" \
  --set-string browserAuth.userTokens.portalAudience=portal-audience >/dev/null 2>&1
then
  echo 'browser user tokens must reject a portal audience without its opt-in' >&2
  exit 1
fi
if helm_template "${browser_values[@]}" "${user_token_values[@]}" \
  --set browserAuth.userTokens.acceptPortalAudience=true >/dev/null 2>&1
then
  echo 'browser user tokens must reject a portal opt-in without its audience' >&2
  exit 1
fi
if helm_template "${browser_values[@]}" \
  --set-string browserAuth.userTokens.issuer=https://issuer.example.test \
  --set-string browserAuth.userTokens.hyperShellAudience=steward-api >/dev/null 2>&1
then
  echo 'browser user tokens must reject enabled settings without a frontend client' >&2
  exit 1
fi

starter_task_json='{"taskDefinition":{"schemaVersion":"steward.task-definition/v2","name":"hello-world","version":2,"runtime":{"agentRef":"codex@0.140.0"},"promptText":"Write hello world to out/hello.txt.","outputs":[{"path":"out/hello.txt","kind":"file","required":true}]},"inputs":{"greeting":"hello"},"executionLog":"full","packagePath":".steward/tasks/hello-world/task-definition.json","title":"Hello world","description":"A deployment-owned starter task.","git":{"repository":"https://github.com/example-org/agentic-ops.git","revision":"git:ref:main","path":"catalog/hello/task-definition.json"},"publishedWorkflow":"repo-summary@2"}'
helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-json "config.apiserver.starterTask=${starter_task_json}" > "${rendered}"
grep -qF 'STEWARD_STARTER_TASK_JSON' "${rendered}"
grep -qF '.steward/tasks/hello-world/task-definition.json' "${rendered}"
grep -qF 'Write hello world to out/hello.txt' "${rendered}"

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
  if grep -qF "${forbidden}" "${rendered}"; then
    echo "core-only chart retained an execution prerequisite: ${forbidden}" >&2
    exit 1
  fi
done

receiver_trigger='[{"id":"Jira_read","enabled":true,"source":"jira","workflow":"triage@1","service":"event-worker","ownerUserId":"usr_11111111111111111111111111111111","envelopeDigest":"steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","filters":{"event":"jira:issue_updated","project":"PROJ","issueType":"10001"},"ignoredAccountIds":["acct-1"],"concurrencyLimit":2,"cooldownSeconds":60,"signingSecret":{"name":"steward-jira-webhook","key":"secret"}}]'
receiver_inputs=(
  --set-string tls.webhook.caBundlePem=public-test-ca
  --set triggerReceiver.enabled=true
  --set-string images.triggerReceiver.tag=test-trigger-receiver
  --set-string images.triggerReceiver.digest=sha256:3333333333333333333333333333333333333333333333333333333333333333
  --set-json "triggerReceiver.triggers=${receiver_trigger}"
  --set-string config.apiserver.jiraBaseUrl=https://jira.example.com
  --set-string triggerReceiver.databaseSecret.name=steward-trigger-database
  --set-string triggerReceiver.databaseSecret.key=url
  --set-string triggerReceiver.workloadExchange.endpoint=https://identity.identity-exchange.svc.cluster.local:8443/v1/workload/exchange
  --set-string triggerReceiver.workloadExchange.serverName=identity.identity-exchange.svc.cluster.local
  --set-string triggerReceiver.workloadExchange.audience=apelogic-workload-exchange
  --set-string triggerReceiver.taskApi.ca.kind=ConfigMap
  --set-string triggerReceiver.taskApi.ca.name=steward-apiserver-ca
  --set-string triggerReceiver.taskApi.ca.key=ca.crt
  --set-string networkPolicy.ingressNamespace=edge-system
  --set-string 'networkPolicy.kubeApiCidrs[0]=192.0.2.1/32'
  --set-string 'networkPolicy.postgresCidrs[0]=192.0.2.2/32'
)
helm_template "${receiver_inputs[@]}" > "${rendered}"
for required in \
  'name: steward-trigger-receiver' \
  'resourceNames: ["steward-trigger-receiver"]' \
  'resources: ["serviceaccounts/token"]' \
  'checksum/trigger-config:' \
  'STEWARD_TRIGGER_CONFIG_FILE' \
  'STEWARD_CAPABILITY_CATALOG_FILE' \
  'STEWARD_WORKLOAD_EXCHANGE_ENDPOINT' \
  'STEWARD_API_CA_CERTIFICATE_FILE' \
  'STEWARD_TRIGGER_MAX_CONCURRENCY' \
  'name: steward-trigger-database' \
  'name: steward-jira-webhook' \
  'name: steward-trigger-receiver-ingress' \
  'name: steward-trigger-receiver-egress'
do
  grep -qF "${required}" "${rendered}" || {
    echo "enabled Jira Trigger receiver omitted: ${required}" >&2
    exit 1
  }
done
grep -qF 'https://steward-apiserver.steward.svc.cluster.local:443' "${rendered}"
grep -qF 'steward-apiserver.steward.svc.cluster.local' "${rendered}"
grep -qF '/run/trigger-signing/Jira_read' "${rendered}"
apiserver_ingress="$(awk '/^metadata: \{ name: steward-apiserver-ingress \}/ { found = 1 } found { print } found && /^---$/ { exit }' "${rendered}")"
grep -qF 'app.kubernetes.io/component: trigger-receiver' <<< "${apiserver_ingress}"

receiver_config="$(
  awk '/^metadata: \{ name: steward-trigger-receiver-config/ { found = 1 } found && /^  config.json: / { sub(/^  config.json: /, ""); print; exit }' "${rendered}" |
    jq -r .
)"
inline_env_value() {
  sed -n "/name: ${1},/s/.*value: \"\([^\"]*\)\".*/\1/p" "${rendered}" | head -1
}
receiver_numeric_env_names=(
  STEWARD_TRIGGER_MAX_CONFIG_BYTES
  STEWARD_TRIGGER_MAX_BODY_BYTES
  STEWARD_TRIGGER_MAX_IDENTIFIER_BYTES
  STEWARD_TRIGGER_MAX_TRIGGERS
  STEWARD_TRIGGER_MAX_IGNORED_ACCOUNTS
  STEWARD_TRIGGER_MAX_CONCURRENCY
  STEWARD_TRIGGER_MAX_COOLDOWN_SECONDS
  STEWARD_TRIGGER_MAX_RECENT_DELIVERIES
  STEWARD_TRIGGER_MAX_ISSUE_COOLDOWNS
  STEWARD_TRIGGER_MAX_SIGNING_SECRET_BYTES
  STEWARD_TRIGGER_DATABASE_ACQUIRE_TIMEOUT_MILLISECONDS
  STEWARD_WORKLOAD_SOURCE_TOKEN_SECONDS
  STEWARD_WORKLOAD_EXCHANGE_CONNECT_TIMEOUT_MILLISECONDS
  STEWARD_WORKLOAD_EXCHANGE_REQUEST_TIMEOUT_MILLISECONDS
  STEWARD_WORKLOAD_EXCHANGE_MAX_REPLY_BYTES
  STEWARD_TRIGGER_TASK_DEADLINE_MILLISECONDS
  STEWARD_TRIGGER_POLL_INITIAL_MILLISECONDS
  STEWARD_TRIGGER_POLL_MAX_MILLISECONDS
  STEWARD_TRIGGER_API_CONNECT_TIMEOUT_MILLISECONDS
  STEWARD_TRIGGER_API_REQUEST_TIMEOUT_MILLISECONDS
  STEWARD_TRIGGER_MAX_TASK_REPLY_BYTES
)
assert_receiver_numeric_environment() {
  local settings='{}' name value
  for name in "${receiver_numeric_env_names[@]}"; do
    value="$(inline_env_value "${name}")"
    if [[ ! "${value}" =~ ^[0-9]+$ ]]; then
      echo "receiver numeric environment value must contain decimal digits only: ${name}=${value}" >&2
      return 1
    fi
    settings="$(jq -c --arg name "${name}" --arg value "${value}" \
      '. + {($name): $value}' <<< "${settings}")"
  done
  STEWARD_CHART_RUNTIME_SETTINGS_JSON="${settings}" \
    cargo test --quiet --package steward-trigger-receiver \
      --bin steward-trigger-receiver \
      tests::chart_rendered_environment_is_parseable -- --exact
}
assert_receiver_numeric_environment

assert_receiver_startup() {
  local settings='{}' name value status=0 stub_port
  local -a startup_environment=()
  for name in "${receiver_numeric_env_names[@]}"; do
    value="$(inline_env_value "${name}")"
    settings="$(jq -c --arg name "${name}" --arg value "${value}" \
      '. + {($name): $value}' <<< "${settings}")"
  done
  while IFS=$'\t' read -r name value; do
    startup_environment+=("${name}=${value}")
  done < <(jq -r 'to_entries[] | [.key, .value] | @tsv' <<< "${settings}")
  jq '.triggers |= map(.enabled = false)' <<< "${receiver_config}" > "${receiver_startup_config}"

  python3 -c '
import socket
import struct
import time

def receive_exact(connection, length):
    value = b""
    while len(value) < length:
        chunk = connection.recv(length - len(value))
        if not chunk:
            return value
        value += chunk
    return value

with socket.socket() as server:
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    print(server.getsockname()[1], flush=True)
    connection, _ = server.accept()
    with connection:
        startup_length = receive_exact(connection, 4)
        if len(startup_length) == 4:
            receive_exact(connection, struct.unpack("!I", startup_length)[0] - 4)
            connection.sendall(b"R" + struct.pack("!II", 8, 0))
            connection.sendall(b"K" + struct.pack("!III", 12, 1, 1))
            connection.sendall(b"Z" + struct.pack("!I", 5) + b"I")
        time.sleep(10)
' > "${receiver_stub_port}" &
  receiver_stub_pid="$!"
  for _ in {1..50}; do
    [[ -s "${receiver_stub_port}" ]] && break
    sleep 0.1
  done
  if [[ ! -s "${receiver_stub_port}" ]]; then
    echo 'receiver startup database stub did not publish its port' >&2
    return 1
  fi
  stub_port="$(cat "${receiver_stub_port}")"
  cat > "${receiver_stub_kubeconfig}" <<'EOF'
apiVersion: v1
kind: Config
clusters:
  - name: synthetic
    cluster:
      server: https://127.0.0.1:1
      insecure-skip-tls-verify: true
contexts:
  - name: synthetic
    context:
      cluster: synthetic
      user: synthetic
current-context: synthetic
users:
  - name: synthetic
    user:
      token: synthetic-token
EOF

  cargo build --quiet --locked --package steward-trigger-receiver --bin steward-trigger-receiver
  env -i \
    PATH="${PATH}" \
    CARGO_HOME="${CARGO_HOME:-${HOME}/.cargo}" \
    HOME="${HOME}" \
    KUBECONFIG="${receiver_stub_kubeconfig}" \
    "${startup_environment[@]}" \
    STEWARD_TRIGGER_CONFIG_FILE="${receiver_startup_config}" \
    STEWARD_CAPABILITY_CATALOG_FILE="${rendered}" \
    STEWARD_DATABASE_URL="postgresql://steward@127.0.0.1:${stub_port}/steward?sslmode=disable" \
    STEWARD_TRIGGER_NAMESPACE=steward-test \
    STEWARD_TRIGGER_SERVICE_ACCOUNT=steward-trigger-receiver \
    STEWARD_TRIGGER_BIND=127.0.0.1:0 \
    STEWARD_WORKLOAD_SOURCE_AUDIENCE=steward-workload-exchange \
    STEWARD_WORKLOAD_EXCHANGE_ENDPOINT=https://exchange.example.test:1/v1/workload/exchange \
    STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME=exchange.example.test \
    STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE="${rendered}" \
    STEWARD_API_ORIGIN=https://steward.example.test:1 \
    STEWARD_API_SERVER_NAME=steward.example.test \
    STEWARD_API_CA_CERTIFICATE_FILE="${rendered}" \
    "${root}/target/debug/steward-trigger-receiver" \
      > "${receiver_startup_output}" 2>&1 &
  receiver_process_pid="$!"
  local completed=0
  for _ in {1..200}; do
    if ! kill -0 "${receiver_process_pid}" 2>/dev/null; then
      completed=1
      break
    fi
    sleep 0.1
  done
  if [[ "${completed}" == 0 ]]; then
    kill "${receiver_process_pid}" 2>/dev/null || true
    wait "${receiver_process_pid}" 2>/dev/null || true
    receiver_process_pid=""
    echo 'receiver startup exceeded its deadline' >&2
    return 1
  fi
  if wait "${receiver_process_pid}"; then
    status=0
  else
    status="$?"
  fi
  receiver_process_pid=""
  kill "${receiver_stub_pid}" 2>/dev/null || true
  wait "${receiver_stub_pid}" 2>/dev/null || true
  receiver_stub_pid=""
  if [[ "${status}" == 0 ]]; then
    echo 'receiver startup unexpectedly connected to its loopback database stub' >&2
    return 1
  fi
  if grep -Fq 'Could not automatically determine the process-level CryptoProvider' \
    "${receiver_startup_output}"
  then
    echo 'receiver startup did not install the Rustls crypto provider' >&2
    cat "${receiver_startup_output}" >&2
    return 1
  fi
  if grep -Eq "panicked at|thread '[^']+' panicked" "${receiver_startup_output}"; then
    echo 'receiver panicked during startup' >&2
    cat "${receiver_startup_output}" >&2
    return 1
  fi
  grep -Eq 'certificate|PEM' \
    "${receiver_startup_output}" || {
      echo 'receiver startup did not pass the stub database into TLS client construction' >&2
      cat "${receiver_startup_output}" >&2
      return 1
    }
}
assert_receiver_startup

STEWARD_CHART_RECEIVER_CONFIG="${receiver_config}" \
STEWARD_CHART_API_ORIGIN="$(inline_env_value STEWARD_API_ORIGIN)" \
STEWARD_CHART_API_SERVER_NAME="$(inline_env_value STEWARD_API_SERVER_NAME)" \
STEWARD_CHART_EXCHANGE_ENDPOINT="$(inline_env_value STEWARD_WORKLOAD_EXCHANGE_ENDPOINT)" \
STEWARD_CHART_EXCHANGE_SERVER_NAME="$(inline_env_value STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME)" \
  cargo test --quiet --package steward-trigger-receiver \
    client::tests::chart_render_contract_from_environment -- --exact

receiver_ceiling_trigger="$(jq -c '.[0].cooldownSeconds = 31536000' <<< "${receiver_trigger}")"
receiver_ceiling_limits='{"maxConfigBytes":4194304,"maxBodyBytes":1048576,"maxIdentifierBytes":1024,"maxTriggers":4096,"maxIgnoredAccounts":4096,"maxConcurrency":1024,"maxCooldownSeconds":31536000,"maxRecentDeliveries":1000000,"maxIssueCooldowns":1000000,"maxSigningSecretBytes":65536,"databaseAcquireTimeoutMilliseconds":60000,"sourceTokenSeconds":86400,"exchangeConnectTimeoutMilliseconds":60000,"exchangeRequestTimeoutMilliseconds":300000,"exchangeMaxReplyBytes":65536,"taskDeadlineMilliseconds":86400000,"pollInitialMilliseconds":60000,"pollMaxMilliseconds":60000,"apiConnectTimeoutMilliseconds":60000,"apiRequestTimeoutMilliseconds":300000,"maxTaskReplyBytes":1048576}'
receiver_ceiling_inputs=(
  --set-json "triggerReceiver.limits=${receiver_ceiling_limits}"
  --set workspace.maxHistoryDepth=4294967295
  --set-json "triggerReceiver.triggers=${receiver_ceiling_trigger}"
)
helm_template "${receiver_inputs[@]}" "${receiver_ceiling_inputs[@]}" > "${rendered}"
assert_receiver_numeric_environment
grep -qF '\"maxHistoryDepth\":4294967295' "${rendered}" || {
  echo 'workspace history depth ceiling must render as a decimal JSON integer' >&2
  exit 1
}
ceiling_receiver_config="$(
  awk '/^metadata: \{ name: steward-trigger-receiver-config/ { found = 1 } found && /^  config.json: / { sub(/^  config.json: /, ""); print; exit }' "${rendered}" |
    jq -r .
)"
jq -e '.triggers[0].cooldownSeconds == 31536000' <<< "${ceiling_receiver_config}" > /dev/null || {
  echo 'receiver cooldown ceiling must render as a decimal JSON integer' >&2
  exit 1
}

helm_template "${receiver_inputs[@]}" \
  --set-string config.apiserver.jiraBaseUrl= > "${rendered}"
receiver_config_without_site="$(
  awk '/^metadata: \{ name: steward-trigger-receiver-config/ { found = 1 } found && /^  config.json: / { sub(/^  config.json: /, ""); print; exit }' "${rendered}" |
    jq -r .
)"
if jq -e 'has("jiraSiteUrl")' <<< "${receiver_config_without_site}" > /dev/null; then
  echo 'receiver configuration must omit the optional Jira site when it is unset' >&2
  exit 1
fi
STEWARD_CHART_RECEIVER_CONFIG="${receiver_config_without_site}" \
STEWARD_CHART_API_ORIGIN="$(inline_env_value STEWARD_API_ORIGIN)" \
STEWARD_CHART_API_SERVER_NAME="$(inline_env_value STEWARD_API_SERVER_NAME)" \
STEWARD_CHART_EXCHANGE_ENDPOINT="$(inline_env_value STEWARD_WORKLOAD_EXCHANGE_ENDPOINT)" \
STEWARD_CHART_EXCHANGE_SERVER_NAME="$(inline_env_value STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME)" \
  cargo test --quiet --package steward-trigger-receiver \
    client::tests::chart_render_contract_from_environment -- --exact

for missing in \
  triggerReceiver.databaseSecret.name \
  triggerReceiver.workloadExchange.endpoint \
  triggerReceiver.workloadExchange.serverName \
  triggerReceiver.workloadExchange.audience \
  triggerReceiver.taskApi.ca.name \
  triggerReceiver.taskApi.ca.key \
  'triggerReceiver.triggers[0].signingSecret.name' \
  'triggerReceiver.triggers[0].signingSecret.key'
do
  if helm_template "${receiver_inputs[@]}" --set-string "${missing}=" > /dev/null 2>&1; then
    echo "enabled Jira Trigger receiver must reject missing ${missing}" >&2
    exit 1
  fi
done

duplicate_triggers="$(printf '%s' "${receiver_trigger}" | jq -c '. + [(.[] | .cooldownSeconds = 61)]')"
if helm_template "${receiver_inputs[@]}" \
  --set-json "triggerReceiver.triggers=${duplicate_triggers}" > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject duplicate Trigger ids' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set-string triggerReceiver.workloadExchange.serverName=other.example.test > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject a workload exchange server-name mismatch' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.maxIdentifierBytes=20 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must count the generated signing-secret path against the identifier limit' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.maxConcurrency=1025 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject concurrency above the binary ceiling' >&2
  exit 1
fi

for invalid_trigger in \
  'triggerReceiver.triggers[0].envelopeDigest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
  'triggerReceiver.triggers[0].filters.project=proj lower' \
  'triggerReceiver.triggers[0].filters.issueType=not-numeric'
do
  if helm_template "${receiver_inputs[@]}" --set-string "${invalid_trigger}" > /dev/null 2>&1; then
    echo "Jira Trigger receiver must reject invalid ${invalid_trigger%%=*}" >&2
    exit 1
  fi
done

first_config_name="$(awk '/^metadata: \{ name: steward-trigger-receiver-config/ { print $4; exit }' "${rendered}" | tr -d '}')"
helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.triggers[0].cooldownSeconds=61 > "${rendered}"
second_config_name="$(awk '/^metadata: \{ name: steward-trigger-receiver-config/ { print $4; exit }' "${rendered}" | tr -d '}')"
if [[ -z "${first_config_name}" || "${first_config_name}" == "${second_config_name}" ]]; then
  echo 'Jira Trigger receiver config changes must select a new immutable ConfigMap' >&2
  exit 1
fi
helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.triggers[0].enabled=false > "${rendered}"
grep -qF 'optional: true' "${rendered}" || {
  echo 'a disabled Trigger must not require its signing Secret to exist' >&2
  exit 1
}

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.sourceTokenSeconds=599 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject a source token lifetime below 600 seconds' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.maxConcurrency=1 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject a Trigger above its configured concurrency limit' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.apiConnectTimeoutMilliseconds=5001 \
  --set triggerReceiver.limits.apiRequestTimeoutMilliseconds=5000 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject inconsistent Task API time settings' >&2
  exit 1
fi

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
if grep -qF 'STEWARD_OPENSHELL_RUNTIME_CLASS_NAME' "${rendered}"; then
  echo 'default runtime render must not request a RuntimeClass' >&2
  exit 1
fi
if helm_template "${governed_inputs[@]}" > /dev/null 2> "${rendered}"; then
  echo 'governed mode must not use an assumed Mint issuer or SPIFFE trust domain' >&2
  exit 1
fi
grep -qE 'mint\.issuer|spiffeTrustDomain' "${rendered}"
helm_template "${governed_inputs[@]}" \
  --set-string config.mint.issuer=https://mint.example.test \
  --set-string config.mint.spiffeTrustDomain=customer.example.test \
  --set-string config.mint.openshellNamespace=customer-openshell > "${rendered}"
grep -qF 'kind: ClusterSPIFFEID' "${rendered}"
grep -qF 'name: steward-mint' "${rendered}"

echo 'core-only chart dependency boundary passed'
