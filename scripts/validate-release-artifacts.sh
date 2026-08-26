#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rendered="$(mktemp)"
stable_bridge_rendered="$(mktemp)"
stable_bridge_bundle="$(mktemp)"
stable_bridge_configmap="$(mktemp)"
stable_bridge_deployment="$(mktemp)"
stable_bridge_controller_deployment="$(mktemp)"
browser_auth_rendered="$(mktemp)"
browser_auth_deployment="$(mktemp)"
task_identity_rendered="$(mktemp)"
task_identity_deployment="$(mktemp)"
browser_hop1_rendered="$(mktemp)"
browser_hop1_deployment="$(mktemp)"
browser_hop1_loopback_rendered="$(mktemp)"
browser_hop1_loopback_proxy_rendered="$(mktemp)"
web_rendered="$(mktemp)"
web_deployment="$(mktemp)"
mint_synthetic_kubeconfig="$(mktemp)"
mint_startup_output="$(mktemp)"
web_container_id=""

cleanup() {
  status="$?"
  trap - EXIT INT TERM
  set +e
  if [[ -n "${web_container_id}" ]]; then
    docker stop --time 5 "${web_container_id}" >/dev/null 2>&1
  fi
  rm -f "${rendered}" "${stable_bridge_rendered}" "${stable_bridge_bundle}" \
    "${stable_bridge_configmap}" "${stable_bridge_deployment}" \
    "${stable_bridge_controller_deployment}" "${browser_auth_rendered}" \
    "${browser_auth_deployment}" "${task_identity_rendered}" \
    "${task_identity_deployment}" "${browser_hop1_rendered}" \
    "${browser_hop1_deployment}" "${browser_hop1_loopback_rendered}" \
    "${browser_hop1_loopback_proxy_rendered}" \
    "${web_rendered}" "${web_deployment}" \
    "${mint_synthetic_kubeconfig}" "${mint_startup_output}"
  exit "${status}"
}
trap cleanup EXIT INT TERM

digest0="sha256:0000000000000000000000000000000000000000000000000000000000000000"
digest1="sha256:1111111111111111111111111111111111111111111111111111111111111111"
digest2="sha256:2222222222222222222222222222222222222222222222222222222222222222"
digest3="sha256:3333333333333333333333333333333333333333333333333333333333333333"
image_values=(
  --set images.apiserver.tag=validation-apiserver
  --set "images.apiserver.digest=${digest0}"
  --set images.controller.tag=validation-controller
  --set "images.controller.digest=${digest1}"
  --set images.mint.tag=validation-mint
  --set "images.mint.digest=${digest2}"
  --set 'runtimeNamespaces[0]=team-a'
  --set-string config.controller.openshellRuntimeClassName=openshell-runc
)
stable_bridge_values=(
  --set stableBridge.enabled=true
  --set-string stableBridge.image=ghcr.io/example-org/steward-bridge@sha256:3333333333333333333333333333333333333333333333333333333333333333
  --set-string stableBridge.signerIdentity=https://github.com/example-org/steward/.github/workflows/release.yml@refs/tags/v0.1.11
  --set-string stableBridge.sourceRepository=https://github.com/example-org/steward
  --set-string stableBridge.sourceCommit=0123456789abcdef0123456789abcdef01234567
  --set-string stableBridge.service=steward-run
  --set-string stableBridge.mcpGatewayOrigin=https://mcp-gw.example.test
)

printf '%s\n%s' \
  '{"statement":"first provenance record"}' \
  '{"statement":"second provenance record"}' > "${stable_bridge_bundle}"
stable_bridge_bundle_content="$(<"${stable_bridge_bundle}")"
stable_bridge_bundle_hash="$(printf '%s' "${stable_bridge_bundle_content}" | shasum -a 256 | awk '{print $1}')"
stable_bridge_configmap_name="steward-stable-bridge-attestation-${stable_bridge_bundle_hash:0:12}"

helm lint "${root}/charts/steward" "${image_values[@]}"
helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" > "${rendered}"

helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set web.enabled=true \
  --set-string web.host=steward.example.test \
  --set-string web.ingress.className=nginx \
  --set-string web.ingress.tlsSecretName=steward-public-tls \
  --set images.web.tag=validation-web \
  --set "images.web.digest=${digest3}" \
  --set browserAuth.enabled=true \
  --set-string browserAuth.google.clientId=google-client-id \
  --set-string browserAuth.google.origin=https://steward.example.test \
  --set-string browserAuth.google.workspaceDomain=example.test \
  --set-string browserAuth.google.organizationId=org_example \
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
  --set-string browserAuth.google.clientSecret.key=client-secret \
  --set 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24' > "${web_rendered}"

awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: Deployment/ && $0 ~ /name: steward-web/ { print; exit }
' "${web_rendered}" > "${web_deployment}"
for required in \
  'kind: Deployment' \
  'metadata: { name: steward-web }' \
  '      automountServiceAccountToken: false' \
  '      securityContext: { runAsNonRoot: true, runAsUser: 65532, runAsGroup: 65532, seccompProfile: { type: RuntimeDefault } }' \
  '          securityContext: { allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: ["ALL"] } }' \
  '          readinessProbe: { httpGet: { path: /health/ready, port: http }, initialDelaySeconds: 2, periodSeconds: 5 }'
do
  grep -Fxq "${required}" "${web_deployment}"
done
for forbidden in STEWARD_DATABASE_URL STEWARD_JIRA_TOKEN STEWARD_LITELLM_MASTER_KEY STEWARD_MINT_SIGNING_KEY serviceAccountToken:
do
  if grep -Fq "${forbidden}" "${web_deployment}"; then
    echo "web workload must not receive control-plane credentials: ${forbidden}" >&2
    exit 1
  fi
done
for required in \
  'kind: Ingress' \
  'name: steward-api' \
  'name: steward-web' \
  'path: /admin/api' \
  'path: /admin/auth' \
  'path: /admin/connections/github/callback' \
  'path: /app/api' \
  'path: /, pathType: Prefix' \
  'metadata: { name: steward-web-egress }' \
  '  egress: []'
do
  grep -Fq "${required}" "${web_rendered}"
done
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set web.enabled=true >/dev/null 2>&1
then
  echo "enabled web presentation without immutable image and ingress inputs must fail chart validation" >&2
  exit 1
fi

helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  "${stable_bridge_values[@]}" \
  --set-file "stableBridge.attestationBundle=${stable_bridge_bundle}" > "${stable_bridge_rendered}"

helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=true \
  --set-string browserAuth.google.clientId=google-client-id \
  --set-string browserAuth.google.origin=https://steward.example.test \
  --set-string browserAuth.google.workspaceDomain=example.test \
  --set-string browserAuth.google.organizationId=org_example \
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
  --set-string browserAuth.google.clientSecret.key=client-secret \
  --set 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24' > "${browser_auth_rendered}"

awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: Deployment/ && $0 ~ /name: steward-apiserver/ { print; exit }
' "${browser_auth_rendered}" > "${browser_auth_deployment}"
for required in \
  '            - { name: STEWARD_GOOGLE_OIDC_CLIENT_ID, value: "google-client-id" }' \
  '            - { name: STEWARD_BROWSER_ORIGIN, value: "https://steward.example.test" }' \
  '            - { name: STEWARD_GOOGLE_WORKSPACE_DOMAIN, value: "example.test" }' \
  '            - { name: STEWARD_ORGANIZATION_ID, value: "org_example" }' \
  '            - { name: STEWARD_GOOGLE_OIDC_CLIENT_SECRET, valueFrom: { secretKeyRef: { name: steward-google-oidc, key: client-secret } } }'
do
  grep -Fxq "${required}" "${browser_auth_deployment}"
done
grep -Fxq '        - ipBlock: { cidr: 203.0.113.0/24 }' "${browser_auth_rendered}"
if grep -Fq 'STEWARD_GOOGLE_OIDC_' "${rendered}" \
  || grep -Fq 'STEWARD_BROWSER_ORIGIN' "${rendered}" \
  || grep -Fq 'steward-google-oidc' "${rendered}" \
  || grep -Fq 'STEWARD_BROWSER_HOP1_' "${rendered}" \
  || grep -Fq 'STEWARD_IDENTITY_BROWSER_HOP1_' "${rendered}" \
  || grep -Fq 'STEWARD_MCP_GW_ORIGIN' "${rendered}" \
  || grep -Fq 'STEWARD_IDENTITY_TASK_' "${rendered}"
then
  echo "disabled browser features must render no browser auth or HOP-1 wiring" >&2
  exit 1
fi

helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set taskIdentity.enabled=true \
  --set-string taskIdentity.issuer=https://identity.example.test \
  --set-string taskIdentity.audience=steward-task-api \
  --set-string taskIdentity.publicJwksConfigMap.name=identity-task-jwks \
  --set-string taskIdentity.publicJwksConfigMap.key=jwks.json > "${task_identity_rendered}"
awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: Deployment/ && $0 ~ /name: steward-apiserver/ { print; exit }
' "${task_identity_rendered}" > "${task_identity_deployment}"
for required in \
  '            - { name: STEWARD_IDENTITY_TASK_ISSUER, value: "https://identity.example.test" }' \
  '            - { name: STEWARD_IDENTITY_TASK_AUDIENCE, value: "steward-task-api" }' \
  '            - { name: STEWARD_IDENTITY_TASK_JWKS_FILE, value: /run/identity-task/jwks.json }' \
  '            - { name: identity-task, mountPath: /run/identity-task, readOnly: true }' \
  '        - name: identity-task' \
  '            name: identity-task-jwks' \
  '            items: [{ key: jwks.json, path: jwks.json }]'
