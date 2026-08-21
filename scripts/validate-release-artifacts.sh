#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rendered="$(mktemp)"
stable_bridge_rendered="$(mktemp)"
stable_bridge_bundle="$(mktemp)"
stable_bridge_configmap="$(mktemp)"
stable_bridge_deployment="$(mktemp)"
stable_bridge_controller_deployment="$(mktemp)"
mint_synthetic_kubeconfig="$(mktemp)"
mint_startup_output="$(mktemp)"
trap 'rm -f "${rendered}" "${stable_bridge_rendered}" "${stable_bridge_bundle}" "${stable_bridge_configmap}" "${stable_bridge_deployment}" "${stable_bridge_controller_deployment}" "${mint_synthetic_kubeconfig}" "${mint_startup_output}"' EXIT INT TERM

digest0="sha256:0000000000000000000000000000000000000000000000000000000000000000"
digest1="sha256:1111111111111111111111111111111111111111111111111111111111111111"
digest2="sha256:2222222222222222222222222222222222222222222222222222222222222222"
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
  "${stable_bridge_values[@]}" \
  --set-file "stableBridge.attestationBundle=${stable_bridge_bundle}" > "${stable_bridge_rendered}"

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
  docker run --rm --entrypoint /bin/sh steward-bridge:release-validation -ceu '
    command -v cp >/dev/null
    command -v tar >/dev/null
    command -v ip >/dev/null
    command -v find >/dev/null
    command -v mktemp >/dev/null
    command -v mkdir >/dev/null
    command -v rm >/dev/null
    command -v touch >/dev/null
    test -w /sandbox
    test "$(id -u)" = "65532"
    test "$(id -g)" = "65532"
    : >/sandbox/release-validation
    rm /sandbox/release-validation
    workspace_init="$(mktemp -d /sandbox/workspace-init.XXXXXX)"
    touch "${workspace_init}/source"
    printf "%s\n" "workspace-init-copy" > "${workspace_init}/source"
    cp "${workspace_init}/source" "${workspace_init}/copied"
    test -f "${workspace_init}/copied"
    test -s "${workspace_init}/copied"
    /bin/busybox cmp "${workspace_init}/source" "${workspace_init}/copied"
    test "$(find "${workspace_init}" -type f -name source)" = "${workspace_init}/source"
    tar -cf "${workspace_init}/source.tar" -C "${workspace_init}" source
    test -f "${workspace_init}/source.tar"
    rm -rf "${workspace_init}"
    test ! -e "${workspace_init}"
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
