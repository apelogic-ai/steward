#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rendered="$(mktemp)"
trap 'rm -f "${rendered}"' EXIT INT TERM

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

helm lint "${root}/charts/steward" "${image_values[@]}"
helm template steward "${root}/charts/steward" \
  --namespace steward \
  --include-crds \
  "${image_values[@]}" > "${rendered}"

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
elif [[ -n "${1:-}" ]]; then
  echo "usage: $0 [--build-images]" >&2
  exit 2
fi

"${root}/scripts/test-promote-ecr-artifact.sh"
"${root}/scripts/test-resolve-ecr-platform-digest.sh"