do
  grep -Fxq "${required}" "${task_identity_deployment}"
done
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set taskIdentity.enabled=true >/dev/null 2>&1
then
  echo "enabled Identity task authentication without every immutable input must fail chart validation" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set taskIdentity.enabled=false \
  --set-string taskIdentity.issuer=https://identity.example.test >/dev/null 2>&1
then
  echo "disabled Identity task authentication with stale configuration must fail chart validation" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=true >/dev/null 2>&1
then
  echo "enabled browser authentication without every required input must fail chart validation" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=true \
  --set-string browserAuth.google.clientId=google-client-id \
  --set-string browserAuth.google.origin=https://steward.example.test \
  --set-string browserAuth.google.workspaceDomain=example.test \
  --set-string browserAuth.google.organizationId=org_example \
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
  --set-string browserAuth.google.clientSecret.key=client-secret >/dev/null 2>&1
then
  echo "browser authentication with default-deny policy must require explicit Google OIDC egress CIDRs" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=false \
  --set-string browserAuth.google.clientId=stale-client >/dev/null 2>&1
then
  echo "disabled browser authentication with stale configuration must fail chart validation" >&2
  exit 1
fi
for invalid_browser_origin in \
  'http://steward.example.test' \
  'https://user@steward.example.test' \
  'https://steward.example.test:70000' \
  'https://steward example.test'
do
  if helm template steward "${root}/charts/steward" \
    --namespace steward \
    --include-crds \
    "${image_values[@]}" \
    --set browserAuth.enabled=true \
    --set-string browserAuth.google.clientId=google-client-id \
    --set-string "browserAuth.google.origin=${invalid_browser_origin}" \
    --set-string browserAuth.google.workspaceDomain=example.test \
    --set-string browserAuth.google.organizationId=org_example \
    --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
    --set-string browserAuth.google.clientSecret.key=client-secret \
    --set 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24' >/dev/null 2>&1
  then
    echo "browser authentication origin must be HTTPS without userinfo, whitespace, or invalid ports: ${invalid_browser_origin}" >&2
    exit 1
  fi
done

helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=true \
  --set-string browserAuth.google.clientId=google-client-id \
  --set-string browserAuth.google.origin=https://steward.example.test \
  --set-string browserAuth.google.workspaceDomain=example.test \
  --set-string browserAuth.google.organizationId=org_example \
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
  --set-string browserAuth.google.clientSecret.key=client-secret \
  --set 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24' \
  --set browserHop1.enabled=true \
  --set-string browserHop1.mcpGatewayOrigin=https://mcp-gw.example.test:8080 \
  --set-string browserHop1.identityEndpoint=https://identity.example.test:8443/v1/browser-hop1/exchange \
  --set-string browserHop1.issuer=https://steward.example.test \
  --set-string browserHop1.assertionAudience=identity-browser-hop1 \
  --set-string browserHop1.keyId=steward-browser-hop1-current \
  --set-string browserHop1.signingKeySecret.name=steward-browser-hop1 \
  --set-string browserHop1.signingKeySecret.key=signing-key.der \
  --set-string browserHop1.publicJwksConfigMap.name=steward-browser-hop1-jwks \
  --set-string browserHop1.publicJwksConfigMap.key=jwks.json \
  --set-string browserHop1.serviceAccountTokenAudience=identity-browser-hop1 > "${browser_hop1_rendered}"

awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: Deployment/ && $0 ~ /name: steward-apiserver/ { print; exit }
' "${browser_hop1_rendered}" > "${browser_hop1_deployment}"
for required in \
  '            - { name: STEWARD_MCP_GW_ORIGIN, value: "https://mcp-gw.example.test:8080" }' \
  '            - { name: STEWARD_IDENTITY_BROWSER_HOP1_ENDPOINT, value: "https://identity.example.test:8443/v1/browser-hop1/exchange" }' \
  '            - { name: STEWARD_IDENTITY_BROWSER_HOP1_CA_CERTIFICATE_FILE, value: /run/browser-hop1/identity-ca.crt }' \
  '            - { name: STEWARD_BROWSER_HOP1_ISSUER, value: "https://steward.example.test" }' \
  '            - { name: STEWARD_BROWSER_HOP1_ASSERTION_AUDIENCE, value: "identity-browser-hop1" }' \
  '            - { name: STEWARD_BROWSER_HOP1_KEY_ID, value: "steward-browser-hop1-current" }' \
  '            - { name: STEWARD_BROWSER_HOP1_SIGNING_KEY_FILE, value: /run/browser-hop1/signing-key.der }' \
  '            - { name: STEWARD_BROWSER_HOP1_JWKS_FILE, value: /run/browser-hop1/jwks.json }' \
  '            - { name: STEWARD_BROWSER_HOP1_SERVICE_ACCOUNT_TOKEN_FILE, value: /var/run/secrets/steward/browser-hop1/source-token }' \
  '            - { name: browser-hop1, mountPath: /run/browser-hop1, readOnly: true }' \
  '            - { name: browser-hop1-source-credential, mountPath: /var/run/secrets/steward/browser-hop1, readOnly: true }' \
  '                  audience: "identity-browser-hop1"' \
  '                  expirationSeconds: 600' \
  '                  path: source-token' \
  '                  name: steward-browser-hop1' \
  '                  name: steward-browser-hop1-jwks' \
  '                  name: steward-workload-exchange-ca'
