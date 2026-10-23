#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'find "${fixture}" -depth -delete 2>/dev/null || true' EXIT INT TERM

report_unexpected_failure() {
  local status="$1" line="$2" command="$3"
  trap - ERR
  printf 'governed chart test failed at line %s: %s (exit %s)\n' \
    "${line}" "${command}" "${status}" >&2
  exit "${status}"
}
trap 'report_unexpected_failure "$?" "$LINENO" "$BASH_COMMAND"' ERR

printf '%s\n' 'apiVersion: v1' 'kind: Config' >"${fixture}/kubeconfig"
export KUBECONFIG="${fixture}/kubeconfig"

charts="spire agent-sandbox openshell litellm arc"
command -v curl >/dev/null || {
  echo 'missing governed chart test tool: curl' >&2
  exit 2
}

require_fixed() {
  local expected="$1" path="$2" description="$3"
  if ! grep -Fq -- "${expected}" "${path}"; then
    echo "governed chart assertion failed: ${description}" >&2
    exit 1
  fi
}

require_fixed_count() {
  local expected="$1" minimum="$2" path="$3" description="$4" actual
  actual="$(
    awk -v expected="${expected}" '
      index($0, expected) { count += 1 }
      END { print count + 0 }
    ' "${path}"
  )"
  if ((actual < minimum)); then
    echo "governed chart assertion failed: ${description}" >&2
    exit 1
  fi
}

require_regex() {
  local expected="$1" path="$2" description="$3"
  if ! grep -Eq -- "${expected}" "${path}"; then
    echo "governed chart assertion failed: ${description}" >&2
    exit 1
  fi
}

require_file() {
  local path="$1" description="$2"
  if [[ ! -f "${path}" ]]; then
    echo "governed chart assertion failed: ${description}" >&2
    exit 1
  fi
}

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
    --namespace steward-test >"${rendered}"
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
  package_contents="${fixture}/${chart}-package-contents.txt"
  tar -tzf "${package}" >"${package_contents}"
  for packaged_file in LICENSE THIRD_PARTY_NOTICES.md; do
    if ! grep -Fxq "${chart}/${packaged_file}" "${package_contents}"; then
      echo "${chart} package omitted ${packaged_file}" >&2
      exit 1
    fi
  done
  upstream_license="UPSTREAM_LICENSES/${chart}-LICENSE"
  if ! grep -Fxq "${chart}/${upstream_license}" "${package_contents}"; then
    echo "${chart} package omitted ${upstream_license}" >&2
    exit 1
  fi
  if [[ "${chart}" == spire || "${chart}" == agent-sandbox \
    || "${chart}" == litellm ]]; then
    if ! grep -Fxq "${chart}/UPSTREAM_MODIFICATIONS.md" \
      "${package_contents}"; then
      echo "${chart} package omitted UPSTREAM_MODIFICATIONS.md" >&2
      exit 1
    fi
  fi
  if grep -Fq "${chart}/.upstream/" "${package_contents}"; then
    echo "${chart} package included its chart-preparation source tree" >&2
    exit 1
  fi

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

database_user="$(printf '$%s' '(DATABASE_USERNAME)')"
database_auth="$(printf '$%s' '(DATABASE_PASSWORD)')"
database_host="$(printf '$%s' '(DATABASE_HOST)')"
database_name="$(printf '$%s' '(DATABASE_NAME)')"
database_url="postgresql://${database_user}"
database_url+=":${database_auth}@${database_host}/${database_name}"
require_fixed "${database_url}" "${fixture}/litellm.yaml" \
  'LiteLLM did not render its Secret-backed database URL'

# Upstream-backed wrappers may own integration resources and validation only.
# They must not copy or override upstream workloads.
check_wrapper_boundary() {
  local chart_directory="$1"
  bash "${root}/scripts/check-governed-chart-wrapper.sh" "${chart_directory}"
}

for chart in ${charts}; do
  if ! check_wrapper_boundary "${fixture}/charts/${chart}" >/dev/null 2>&1; then
    echo "${chart} contains a wrapper-owned upstream workload template" >&2
    exit 1
  fi
