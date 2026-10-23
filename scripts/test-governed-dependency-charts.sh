#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'find "${fixture}" -depth -delete 2>/dev/null || true' EXIT INT TERM

charts="spire agent-sandbox openshell litellm arc"
for chart in ${charts}; do
  for file in Chart.yaml README.md values.schema.json values.yaml; do
    if [[ ! -f "${root}/charts/${chart}/${file}" ]]; then
      echo "missing governed dependency chart file: charts/${chart}/${file}" >&2
      exit 1
    fi
  done
done

bash "${root}/scripts/prepare-governed-dependency-charts.sh" "${fixture}/charts"
mkdir -p "${fixture}/packages"

rendered_images="${fixture}/rendered-images.txt"
: >"${rendered_images}"
for chart in ${charts}; do
  prepared="${fixture}/charts/${chart}"
  rendered="${fixture}/${chart}.yaml"
  helm lint "${prepared}"
  helm template "test-${chart}" "${prepared}" \
    --namespace steward-test \
    --skip-tests >"${rendered}"
  case "${chart}" in
    spire)
      (cd "${fixture}" && helm package charts/spire --destination packages >/dev/null)
      ;;
    agent-sandbox)
      (cd "${fixture}" && helm package charts/agent-sandbox --destination packages >/dev/null)
      ;;
    openshell)
      (cd "${fixture}" && helm package charts/openshell --destination packages >/dev/null)
      ;;
    litellm)
      (cd "${fixture}" && helm package charts/litellm --destination packages >/dev/null)
      ;;
    arc)
      (cd "${fixture}" && helm package charts/arc --destination packages >/dev/null)
      ;;
  esac

  chart_version="$(sed -nE 's/^version:[[:space:]]*([^[:space:]]+).*$/\1/p' "${prepared}/Chart.yaml")"
  package="${fixture}/packages/${chart}-${chart_version}.tgz"
  for packaged_file in LICENSE THIRD_PARTY_NOTICES.md; do
    if ! tar -tzf "${package}" | grep -Fxq "${chart}/${packaged_file}"; then
      echo "${chart} package omitted ${packaged_file}" >&2
      exit 1
    fi
  done

  if grep -Eq '^kind:[[:space:]]+Namespace$' "${rendered}"; then
    echo "${chart} rendered a Namespace" >&2
    exit 1
  fi
  if grep -Eq '^kind:[[:space:]]+Secret$' "${rendered}"; then
    echo "${chart} rendered inline Secret material" >&2
    exit 1
  fi

  awk '$1 == "image:" {gsub(/^"|"$/, "", $2); print $2}' "${rendered}" \
    >>"${rendered_images}"
done

sort -u "${rendered_images}" -o "${rendered_images}"
while IFS= read -r image; do
  [[ "${image}" =~ @sha256:[0-9a-f]{64}$ ]] || {
    echo "rendered image is not immutable: ${image}" >&2
    exit 1
  }
  grep -Fxq "${image}" "${root}/charts/governed-images.txt" || {
    echo "rendered image is absent from charts/governed-images.txt: ${image}" >&2
    exit 1
  }
done <"${rendered_images}"

# The four upstream-backed wrappers may own integration resources and
# validation only. They must not copy or override upstream workloads.
for chart in spire openshell litellm arc; do
  if find "${root}/charts/${chart}/templates" -type f \
    ! -name '_helpers.tpl' \
    ! -name '_validate.tpl' \
    ! -name '*route.yaml' \
    ! -name 'NOTES.txt' -print -quit 2>/dev/null | grep -q .; then
    echo "${chart} contains a wrapper-owned upstream workload template" >&2
    exit 1
  fi
done

# LiteLLM must use the one reviewed image coordinate already exercised by
# conformance. This assertion makes the repeated chart value fail on drift.
litellm_pin="$(
  sed -nE 's/^LATEST_IMAGE="([^"]+)"$/\1/p' \
    "${root}/scripts/g4-upstream-conformance"
)"
grep -Fq "${litellm_pin##*@}" "${root}/charts/litellm/values.yaml"
grep -Fq 'appVersion: 1.106.2' "${root}/charts/litellm/Chart.yaml"