do
  grep -Fxq "${required}" "${browser_hop1_deployment}"
done
grep -Fxq '    - to: [{ namespaceSelector: { matchLabels: { kubernetes.io/metadata.name: mcp-gw } } }]' "${browser_hop1_rendered}"
grep -Fxq '    - to: [{ namespaceSelector: { matchLabels: { kubernetes.io/metadata.name: identity-exchange } } }]' "${browser_hop1_rendered}"
helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=true \
  --set-string browserAuth.google.clientId=google-client-id \
  --set-string browserAuth.google.origin=https://steward.example.test \
  --set-string browserAuth.google.workspaceDomain=example.test \
  --set-string browserAuth.google.organizationId=org_example \
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
  --set-string browserAuth.google.clientSecret.key=client-secret \
  --set 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24' \
  --set browserHop1.enabled=true \
  --set-string browserHop1.mcpGatewayOrigin=http://127.0.0.1:18080 \
  --set-string browserHop1.identityEndpoint=https://identity.example.test:8443/v1/browser-hop1/exchange \
  --set-string browserHop1.issuer=https://steward.example.test \
  --set-string browserHop1.assertionAudience=identity-browser-hop1 \
  --set-string browserHop1.keyId=steward-browser-hop1-current \
  --set-string browserHop1.signingKeySecret.name=steward-browser-hop1 \
  --set-string browserHop1.signingKeySecret.key=signing-key.der \
  --set-string browserHop1.publicJwksConfigMap.name=steward-browser-hop1-jwks \
  --set-string browserHop1.publicJwksConfigMap.key=jwks.json \
  --set-string browserHop1.serviceAccountTokenAudience=identity-browser-hop1 > "${browser_hop1_loopback_rendered}"
grep -Fxq '            - { name: STEWARD_MCP_GW_ORIGIN, value: "http://127.0.0.1:18080" }' "${browser_hop1_loopback_rendered}"
helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=true \
  --set-string browserAuth.google.clientId=google-client-id \
  --set-string browserAuth.google.origin=https://steward.example.test \
  --set-string browserAuth.google.workspaceDomain=example.test \
  --set-string browserAuth.google.organizationId=org_example \
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
  --set-string browserAuth.google.clientSecret.key=client-secret \
  --set 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24' \
  --set browserHop1.enabled=true \
  --set-string browserHop1.mcpGatewayOrigin=http://127.0.0.1:18080 \
  --set-string browserHop1.identityEndpoint=https://identity.example.test:8443/v1/browser-hop1/exchange \
  --set-string browserHop1.issuer=https://steward.example.test \
  --set-string browserHop1.assertionAudience=identity-browser-hop1 \
  --set-string browserHop1.keyId=steward-browser-hop1-current \
  --set-string browserHop1.signingKeySecret.name=steward-browser-hop1 \
  --set-string browserHop1.signingKeySecret.key=signing-key.der \
  --set-string browserHop1.publicJwksConfigMap.name=steward-browser-hop1-jwks \
  --set-string browserHop1.publicJwksConfigMap.key=jwks.json \
  --set-string browserHop1.serviceAccountTokenAudience=identity-browser-hop1 \
  --set browserHop1.loopbackProxy.enabled=true \
  --set-string browserHop1.loopbackProxy.image=example.test/local-socat@sha256:4444444444444444444444444444444444444444444444444444444444444444 \
  --set-string browserHop1.loopbackProxy.targetHost=mcp-gw-github-wrapper.mcp-gw.svc.cluster.local \
  --set browserHop1.loopbackProxy.targetPort=8080 > "${browser_hop1_loopback_proxy_rendered}"
awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: Deployment/ && $0 ~ /name: steward-apiserver/ { print; exit }
' "${browser_hop1_loopback_proxy_rendered}" > "${browser_hop1_deployment}"
for required in \
  '        - name: mcp-loopback' \
  '        image: example.test/local-socat@sha256:4444444444444444444444444444444444444444444444444444444444444444' \
  '        - TCP-LISTEN:18080,bind=127.0.0.1,reuseaddr,fork' \
  '        - TCP:mcp-gw-github-wrapper.mcp-gw.svc.cluster.local:8080'
do
  grep -Fxq "${required}" "${browser_hop1_deployment}"
done
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=true \
  --set-string browserAuth.google.clientId=google-client-id \
  --set-string browserAuth.google.origin=https://steward.example.test \
  --set-string browserAuth.google.workspaceDomain=example.test \
  --set-string browserAuth.google.organizationId=org_example \
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
  --set-string browserAuth.google.clientSecret.key=client-secret \
  --set 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24' \
  --set browserHop1.enabled=true \
  --set-string browserHop1.mcpGatewayOrigin=https://mcp-gw.example.test:8080 \
  --set-string browserHop1.identityEndpoint=https://identity.example.test:8443/v1/browser-hop1/exchange \
  --set-string browserHop1.issuer=https://steward.example.test \
  --set-string browserHop1.assertionAudience=identity-browser-hop1 \
  --set-string browserHop1.keyId=steward-browser-hop1-current \
  --set-string browserHop1.signingKeySecret.name=steward-browser-hop1 \
  --set-string browserHop1.signingKeySecret.key=signing-key.der \
  --set-string browserHop1.publicJwksConfigMap.name=steward-browser-hop1-jwks \
  --set-string browserHop1.publicJwksConfigMap.key=jwks.json \
  --set-string browserHop1.serviceAccountTokenAudience=identity-browser-hop1 \
  --set browserHop1.loopbackProxy.enabled=true \
  --set-string browserHop1.loopbackProxy.image=example.test/local-socat@sha256:4444444444444444444444444444444444444444444444444444444444444444 \
  --set-string browserHop1.loopbackProxy.targetHost=mcp-gw-github-wrapper.mcp-gw.svc.cluster.local \
  --set browserHop1.loopbackProxy.targetPort=8080 >/dev/null 2>&1
then
  echo "browser HOP-1 loopback proxy must require its exact localhost origin" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserAuth.enabled=true \
  --set-string browserAuth.google.clientId=google-client-id \
  --set-string browserAuth.google.origin=https://steward.example.test \
  --set-string browserAuth.google.workspaceDomain=example.test \
  --set-string browserAuth.google.organizationId=org_example \
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc \
  --set-string browserAuth.google.clientSecret.key=client-secret \
  --set 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24' \
  --set browserHop1.enabled=true \
  --set-string browserHop1.mcpGatewayOrigin=http://mcp-gw.example.test:8080 \
  --set-string browserHop1.identityEndpoint=https://identity.example.test:8443/v1/browser-hop1/exchange \
  --set-string browserHop1.issuer=https://steward.example.test \
  --set-string browserHop1.assertionAudience=identity-browser-hop1 \
  --set-string browserHop1.keyId=steward-browser-hop1-current \
  --set-string browserHop1.signingKeySecret.name=steward-browser-hop1 \
  --set-string browserHop1.signingKeySecret.key=signing-key.der \
  --set-string browserHop1.publicJwksConfigMap.name=steward-browser-hop1-jwks \
  --set-string browserHop1.publicJwksConfigMap.key=jwks.json \
  --set-string browserHop1.serviceAccountTokenAudience=identity-browser-hop1 >/dev/null 2>&1
then
  echo "browser HOP-1 must reject non-loopback HTTP MCP-GW origins" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set browserHop1.enabled=true >/dev/null 2>&1
then
  echo "browser HOP-1 must reject partial or browser-auth-disabled configuration" >&2
  exit 1
fi

awk -v name="${stable_bridge_configmap_name}" '
  BEGIN { RS = "---\\n" }
  $0 ~ ("kind: ConfigMap\\nmetadata: \\{ name: " name " \\}") { print; exit }
' "${stable_bridge_rendered}" > "${stable_bridge_configmap}"
grep -Fxq 'kind: ConfigMap' "${stable_bridge_configmap}"
grep -Fxq "metadata: { name: ${stable_bridge_configmap_name} }" "${stable_bridge_configmap}"
grep -Fxq 'immutable: true' "${stable_bridge_configmap}"
rendered_stable_bridge_bundle="$(awk '
  $0 == "  bundle.jsonl: |-" { capture = 1; next }
  capture && /^    / { line = $0; sub(/^    /, "", line); print line; next }
  capture { exit }
' "${stable_bridge_configmap}")"
if [[ "${rendered_stable_bridge_bundle}" != "${stable_bridge_bundle_content}" ]]; then
  echo "stable bridge attestation ConfigMap bytes must exactly match the configured bundle" >&2
  exit 1