done

mkdir -p "${fixture}/mutations"
cp -R "${fixture}/charts/litellm" "${fixture}/mutations/litellm-helper"
printf '%s\n' '{{- define "litellm.fullname" -}}hijacked{{- end -}}' \
  >>"${fixture}/mutations/litellm-helper/templates/_helpers.tpl"
helper_guard_output=""
if helper_guard_output="$(
  check_wrapper_boundary "${fixture}/mutations/litellm-helper" 2>&1
)"; then
  echo 'wrapper guard accepted an upstream helper override' >&2
  exit 1
fi
if ! grep -Fq 'litellm wrapper redefines an upstream template name' \
  <<<"${helper_guard_output}"; then
  echo 'wrapper guard rejected the helper override for an unexpected reason' >&2
  exit 1
fi

cp -R "${fixture}/charts/litellm" "${fixture}/mutations/litellm-block"
printf '%s\n' '{{- block "litellm.fullname" . -}}hijacked{{- end -}}' \
  >>"${fixture}/mutations/litellm-block/templates/_helpers.tpl"
block_guard_output=""
if block_guard_output="$(
  check_wrapper_boundary "${fixture}/mutations/litellm-block" 2>&1
)"; then
  echo 'wrapper guard accepted an upstream helper block override' >&2
  exit 1
fi
if ! grep -Fq 'litellm wrapper redefines an upstream template name' \
  <<<"${block_guard_output}"; then
  echo 'wrapper guard rejected the helper block override for an unexpected reason' >&2
  exit 1
fi

mkdir -p "${fixture}/without-rg"
printf '%s\n' '#!/bin/sh' 'exit 127' >"${fixture}/without-rg/rg"
chmod +x "${fixture}/without-rg/rg"
helper_guard_without_rg=""
if helper_guard_without_rg="$(
  PATH="${fixture}/without-rg:${PATH}" \
    check_wrapper_boundary "${fixture}/mutations/litellm-helper" 2>&1
)"; then
  echo 'wrapper guard accepted an upstream helper override without ripgrep' >&2
  exit 1
fi
if ! grep -Fq 'litellm wrapper redefines an upstream template name' \
  <<<"${helper_guard_without_rg}"; then
  echo 'wrapper guard without ripgrep failed for an unexpected reason' >&2
  exit 1
fi

for chart in openshell agent-sandbox; do
  mutation="${fixture}/mutations/${chart}-deployment"
  cp -R "${fixture}/charts/${chart}" "${mutation}"
  mkdir -p "${mutation}/templates"
  printf '%s\n' \
    'apiVersion: apps/v1' \
    'kind: Deployment' \
    'metadata:' \
    '  name: injected' \
    'spec:' \
    '  selector:' \
    '    matchLabels:' \
    '      app: injected' \
    '  template:' \
    '    metadata:' \
    '      labels:' \
    '        app: injected' \
    '    spec:' \
    '      containers:' \
    '        - name: injected' \
    '          image: registry.example.test/injected@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
    >"${mutation}/templates/extra-route.yaml"
  workload_guard_output=""
  if workload_guard_output="$(check_wrapper_boundary "${mutation}" 2>&1)"; then
    echo "wrapper guard accepted an injected ${chart} Deployment" >&2
    exit 1
  fi
  if ! grep -Fq \
    "${chart}/templates/ wrapper rendered forbidden kind Deployment" \
    <<<"${workload_guard_output}"; then
    echo "wrapper guard rejected the injected ${chart} Deployment for an unexpected reason" >&2
    exit 1
  fi
done

# LiteLLM must use the one reviewed image coordinate already exercised by
# conformance. This assertion makes the repeated chart value fail on drift.
litellm_pin="$(
  sed -nE 's/^LATEST_IMAGE="([^"]+)"$/\1/p' \
    "${root}/scripts/g4-upstream-conformance"
)"
require_fixed "${litellm_pin##*@}" "${root}/charts/litellm/values.yaml" \
  'LiteLLM values do not match the conformance image digest'