# The OpenShell application version is already a released install contract.
openshell_version="$(
  sed -nE 's/^OPEN_SHELL_RELEASE="v([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' \
    "${root}/scripts/openshell-adapter-e2e.sh"
)"
grep -Eq "^[[:space:]]*appVersion:[[:space:]]*\"?${openshell_version}\"?$" \
  "${root}/charts/openshell/Chart.yaml"
grep -Eq "^[[:space:]]+version:[[:space:]]*\"?${openshell_version}\"?$" \
  "${root}/charts/openshell/Chart.yaml"

agent_sandbox_version="$(
  sed -nE 's/^pinned_agent_sandbox = "v([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' \
    "${root}/conformance/register.toml"
)"
grep -Fq "appVersion: ${agent_sandbox_version}" \
  "${root}/charts/agent-sandbox/Chart.yaml"
spire_version="$(
  sed -nE 's/^pinned_spire = "([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' \
    "${root}/conformance/register.toml"
)"
grep -Fq "appVersion: ${spire_version}" "${root}/charts/spire/Chart.yaml"

# Inline secret material is never accepted by the wrapper contracts.
assert_refused() {
  local chart="$1" reason="$2"
  shift 2
  if helm template refused "${fixture}/charts/${chart}" \
    --namespace steward-test "$@" >/dev/null 2>&1; then
    echo "${chart} accepted ${reason}" >&2
    exit 1
  fi
}

assert_refused litellm "inline master-key material" \
  --set-string litellm.masterkey=inline-value
if helm template refused-secret-mismatch "${fixture}/charts/litellm" \
  --namespace steward-test \
  --set-string litellm.masterkeySecretName=secret-a \
  --set-string litellm.db.secret.name=secret-b >/dev/null 2>&1; then
  echo "litellm accepted multiple existing Secret names" >&2
  exit 1
fi
if helm template refused-openshell-secret-mismatch "${fixture}/charts/openshell" \
  --namespace steward-test \
  --set-string openshell.server.tls.certSecretName=other-secret >/dev/null 2>&1; then
  echo "openshell accepted multiple existing Secret names" >&2
  exit 1
fi

# Mutable image inputs must fail at chart validation for every wrapper.
mutable_image_cases='openshell openshell.image.tag
openshell openshell.supervisor.image.tag
litellm litellm.image.tag
spire spire.spire-server.image.tag
spire spire.spire-server.controllerManager.image.tag
spire spire.spire-server.chown.image.tag
spire spire.spire-agent.image.tag
spire spire.spiffe-csi-driver.image.tag
spire spire.spiffe-csi-driver.nodeDriverRegistrar.image.tag
arc arc.image.tag'
while read -r chart path; do
  if helm template refused-image "${fixture}/charts/${chart}" \
    --namespace steward-test \
    --set-string "${path}=mutable" >/dev/null 2>&1; then
    echo "${chart} accepted a mutable image tag at ${path}" >&2
    exit 1
  fi
done <<<"${mutable_image_cases}"
assert_refused openshell "a mutable sandbox image" \
  --set-string openshell.server.sandboxImage=registry.example.test/sandbox:mutable
if helm template refused-image "${fixture}/charts/agent-sandbox" \
  --namespace steward-test \
  --set-string image.digest=sha256:bad >/dev/null 2>&1; then
  echo "agent-sandbox accepted a malformed image digest" >&2
  exit 1
fi
assert_refused agent-sandbox "a malformed patch image digest" \
  --set-string crdNamespacePatch.image.digest=sha256:bad

# Features that would create secret material, duplicate routes, or mutable
# in-cluster dependencies remain fail-closed.
assert_refused openshell "the upstream PKI initialization job" \
  --set openshell.pkiInitJob.enabled=true
assert_refused openshell "the upstream certificate integration" \
  --set openshell.certManager.enabled=true
assert_refused openshell "the upstream route" \
  --set openshell.grpcRoute.enabled=true
assert_refused litellm "a bundled database" \
  --set litellm.db.deployStandalone=true
assert_refused litellm "a missing existing database" \
  --set litellm.db.useExisting=false
assert_refused litellm "generated proxy configuration" \
  --set litellm.proxyConfigMap.create=true
assert_refused litellm "the upstream ingress" \
  --set litellm.ingress.enabled=true
assert_refused spire "Namespace creation" \
  --set spire.global.spire.namespaces.create=true