fi

awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: Deployment/ && $0 ~ /name: steward-apiserver/ { print; exit }
' "${stable_bridge_rendered}" > "${stable_bridge_deployment}"
grep -Fxq "            - { name: STEWARD_STABLE_BRIDGE_IMAGE, value: \"ghcr.io/example-org/steward-bridge@sha256:3333333333333333333333333333333333333333333333333333333333333333\" }" "${stable_bridge_deployment}"
grep -Fxq '            - { name: STEWARD_STABLE_BRIDGE_SIGNER_IDENTITY, value: "https://github.com/example-org/steward/.github/workflows/release.yml@refs/tags/v0.1.11" }' "${stable_bridge_deployment}"
grep -Fxq '            - { name: STEWARD_STABLE_BRIDGE_SOURCE_REPOSITORY, value: "https://github.com/example-org/steward" }' "${stable_bridge_deployment}"
grep -Fxq '            - { name: STEWARD_STABLE_BRIDGE_SOURCE_COMMIT, value: "0123456789abcdef0123456789abcdef01234567" }' "${stable_bridge_deployment}"
grep -Fxq '            - { name: STEWARD_STABLE_BRIDGE_ATTESTATION_BUNDLE_FILE, value: /run/stable-bridge-attestation/bundle.jsonl }' "${stable_bridge_deployment}"
grep -Fxq '            - { name: STEWARD_STABLE_BRIDGE_SERVICE, value: "steward-run" }' "${stable_bridge_deployment}"
grep -Fxq '            - { name: stable-bridge-attestation, mountPath: /run/stable-bridge-attestation, readOnly: true }' "${stable_bridge_deployment}"
grep -Fxq '        - name: stable-bridge-attestation' "${stable_bridge_deployment}"
grep -Fxq "            name: ${stable_bridge_configmap_name}" "${stable_bridge_deployment}"
grep -Fxq '            items: [{ key: bundle.jsonl, path: bundle.jsonl }]' "${stable_bridge_deployment}"

awk '
  BEGIN { RS = "---\\n" }
  $0 ~ /kind: Deployment/ && $0 ~ /name: steward-controller/ { print; exit }
' "${stable_bridge_rendered}" > "${stable_bridge_controller_deployment}"
grep -Fxq 'kind: Deployment' "${stable_bridge_controller_deployment}"
for required in \
  '            - { name: STEWARD_STABLE_BRIDGE_IMAGE, value: "ghcr.io/example-org/steward-bridge@sha256:3333333333333333333333333333333333333333333333333333333333333333" }' \
  '            - { name: STEWARD_STABLE_BRIDGE_SIGNER_IDENTITY, value: "https://github.com/example-org/steward/.github/workflows/release.yml@refs/tags/v0.1.11" }' \
  '            - { name: STEWARD_STABLE_BRIDGE_SOURCE_REPOSITORY, value: "https://github.com/example-org/steward" }' \
  '            - { name: STEWARD_STABLE_BRIDGE_SOURCE_COMMIT, value: "0123456789abcdef0123456789abcdef01234567" }' \
  '            - { name: STEWARD_STABLE_BRIDGE_ATTESTATION_BUNDLE_FILE, value: /run/stable-bridge-attestation/bundle.jsonl }' \
  '            - { name: STEWARD_STABLE_BRIDGE_MCP_GW_ORIGIN, value: "https://mcp-gw.example.test" }' \
  '            - { name: stable-bridge-attestation, mountPath: /run/stable-bridge-attestation, readOnly: true }' \
  '        - name: stable-bridge-attestation' \
  "            name: ${stable_bridge_configmap_name}" \
  '            items: [{ key: bundle.jsonl, path: bundle.jsonl }]'
do
  grep -Fxq "${required}" "${stable_bridge_controller_deployment}"
done

if grep -Fq 'steward-stable-bridge-attestation-' "${rendered}" \
  || grep -Fq 'STEWARD_STABLE_BRIDGE_' "${rendered}"
then
  echo "a disabled stable bridge must render no bridge ConfigMap or workload wiring" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  --set stableBridge.enabled=true >/dev/null 2>&1
then
  echo "an enabled stable bridge without immutable provenance inputs must fail chart validation" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  "${stable_bridge_values[@]}" \
  --set-string stableBridge.image=ghcr.io/example-org/steward-bridge:mutable \
  --set-file "stableBridge.attestationBundle=${stable_bridge_bundle}" >/dev/null 2>&1
then
  echo "a mutable stable bridge image must fail chart validation" >&2
  exit 1
fi
if helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" \
  "${stable_bridge_values[@]}" \
  --set-string stableBridge.mcpGatewayOrigin= \
  --set-file "stableBridge.attestationBundle=${stable_bridge_bundle}" >/dev/null 2>&1