require_fixed 'appVersion: 1.106.2' "${root}/charts/litellm/Chart.yaml" \
  'LiteLLM appVersion does not match the pinned upstream release'

# The OpenShell application version is already a released install contract.
openshell_version="$(
  sed -nE 's/^OPEN_SHELL_RELEASE="v([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' \
    "${root}/scripts/openshell-adapter-e2e.sh"
)"
require_regex \
  "^[[:space:]]*appVersion:[[:space:]]*\"?${openshell_version}\"?$" \
  "${root}/charts/openshell/Chart.yaml" \
  'OpenShell appVersion does not match the adapter pin'
require_regex \
  "^[[:space:]]+version:[[:space:]]*\"?${openshell_version}\"?$" \
  "${root}/charts/openshell/Chart.yaml" \
  'OpenShell dependency version does not match the adapter pin'

agent_sandbox_version="$(
  sed -nE 's/^pinned_agent_sandbox = "v([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' \
    "${root}/conformance/register.toml"
)"
require_fixed "appVersion: ${agent_sandbox_version}" \
  "${root}/charts/agent-sandbox/Chart.yaml" \
  'agent-sandbox appVersion does not match the conformance pin'
spire_version="$(
  sed -nE 's/^pinned_spire = "([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' \
    "${root}/conformance/register.toml"
)"
require_fixed "appVersion: ${spire_version}" \
  "${root}/charts/spire/Chart.yaml" \
  'SPIRE appVersion does not match the conformance pin'
require_fixed '--version 0.29.0' "${root}/scripts/openshell-testbed.sh" \
  'OpenShell testbed does not match the wrapped SPIRE chart version'
require_fixed '--version 0.5.0' "${root}/scripts/openshell-testbed.sh" \
  'OpenShell testbed does not match the vendored SPIRE CRD version'
spire_doc_pin="SPIRE | \`spire\` chart 0.29.0, \`spire-crds\` chart 0.5.0"
require_fixed "${spire_doc_pin}" \
  "${root}/docs/installation/installation-guide.md" \
  'installation guide does not record the wrapped SPIRE chart versions'
base_sandbox_image="$(
  sed -nE 's/^pinned_base_sandbox_image = "([^"]+)"$/\1/p' \
    "${root}/conformance/register.toml"
)"
require_fixed "sandboxImage: ${base_sandbox_image}" \
  "${root}/charts/openshell/values.yaml" \
  'OpenShell sandbox image does not match the conformance pin'
if grep -Eq '^[[:space:]]+- name: spire-crds$' \
  "${root}/charts/spire/Chart.yaml"; then
  echo 'spire still depends on the late-rendered upstream CRD chart' >&2
  exit 1
fi
for crd in \
  spire.spiffe.io_clusterfederatedtrustdomains.yaml \
  spire.spiffe.io_clusterspiffeids.yaml \
  spire.spiffe.io_clusterstaticentries.yaml; do
  if [[ ! -f "${root}/charts/spire/crds/${crd}" ]]; then
    echo "spire omitted vendored CRD: ${crd}" >&2
    exit 1
  fi
done

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
assert_refused litellm "inline credential-shaped environment variable" \
  --set-string litellm.envVars.OPENAI_API_KEY=inline-value
inline_material=inline-value
inline_db_url="$(printf '%s' 'postgresql://' 'alice' ':' "${inline_material}" \
  '@' 'database.example.test/litellm')"
inline_https_url="$(printf '%s' 'https://' 'alice' ':' "${inline_material}" \
  '@' 'gateway.example.test/v1')"
assert_refused litellm "an inline DATABASE_URL environment variable" \
  --set-string "litellm.envVars.DATABASE_URL=${inline_db_url}"
assert_refused litellm "URL user information in an environment variable" \
  --set-string "litellm.envVars.MODEL_ENDPOINT=${inline_https_url}"