# Routes are accepted only with the complete shared-Gateway contract.
for chart in openshell litellm; do
  complete_route=(
    --set route.enabled=true
    --set-string route.parentRefs[0].name=shared-gateway
    --set-string route.hostname="${chart}.example.test"
    --set-string route.timeouts.request=30s
    --set-string route.timeouts.backendRequest=20s
  )
  helm template complete-route "${fixture}/charts/${chart}" \
    --namespace steward-test "${complete_route[@]}" \
    >"${fixture}/${chart}-route.yaml"
  grep -Eq '^kind:[[:space:]]+(GRPC|HTTP)Route$' \
    "${fixture}/${chart}-route.yaml"

  for missing in parent hostname request backend; do
    case "${missing}" in
      parent) incomplete=(--set route.enabled=true --set-string route.hostname="${chart}.example.test" --set-string route.timeouts.request=30s --set-string route.timeouts.backendRequest=20s) ;;
      hostname) incomplete=(--set route.enabled=true --set-string route.parentRefs[0].name=shared-gateway --set-string route.timeouts.request=30s --set-string route.timeouts.backendRequest=20s) ;;
      request) incomplete=(--set route.enabled=true --set-string route.parentRefs[0].name=shared-gateway --set-string route.hostname="${chart}.example.test" --set-string route.timeouts.backendRequest=20s) ;;
      backend) incomplete=(--set route.enabled=true --set-string route.parentRefs[0].name=shared-gateway --set-string route.hostname="${chart}.example.test" --set-string route.timeouts.request=30s) ;;
    esac
    if helm template refused-route "${fixture}/charts/${chart}" \
      --namespace steward-test "${incomplete[@]}" >/dev/null 2>&1; then
      echo "${chart} accepted a route missing ${missing}" >&2
      exit 1
    fi
  done
done

# Components which do not live in the release namespace use explicit values.
helm template namespace-test "${fixture}/charts/agent-sandbox" \
  --namespace steward-test \
  --set-string targetNamespace=sandbox-system-test \
  >"${fixture}/agent-sandbox-namespace.yaml"
grep -Fq 'namespace: sandbox-system-test' "${fixture}/agent-sandbox-namespace.yaml"
if grep -Fq 'namespace: agent-sandbox-system' "${fixture}/agent-sandbox-namespace.yaml"; then
  echo 'agent-sandbox ignored targetNamespace' >&2
  exit 1
fi
helm template namespace-test "${fixture}/charts/arc" \
  --namespace steward-test \
  --set-string arc.namespaceOverride=arc-system-test \
  >"${fixture}/arc-namespace.yaml"
grep -Fq 'namespace: arc-system-test' "${fixture}/arc-namespace.yaml"
helm template namespace-test "${fixture}/charts/spire" \
  --namespace steward-test \
  --set-string spire.global.spire.namespaces.system.name=spire-system-test \
  --set-string spire.global.spire.namespaces.server.name=spire-server-test \
  >"${fixture}/spire-namespace.yaml"
grep -Fq 'namespace: spire-system-test' "${fixture}/spire-namespace.yaml"
grep -Fq 'namespace: spire-server-test' "${fixture}/spire-namespace.yaml"

test -f "${fixture}/charts/agent-sandbox/crds/sandboxes.agents.x-k8s.io.yaml"
test -f "${fixture}/charts/agent-sandbox/templates/upstream.yaml"

# Every dependency chart must remain renderable as a nested dependency without
# relying on the parent release name or namespace.
mkdir -p "${fixture}/umbrella"
{
  printf '%s\n' 'apiVersion: v2' 'name: dependency-render-test' \
    'version: 0.0.0' 'dependencies:'
  for chart in ${charts}; do
    printf '%s\n' \
      "  - name: ${chart}" \
      '    version: "*"' \
      "    repository: file://${fixture}/charts/${chart}"
  done
} >"${fixture}/umbrella/Chart.yaml"
helm dependency build "${fixture}/umbrella" >/dev/null
helm template nested "${fixture}/umbrella" \
  --namespace steward-test \
  --skip-tests >"${fixture}/nested.yaml"
if grep -Eq '^kind:[[:space:]]+Namespace$' "${fixture}/nested.yaml"; then
  echo 'dummy umbrella rendered a Namespace' >&2
  exit 1
fi

echo 'governed dependency chart contracts passed'