then
  echo "a stable bridge image without its controller-owned MCP-GW origin must fail chart validation" >&2
  exit 1
fi
for invalid_bridge_origin in \
  'https://bridge-user@mcp-gw.example.test' \
  'https://mcp-gw.example.test:70000' \
  'https://mcp gw.example.test'
do
  if helm template steward "${root}/charts/steward" \
    --namespace steward \
    --include-crds \
    "${image_values[@]}" \
    "${stable_bridge_values[@]}" \
    --set-string "stableBridge.mcpGatewayOrigin=${invalid_bridge_origin}" \
    --set-file "stableBridge.attestationBundle=${stable_bridge_bundle}" >/dev/null 2>&1
  then
    echo "a stable bridge origin must reject userinfo, invalid ports, and whitespace: ${invalid_bridge_origin}" >&2
    exit 1
  fi
done
if helm lint "${root}/charts/steward" "${image_values[@]}" \
  --set stableBridge.enabled=true >/dev/null 2>&1
then
  echo "an enabled stable bridge without immutable provenance inputs must fail chart validation" >&2
  exit 1
fi

test "$(grep -c '^kind: Deployment$' "${rendered}")" -eq 3
test "$(grep -c '^kind: ServiceAccount$' "${rendered}")" -eq 3
test "$(grep -c '^kind: NetworkPolicy$' "${rendered}")" -eq 8
test "$(grep -c '^kind: Role$' "${rendered}")" -eq 2
test "$(grep -c '^kind: RoleBinding$' "${rendered}")" -eq 2
test "$(grep -c '^kind: ClusterSPIFFEID$' "${rendered}")" -eq 1
test "$(grep -c '^  namespace: team-a$' "${rendered}")" -eq 4
grep -q '^kind: CustomResourceDefinition$' "${rendered}"
grep -q 'failurePolicy: Fail' "${rendered}"
grep -q 'driver: csi.spiffe.io' "${rendered}"
grep -q 'name: STEWARD_OPENSHELL_SERVER_NAME' "${rendered}"
grep -q 'name: STEWARD_OPENSHELL_RUNTIME_CLASS_NAME' "${rendered}"
grep -Eq 'value: "?openshell-runc"?' "${rendered}"
grep -q 'name: STEWARD_OPENSHELL_CA_CERTIFICATE_FILE' "${rendered}"
grep -q 'name: STEWARD_OPENSHELL_CLIENT_CERTIFICATE_FILE' "${rendered}"
grep -q 'name: STEWARD_OPENSHELL_CLIENT_PRIVATE_KEY_FILE' "${rendered}"
grep -q 'name: STEWARD_WORKLOAD_EXCHANGE_ENDPOINT' "${rendered}"
grep -q 'name: STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME' "${rendered}"
grep -q 'name: STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE' "${rendered}"
grep -q 'name: STEWARD_WORKLOAD_SOURCE_CREDENTIAL_FILE' "${rendered}"
test "$(grep -c 'name: STEWARD_KUBERNETES_TOKEN_REVIEW_AUDIENCE' "${rendered}")" -eq 1
grep -q 'value: "https://kubernetes.default.svc"' "${rendered}"
if grep -Eq 'name: STEWARD_(TASK_)?TOKEN_AUDIENCE' "${rendered}"; then
  echo "ambiguous legacy TokenReview audience variables must not be rendered" >&2
  exit 1
fi
if helm lint "${root}/charts/steward" "${image_values[@]}" \
  --set-string config.apiserver.kubernetesTokenReviewAudience= >/dev/null 2>&1
then
  echo "an empty Kubernetes TokenReview audience must fail chart validation" >&2
  exit 1
fi
legacy_runtime_values=("${image_values[@]:0:${#image_values[@]}-2}")
helm lint "${root}/charts/steward" "${legacy_runtime_values[@]}" \
  --set-string config.controller.openshellRuntimeClassName=kata-qemu >/dev/null
if helm lint "${root}/charts/steward" "${legacy_runtime_values[@]}" \
  --set-string config.controller.openshellRuntimeClassName=invalid/runtime >/dev/null 2>&1
then
  echo "an invalid Kubernetes RuntimeClass name must fail chart validation" >&2
  exit 1
fi
test "$(grep -c 'secretName: steward-openshell-client' "${rendered}")" -eq 1
grep -q 'serviceAccountToken:' "${rendered}"
grep -q 'audience: apelogic-workload-exchange' "${rendered}"
grep -q 'expirationSeconds: 600' "${rendered}"
grep -q 'mountPath: /var/run/secrets/steward/workload' "${rendered}"
grep -q 'mountPath: /run/workload-exchange' "${rendered}"
if grep -q 'name: STEWARD_OPENSHELL_BEARER_TOKEN_FILE' "${rendered}"; then
  echo "the raw workload credential must not be sent directly to OpenShell" >&2
  exit 1