assert_refused litellm "an inline extra environment variable" \
  --set-json 'litellm.extraEnvVars=[{"name":"OPENAI_API_KEY","value":"inline"}]'
assert_refused litellm "a password-bearing database URL" \
  --set-string "litellm.db.url=${inline_db_url}"
assert_refused litellm "an inline read-replica database URL" \
  --set-string "litellm.db.readReplicaUrl=${inline_db_url}"
assert_refused litellm "the bundled Redis chart with an inline password" \
  --set litellm.redis.enabled=true \
  --set-string "litellm.redis.auth.password=${inline_material}"
assert_refused litellm "an extra Secret resource" \
  --set-json 'litellm.extraResources=[{"apiVersion":"v1","kind":"Secret","metadata":{"name":"inline"},"stringData":{"key":"inline"}}]'
assert_refused litellm "an extra Namespace resource" \
  --set-json 'litellm.extraResources=[{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"extra"}}]'
if helm template refused-secret-mismatch "${fixture}/charts/litellm" \
  --namespace steward-test \
  --set-string litellm.masterkeySecretName=secret-a \
  --set-string litellm.db.secret.name=secret-b >/dev/null 2>&1; then
  echo "litellm accepted multiple existing Secret names" >&2
  exit 1
fi
assert_refused litellm "a mismatched environment Secret name" \
  --set-string litellm.environmentSecrets[0]=other-secret
if helm template refused-openshell-secret-mismatch "${fixture}/charts/openshell" \
  --namespace steward-test \
  --set-string openshell.server.tls.certSecretName=other-secret >/dev/null 2>&1; then
  echo "openshell accepted multiple existing Secret names" >&2
  exit 1
fi
assert_refused openshell "a mismatched sandbox JWT Secret name" \
  --set-string openshell.server.sandboxJwt.signingSecretName=other-secret
assert_refused spire "a SQL datastore without an external Secret" \
  --set-string spire.spire-server.dataStore.sql.databaseType=postgres
assert_refused spire "an inline SQL datastore password" \
  --set-string "spire.spire-server.dataStore.sql.password=${inline_material}"
assert_refused spire "an inline read-only SQL datastore password" \
  --set-string "spire.spire-server.dataStore.sql.readOnly.password=${inline_material}"
assert_refused spire "an inline cloud access-key ID" \
  --set-string spire.spire-server.secrets.aws.accessKeyID=inline-value
assert_refused spire "an inline cloud secret-access key" \
  --set-string spire.spire-server.secrets.aws.secretAccessKey=inline-value
assert_refused spire "an inline KMS access-key ID" \
  --set-string spire.spire-server.keyManager.awsKMS.accessKeyID=inline-value
assert_refused spire "an inline KMS secret-access key" \
  --set-string spire.spire-server.keyManager.awsKMS.secretAccessKey=inline-value
assert_refused spire "inline application credentials" \
  --set-string spire.spire-server.secrets.gcp.applicationCredentials=inline-value
assert_refused spire "an inline upstream authority Secret" \
  --set spire.spire-server.upstreamAuthority.disk.secret.create=true \
  --set-string spire.spire-server.upstreamAuthority.disk.secret.data.key=inline-value
assert_refused spire "an inline kubeconfig" \
  --set-string spire.spire-server.kubeConfigs.external.kubeConfig=inline-value

helm template external-sql "${fixture}/charts/spire" \
  --namespace steward-test \
  --set-string spire.spire-server.dataStore.sql.databaseType=postgres \
  --set spire.spire-server.dataStore.sql.externalSecret.enabled=true \
  --set-string spire.spire-server.dataStore.sql.externalSecret.name=governed-execution \
  --set-string spire.spire-server.dataStore.sql.externalSecret.key=spire-database-password \
  >/dev/null

# Mutable image inputs must fail at chart validation for every wrapper.
mutable_image_cases='openshell openshell.image.tag
openshell openshell.supervisor.image.tag
litellm litellm.image.tag
spire spire.spire-server.image.tag
spire spire.spire-server.controllerManager.image.tag
spire spire.spire-server.chown.image.tag
spire spire.spire-agent.image.tag
spire spire.spire-agent.fsGroupFix.image.tag
spire spire.spire-agent.socketAlternate.image.tag
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
assert_refused agent-sandbox "a mutable controller image" \
  --set-string agentSandbox.image.tag=v0.5.0
assert_refused agent-sandbox "Namespace creation" \
  --set agentSandbox.namespace.create=true

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
for namespace_create_path in \
  spire.global.spire.namespaces.create \
  spire.global.spire.namespaces.system.create \
  spire.global.spire.namespaces.server.create; do
  assert_refused spire "Namespace creation at ${namespace_create_path}" \
    --set "${namespace_create_path}=true"
done

# Routes are accepted only with the complete shared-Gateway contract.
validate_openshell_route_schema() {
  local rendered_route="$1"
  local scope field invalid=0 saw_rule=0

  # Compact property snapshot from Gateway API v1.3.0's GRPCRoute CRD:
  # https://github.com/kubernetes-sigs/gateway-api/blob/v1.3.0/config/crd/standard/gateway.networking.k8s.io_grpcroutes.yaml
  # Source CRD SHA-256:
  # 5867279c3cfd77377fe7e9255b22751f17592242b9b5e7485c74b13b268d13c5
  local spec_fields='hostnames parentRefs rules'
  local rule_fields='backendRefs filters matches'

  grep -Fxq 'apiVersion: gateway.networking.k8s.io/v1' \
    "${rendered_route}" || {
    echo 'OpenShell route does not use the pinned Gateway API version' >&2
    return 1
  }
  grep -Fxq 'kind: GRPCRoute' "${rendered_route}" || {
    echo 'OpenShell route is not a GRPCRoute' >&2
    return 1
  }

  while read -r scope field; do
    case "${scope}" in
      spec)
        if [[ " ${spec_fields} " != *" ${field} "* ]]; then
          echo "OpenShell GRPCRoute spec field is absent from Gateway API v1.3.0: ${field}" >&2
          invalid=1
        fi
        ;;
      rule)
        saw_rule=1
        if [[ " ${rule_fields} " != *" ${field} "* ]]; then
          echo "OpenShell GRPCRoute rule field is absent from Gateway API v1.3.0: ${field}" >&2
          invalid=1
        fi
        ;;
    esac
  done < <(
    awk '
      /^---$/ {
        in_route = 0
        in_spec = 0
        in_rules = 0
      }
      /^kind:[[:space:]]+GRPCRoute[[:space:]]*$/ {
        in_route = 1
        next
      }
      in_route && /^spec:[[:space:]]*$/ {
        in_spec = 1
        next
      }
      in_spec && /^  [A-Za-z][A-Za-z0-9]*:[[:space:]]*/ {
        field = $0
        sub(/^  /, "", field)
        sub(/:.*/, "", field)
        print "spec", field
        in_rules = (field == "rules")
        next
      }
      in_rules && /^    - [A-Za-z][A-Za-z0-9]*:[[:space:]]*/ {
        field = $0
        sub(/^    - /, "", field)
        sub(/:.*/, "", field)
        print "rule", field
        next
      }
      in_rules && /^      [A-Za-z][A-Za-z0-9]*:[[:space:]]*/ {
        field = $0
        sub(/^      /, "", field)
        sub(/:.*/, "", field)
        print "rule", field
      }
    ' "${rendered_route}"
  )

  if ((saw_rule == 0)); then
    echo 'OpenShell GRPCRoute has no rule to validate' >&2
    return 1
  fi
  ((invalid == 0))
}

mkdir -p "${fixture}/without-curl"
printf '%s\n' '#!/bin/sh' 'exit 97' >"${fixture}/without-curl/curl"
chmod +x "${fixture}/without-curl/curl"