fi
if grep -q 'key: source-token, path: source-token' "${rendered}"; then
  echo "workload source credentials must not be sourced from a Secret" >&2
  exit 1
fi
if awk '
  /^---$/ { cluster_role = 0 }
  /^kind: ClusterRole$/ { cluster_role = 1 }
  cluster_role && /resources: \["secrets"\]/ { found = 1 }
  END { exit found ? 0 : 1 }
' "${rendered}"
then
  echo "globally bound ClusterRoles must not grant Secret access" >&2
  exit 1
fi
if grep -q '^  namespace: team-b$' "${rendered}"; then
  echo "Secret RBAC escaped the authorized runtime namespace" >&2
  exit 1
fi
if grep -q '^kind: Ingress$' "${rendered}"; then
  echo "Steward must not expose a public Ingress" >&2
  exit 1
fi

if [[ "${1:-}" == "--build-images" ]]; then
  for component in apiserver controller mint; do
    docker build \
      --file "${root}/build/package.Dockerfile" \
      --build-arg "BINARY=steward-${component}-bin" \
      --tag "steward-${component}:release-validation" \
      "${root}"
  done
  docker build \
    --file "${root}/build/connections-bridge.Dockerfile" \
    --tag "steward-bridge:release-validation" \
    "${root}"
  docker build \
    --file "${root}/build/web.Dockerfile" \
    --tag "steward-web:release-validation" \
    "${root}"
  web_container_id="$(
    docker run --rm --detach \
      --read-only \
      --tmpfs /tmp:rw,noexec,nosuid,size=16m \
      --publish 127.0.0.1::3000 \
      --label "steward.test/run-id=release-web-$$" \
      steward-web:release-validation
  )"
  web_runtime="$(docker inspect --format '{{.Config.User}} {{.HostConfig.ReadonlyRootfs}}' "${web_container_id}")"
  if [[ "${web_runtime}" != "65532:65532 true" ]]; then
    echo "web release image must run as 65532:65532 on a read-only root filesystem" >&2
    exit 1
  fi
  web_address="$(docker port "${web_container_id}" 3000/tcp | head -n 1)"
  web_port="${web_address##*:}"
  if [[ ! "${web_port}" =~ ^[0-9]+$ ]]; then
    echo "web release image did not publish a loopback readiness port" >&2
    exit 1
  fi
  if ! web_status="$(
    curl --fail --silent --show-error \
      --retry 20 \
      --retry-all-errors \
      --retry-connrefused \
      --retry-delay 1 \
      --output /dev/null \
      --write-out '%{http_code}' \
      "http://127.0.0.1:${web_port}/health/ready"
  )"
  then
    echo "web release image did not become ready" >&2
    exit 1
  fi
  if [[ "${web_status}" != "204" ]]; then
    echo "web release image did not become ready: expected 204, received ${web_status}" >&2
    exit 1
  fi
  docker stop --time 5 "${web_container_id}" >/dev/null
  web_container_id=""
  docker run --rm --entrypoint /bin/sh steward-bridge:release-validation -ceu '
    command -v tar >/dev/null
    command -v ip >/dev/null
    command -v mkdir >/dev/null
    command -v rm >/dev/null
    test -w /sandbox
    test "$(id -u)" = "65532"
    : >/sandbox/release-validation
    rm /sandbox/release-validation
  '
  cat > "${mint_synthetic_kubeconfig}" <<'EOF'
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
  chmod 0644 "${mint_synthetic_kubeconfig}"
  if docker run --rm --network none \
    --env KUBECONFIG=/run/steward-release/kubeconfig \
    --volume "${mint_synthetic_kubeconfig}:/run/steward-release/kubeconfig:ro" \
    steward-mint:release-validation > "${mint_startup_output}" 2>&1
  then
    echo "the mint release-image smoke expects the synthetic Kubernetes endpoint to be unavailable" >&2
    exit 1
  fi
  if grep -Fq 'Could not automatically determine the process-level CryptoProvider' "${mint_startup_output}"; then
    echo "the combined release mint image must select the Rustls crypto provider before Kubernetes startup" >&2
    cat "${mint_startup_output}" >&2
    exit 1
  fi
  grep -Fq 'OpenShell identity discovery failed' "${mint_startup_output}"
elif [[ -n "${1:-}" ]]; then
  echo "usage: $0 [--build-images]" >&2
  exit 2
fi

"${root}/scripts/test-promote-ecr-artifact.sh"
"${root}/scripts/test-resolve-ecr-platform-digest.sh"