for chart in openshell litellm; do
  complete_route=(--set route.enabled=true
    --set-string route.parentRefs[0].name=shared-gateway
    --set-string route.hostname="${chart}.example.test")
  if [[ "${chart}" == litellm ]]; then
    complete_route+=(--set-string route.timeouts.request=30s
      --set-string route.timeouts.backendRequest=20s)
  fi
  helm template complete-route "${fixture}/charts/${chart}" \
    --namespace steward-test "${complete_route[@]}" \
    >"${fixture}/${chart}-route.yaml"
  require_regex '^kind:[[:space:]]+(GRPC|HTTP)Route$' \
    "${fixture}/${chart}-route.yaml" \
    "${chart} did not render the enabled shared-Gateway route"

  if [[ "${chart}" == litellm ]]; then
    require_fixed 'request: "30s"' "${fixture}/${chart}-route.yaml" \
      'LiteLLM route omitted its request timeout'
    require_fixed 'backendRequest: "20s"' "${fixture}/${chart}-route.yaml" \
      'LiteLLM route omitted its backend-request timeout'
  fi

  if [[ "${chart}" == openshell ]]; then
    if ! PATH="${fixture}/without-curl:${PATH}" \
      validate_openshell_route_schema "${fixture}/${chart}-route.yaml"; then
      echo 'OpenShell route did not pass offline schema validation' >&2
      exit 1
    fi
    awk '
      { print }
      /^  rules:[[:space:]]*$/ {
        print "    - timeouts:"
        print "        request: 30s"
      }
    ' "${fixture}/${chart}-route.yaml" \
      >"${fixture}/${chart}-route-unknown-field.yaml"
    route_schema_output=""
    if route_schema_output="$(
      validate_openshell_route_schema \
        "${fixture}/${chart}-route-unknown-field.yaml" 2>&1
    )"; then
      echo 'OpenShell route schema accepted the unsupported timeouts field' >&2
      exit 1
    fi
    if ! grep -Fq \
      'OpenShell GRPCRoute rule field is absent from Gateway API v1.3.0: timeouts' \
      <<<"${route_schema_output}"; then
      echo 'OpenShell route schema refused the mutation for an unexpected reason' >&2
      exit 1
    fi
  fi

  missing_route_fields="parent hostname"
  if [[ "${chart}" == litellm ]]; then
    missing_route_fields+=" request backend"
  fi
  for missing in ${missing_route_fields}; do
    case "${missing}" in
      parent) incomplete=(--set route.enabled=true --set-string route.hostname="${chart}.example.test") ;;
      hostname) incomplete=(--set route.enabled=true --set-string route.parentRefs[0].name=shared-gateway) ;;
      request) incomplete=(--set route.enabled=true --set-string route.parentRefs[0].name=shared-gateway --set-string route.hostname="${chart}.example.test" --set-string route.timeouts.backendRequest=20s) ;;
      backend) incomplete=(--set route.enabled=true --set-string route.parentRefs[0].name=shared-gateway --set-string route.hostname="${chart}.example.test" --set-string route.timeouts.request=30s) ;;
    esac
    if [[ "${chart}" == litellm && "${missing}" =~ ^(parent|hostname)$ ]]; then
      incomplete+=(--set-string route.timeouts.request=30s
        --set-string route.timeouts.backendRequest=20s)
    fi
    if helm template refused-route "${fixture}/charts/${chart}" \
      --namespace steward-test "${incomplete[@]}" >/dev/null 2>&1; then
      echo "${chart} accepted a route missing ${missing}" >&2
      exit 1
    fi
  done
done

helm template fullname-override "${fixture}/charts/litellm" \
  --namespace steward-test \
  --set-string litellm.fullnameOverride=llm \
  --set route.enabled=true \
  --set-string route.parentRefs[0].name=shared-gateway \
  --set-string route.hostname=litellm.example.test \
  --set-string route.timeouts.request=30s \
  --set-string route.timeouts.backendRequest=20s \
  >"${fixture}/litellm-fullname-override.yaml"
if ! awk '
  /^kind:[[:space:]]+HTTPRoute$/ { in_route = 1; next }
  in_route && /^    - backendRefs:[[:space:]]*$/ { in_backends = 1; next }
  in_backends && /^        - name:[[:space:]]+llm[[:space:]]*$/ { found = 1 }
  END { exit !found }
' "${fixture}/litellm-fullname-override.yaml"; then
  echo 'LiteLLM route did not honor the upstream fullname override' >&2
  exit 1
fi

for field in \
  'runAsNonRoot: true' \
  'runAsUser: 65534' \
  'runAsGroup: 65534' \
  'fsGroup: 65534' \
  'type: RuntimeDefault' \
  'allowPrivilegeEscalation: false' \
  'drop:' \
  '- ALL'; do
  require_fixed_count "${field}" 2 "${fixture}/litellm.yaml" \
    "LiteLLM workload renders omitted restricted Pod Security field: ${field}"
done

# Components which do not live in the release namespace use explicit values.
helm template namespace-test "${fixture}/charts/agent-sandbox" \
  --namespace steward-test \
  --set-string agentSandbox.namespace.name=sandbox-system-test \
  >"${fixture}/agent-sandbox-namespace.yaml"
require_fixed 'namespace: sandbox-system-test' \
  "${fixture}/agent-sandbox-namespace.yaml" \
  'agent-sandbox did not use its configured namespace'
if grep -Fq 'namespace: agent-sandbox-system' "${fixture}/agent-sandbox-namespace.yaml"; then
  echo 'agent-sandbox ignored agentSandbox.namespace.name' >&2
  exit 1
fi
if grep -Eq 'app.kubernetes.io/version:[[:space:]].*@sha256:' \
  "${fixture}/agent-sandbox-namespace.yaml"; then
  echo 'agent-sandbox rendered an image digest into a Kubernetes label' >&2
  exit 1
fi
require_regex 'app.kubernetes.io/version:[[:space:]]+"v0.5.0"$' \
  "${fixture}/agent-sandbox-namespace.yaml" \
  'agent-sandbox did not retain its release version label'
helm template namespace-test "${fixture}/charts/arc" \
  --namespace steward-test \
  --set-string arc.namespaceOverride=arc-system-test \
  >"${fixture}/arc-namespace.yaml"
require_fixed 'namespace: arc-system-test' \
  "${fixture}/arc-namespace.yaml" \
  'ARC did not use its configured namespace'
helm template namespace-test "${fixture}/charts/spire" \
  --namespace steward-test \
  --set-string spire.global.spire.namespaces.system.name=spire-system-test \
  --set-string spire.global.spire.namespaces.server.name=spire-server-test \
  >"${fixture}/spire-namespace.yaml"
require_fixed 'namespace: spire-system-test' "${fixture}/spire-namespace.yaml" \
  'SPIRE agent resources did not use the configured system namespace'
require_fixed 'namespace: spire-server-test' "${fixture}/spire-namespace.yaml" \
  'SPIRE server resources did not use the configured server namespace'

if [[ ! -d "${fixture}/charts/agent-sandbox/.upstream/agent-sandbox" ]]; then
  echo 'agent-sandbox did not wrap the pinned upstream Helm chart' >&2
  exit 1
fi
agent_wrapper_templates="${fixture}/charts/agent-sandbox/templates"
agent_wrapper_template=""
if [[ -d "${agent_wrapper_templates}" ]]; then
  agent_wrapper_template="$(
    find "${agent_wrapper_templates}" -type f -print -quit
  )"
fi
if [[ -n "${agent_wrapper_template}" ]]; then
  echo 'agent-sandbox contains wrapper-owned workload or hook templates' >&2
  exit 1
fi
for crd in \
  agents.x-k8s.io_sandboxes.yaml \
  extensions.agents.x-k8s.io_sandboxclaims.yaml \
  extensions.agents.x-k8s.io_sandboxtemplates.yaml \
  extensions.agents.x-k8s.io_sandboxwarmpools.yaml; do
  require_file \
    "${fixture}/charts/agent-sandbox/.upstream/agent-sandbox/crds/${crd}" \
    "agent-sandbox upstream chart omitted CRD ${crd}"
done

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
