#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'find "${fixture}" -depth -delete 2>/dev/null || true' EXIT INT TERM

for tool in awk cmp grep helm sed tar; do
  command -v "${tool}" >/dev/null || {
    echo "missing required steward-bundle test tool: ${tool}" >&2
    exit 2
  }
done

prepared="${fixture}/steward-bundle"
bash "${root}/scripts/prepare-steward-bundle-chart.sh" "${prepared}"

incomplete_release_output=""
if incomplete_release_output="$(
  STEWARD_BUNDLE_RELEASE_VERSION=9.8.7 \
    bash "${root}/scripts/prepare-steward-bundle-chart.sh" \
      "${fixture}/incomplete-release" 2>&1
)"; then
  echo 'steward-bundle preparation accepted incomplete release image evidence' >&2
  exit 1
fi
grep -Fq 'complete Steward bundle release image evidence is required' \
  <<<"${incomplete_release_output}" || {
  echo 'steward-bundle preparation refused incomplete release evidence for an unexpected reason' >&2
  exit 1
}
cp "${root}/charts/steward-bundle/values.yaml" "${fixture}/invalid-release-values.yaml"
if python3 "${root}/scripts/set-steward-bundle-release-images.py" \
  "${fixture}/invalid-release-values.yaml" 9.8.7 invalid \
  "sha256:$(printf 'b%.0s' {1..64})" \
  "sha256:$(printf 'c%.0s' {1..64})" \
  "sha256:$(printf 'd%.0s' {1..64})" \
  "sha256:$(printf 'e%.0s' {1..64})" >/dev/null 2>&1; then
  echo 'steward-bundle preparation accepted a malformed release digest' >&2
  exit 1
fi
cp "${root}/charts/steward-bundle/values.yaml" \
  "${fixture}/invalid-release-repository-values.yaml"
if STEWARD_BUNDLE_RELEASE_IMAGE_REPOSITORY=https://registry.example.test/steward \
  python3 "${root}/scripts/set-steward-bundle-release-images.py" \
    "${fixture}/invalid-release-repository-values.yaml" 9.8.7 \
    "sha256:$(printf 'a%.0s' {1..64})" \
    "sha256:$(printf 'b%.0s' {1..64})" \
    "sha256:$(printf 'c%.0s' {1..64})" \
    "sha256:$(printf 'd%.0s' {1..64})" \
    "sha256:$(printf 'e%.0s' {1..64})" >/dev/null 2>&1; then
  echo 'steward-bundle preparation accepted an invalid release image repository' >&2
  exit 1
fi

release_prepared="${fixture}/steward-bundle-release"
STEWARD_BUNDLE_RELEASE_VERSION=9.8.7 \
  STEWARD_BUNDLE_RELEASE_IMAGE_REPOSITORY=registry.example.test/steward \
  STEWARD_BUNDLE_RELEASE_APISERVER_DIGEST="sha256:$(printf 'a%.0s' {1..64})" \
  STEWARD_BUNDLE_RELEASE_CONTROLLER_DIGEST="sha256:$(printf 'b%.0s' {1..64})" \
  STEWARD_BUNDLE_RELEASE_MINT_DIGEST="sha256:$(printf 'c%.0s' {1..64})" \
  STEWARD_BUNDLE_RELEASE_WEB_DIGEST="sha256:$(printf 'd%.0s' {1..64})" \
  STEWARD_BUNDLE_RELEASE_TRIGGER_RECEIVER_DIGEST="sha256:$(printf 'e%.0s' {1..64})" \
  bash "${root}/scripts/prepare-steward-bundle-chart.sh" "${release_prepared}"
release_values="${release_prepared}/values.yaml"
if [[ "$(awk '
  $1 == "repository:" && $0 ~ /^    / { print $2; exit }
' "${release_values}")" != registry.example.test/steward ]]; then
  echo 'prepared release bundle omitted its release image repository' >&2
  exit 1
fi
for component_digest in \
  "apiserver apiserver $(printf 'a%.0s' {1..64})" \
  "controller controller $(printf 'b%.0s' {1..64})" \
  "mint mint $(printf 'c%.0s' {1..64})" \
  "web web $(printf 'd%.0s' {1..64})" \
  "triggerReceiver trigger-receiver $(printf 'e%.0s' {1..64})"; do
  read -r component suffix digest <<<"${component_digest}"
  values="$(awk -v component="${component}" '
    $1 == component ":" && $0 ~ /^    / { active = 1; next }
    active && $1 == "tag:" { tag = $2 }
    active && $1 == "digest:" { print tag, $2; exit }
  ' "${release_values}")"
  if [[ "${values}" != "9.8.7-${suffix} sha256:${digest}" ]]; then
    echo "prepared release bundle omitted ${component} release evidence" >&2
    exit 1
  fi
done

helm lint "${prepared}"
helm template bundle "${prepared}" --namespace steward-test \
  --include-crds >"${fixture}/default.yaml"

for source in steward spire agentSandbox openshell litellm; do
  grep -Fq "# Source: steward-bundle/charts/${source}/" \
    "${fixture}/default.yaml" || {
    echo "steward-bundle default render omitted ${source}" >&2
    exit 1
  }
done
if grep -Fq '# Source: steward-bundle/charts/arc/' "${fixture}/default.yaml"; then
  echo 'steward-bundle enabled ARC by default' >&2
  exit 1
fi
if grep -Eq '^kind: (Namespace|Secret)$' "${fixture}/default.yaml"; then
  echo 'steward-bundle rendered a Namespace or Secret' >&2
  exit 1
fi
if ! grep -Fxq 'kind: CustomResourceDefinition' "${fixture}/default.yaml" \
  || ! grep -Fxq 'kind: ClusterSPIFFEID' "${fixture}/default.yaml"; then
  echo 'steward-bundle default render omitted required CRDs or SPIRE identities' >&2
  exit 1
fi
rendered_spiffe_ids="$(awk '
  /^kind: ClusterSPIFFEID$/ { identity = 1; next }
  identity && /^  name:/ { print $2; identity = 0 }
' "${fixture}/default.yaml" | sort)"
expected_spiffe_ids="$(printf '%s\n' \
  steward-test-bundle-default \
  steward-test-bundle-openshell-sandboxes)"
if [[ "${rendered_spiffe_ids}" != "${expected_spiffe_ids}" ]]; then
  echo 'steward-bundle default render included an unexpected SPIRE identity' >&2
  diff -u <(printf '%s\n' "${expected_spiffe_ids}") \
    <(printf '%s\n' "${rendered_spiffe_ids}") >&2 || true
  exit 1
fi
if ! awk '
  /^kind:/ {
    if ($2 == "CustomResourceDefinition" && saw_object) exit 1
    if ($2 != "CustomResourceDefinition") saw_object = 1
  }
' "${fixture}/default.yaml"; then
  echo 'steward-bundle rendered a CRD after a dependent object' >&2
  exit 1
fi
while IFS= read -r image; do
  if [[ "${image}" != *'@sha256:'* ]]; then
    echo "steward-bundle rendered a mutable image: ${image}" >&2
    exit 1
  fi
done < <(sed -nE 's/^[[:space:]]*image:[[:space:]]*"?([^"[:space:]]+)"?.*/\1/p' \
  "${fixture}/default.yaml")

all_disabled=(
  --set components.steward.enabled=false
  --set components.spire.enabled=false
  --set components.agentSandbox.enabled=false
  --set components.openshell.enabled=false
  --set components.litellm.enabled=false
  --set components.arc.enabled=false
)
component_cases='steward steward
spire spire
agentSandbox agentSandbox
openshell openshell
litellm litellm
arc arc'
while read -r component source; do
  release="only-${component,,}"
  helm template "${release}" "${prepared}" --namespace steward-test \
    --include-crds "${all_disabled[@]}" \
    --set "components.${component}.enabled=true" \
    >"${fixture}/only-${component}.yaml"
  grep -Fq "# Source: steward-bundle/charts/${source}/" \
    "${fixture}/only-${component}.yaml" || {
    echo "enabling only ${component} did not render its objects" >&2
    exit 1
  }
  if grep -Eq "# Source: steward-bundle/charts/($(printf '%s\n' steward spire agentSandbox openshell litellm arc | grep -Fvx "${source}" | paste -sd '|' -))/" \
    "${fixture}/only-${component}.yaml"; then
    echo "enabling only ${component} rendered another subchart" >&2
    exit 1
  fi
done <<<"${component_cases}"

helm template none "${prepared}" --namespace steward-test \
  --include-crds "${all_disabled[@]}" >"${fixture}/none.yaml"
if grep -Eq '^(kind:|# Source:)' "${fixture}/none.yaml"; then
  echo 'disabling every steward-bundle component still rendered objects' >&2
  exit 1
fi

assert_refused() {
  local reason="$1" expected="$2"
  shift 2
  local output=""
  if output="$(helm template refused "${prepared}" --namespace steward-test \
    "$@" 2>&1)"; then
    echo "steward-bundle accepted ${reason}" >&2
    exit 1
  fi
  if ! grep -Fq "${expected}" <<<"${output}"; then
    echo "steward-bundle refused ${reason} for an unexpected reason" >&2
    printf '%s\n' "${output}" >&2
    exit 1
  fi
}

assert_secret_mismatch_refused() {
  local path="$1"
  shift
  local expected='All enabled charts must use one existing Secret' output=""
  if [[ "${path}" == openshell.openshell.* ]]; then
    expected='OpenShell secret references must name one existing Secret'
  elif [[ "${path}" == litellm.litellm.environmentSecrets* ]]; then
    expected='LiteLLM environment secret references must name the existing Secret'
  elif [[ "${path}" == litellm.litellm.* ]]; then
    expected='LiteLLM secret references must name one existing Secret'
  fi
  if output="$(helm template refused "${prepared}" --namespace steward-test \
    --show-only templates/validate.yaml --skip-schema-validation \
    "$@" --set-string "${path}=other-secret" 2>&1)"; then
    echo "steward-bundle accepted a mismatched Secret at ${path}" >&2
    exit 1
  fi
  if ! grep -Fq "${expected}" <<<"${output}"; then
    echo "steward-bundle refused a mismatched Secret at ${path} for an unexpected reason" >&2
    printf '%s\n' "${output}" >&2
    exit 1
  fi
}

assert_refused 'an inline LiteLLM master key' 'Must not validate the schema (not)' \
  --set-string litellm.litellm.masterkey=not-a-real-secret
assert_refused 'an incomplete LiteLLM route' 'parentRefs' \
  --set litellm.route.enabled=true
assert_refused 'an incomplete OpenShell route' 'parentRefs' \
  --set openshell.route.enabled=true
assert_refused 'a root global override' 'global: Must have at most 0 properties' \
  --set global.spire.namespaces.create=true
assert_refused 'a nested reserved global override' \
  'agentSandbox.global: Must have at most 0 properties' \
  --set agentSandbox.global.escape=true
assert_refused 'nested SPIRE Namespace creation' \
  'spire.global.spire.namespaces.create does not match: false' \
  --set spire.global.spire.namespaces.create=true
assert_refused 'different existing Secret names across enabled charts' \
  'All enabled charts must use one existing Secret' \
  --set-string litellm.litellm.masterkeySecretName=other-secret \
  --set-string litellm.litellm.environmentSecrets[0]=other-secret \
  --set-string litellm.litellm.db.secret.name=other-secret
assert_refused 'a different Steward database CA Secret' \
  'All enabled charts must use one existing Secret' \
  --set-string steward.databaseTls.mode=verify-full \
  --set-string steward.databaseTls.ca.kind=Secret \
  --set-string steward.databaseTls.ca.name=other-secret \
  --set-string steward.databaseTls.ca.key=ca.crt
assert_refused 'a different SPIRE database Secret' \
  'All enabled charts must use one existing Secret' \
  --set-string spire.spire.spire-server.dataStore.sql.databaseType=postgres \
  --set spire.spire.spire-server.dataStore.sql.externalSecret.enabled=true \
  --set-string spire.spire.spire-server.dataStore.sql.externalSecret.name=other-secret \
  --set-string spire.spire.spire-server.dataStore.sql.externalSecret.key=database-password

secret_mismatch_paths=(
  steward.secrets.database.name
  steward.secrets.managedInferenceDatabase.name
  steward.secrets.managedInferenceEncryptionKey.name
  steward.secrets.jira.name
  steward.secrets.litellm.name
  steward.secrets.openshellClient.name
  steward.secrets.mint.name
  steward.tls.api.secretName
  steward.tls.webhook.secretName
  steward.databaseTls.ca.name
  steward.workloadExchangeTrust.name
  steward.triggerReceiver.taskApi.ca.name
  steward.browserAuth.google.clientSecret.name
  steward.githubSource.privateKeySecret.name
  steward.web.ingress.tlsSecretName
  steward.triggerReceiver.databaseSecret.name
  'steward.triggerReceiver.triggers[0].signingSecret.name'
  spire.spire.spire-server.dataStore.sql.externalSecret.name
  openshell.openshell.server.externalDbSecret
  openshell.openshell.server.tls.certSecretName
  openshell.openshell.server.tls.clientCaSecretName
  openshell.openshell.server.tls.clientTlsSecretName
  openshell.openshell.server.sandboxJwt.signingSecretName
  litellm.litellm.masterkeySecretName
  'litellm.litellm.environmentSecrets[0]'
  litellm.litellm.db.secret.name
)
trigger_prerequisites=(
  --set steward.triggerReceiver.enabled=true
  --set-string 'steward.triggerReceiver.triggers[0].id=PROJ-123'
  --set 'steward.triggerReceiver.triggers[0].enabled=true'
  --set-string 'steward.triggerReceiver.triggers[0].source=jira'
  --set-string 'steward.triggerReceiver.triggers[0].workflow=workflow@1'
  --set-string 'steward.triggerReceiver.triggers[0].service=service'
  --set-string 'steward.triggerReceiver.triggers[0].ownerUserId=usr_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
  --set-string 'steward.triggerReceiver.triggers[0].envelopeDigest=steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
  --set-string 'steward.triggerReceiver.triggers[0].signingSecret.name=governed-execution'
  --set-string 'steward.triggerReceiver.triggers[0].signingSecret.key=webhook-signing-key'
  --set 'steward.triggerReceiver.triggers[0].concurrencyLimit=1'
  --set 'steward.triggerReceiver.triggers[0].cooldownSeconds=0'
  --set-string 'steward.triggerReceiver.triggers[0].ignoredAccountIds[0]=account-1'
)
for path in "${secret_mismatch_paths[@]}"; do
  prerequisites=()
  case "${path}" in
    steward.databaseTls.ca.name)
      prerequisites+=(
        --set-string steward.databaseTls.mode=verify-full
        --set-string steward.databaseTls.ca.kind=Secret
        --set-string steward.databaseTls.ca.key=ca.crt
      )
      ;;
    steward.workloadExchangeTrust.name)
      prerequisites+=(
        "${trigger_prerequisites[@]}"
        --set-string steward.workloadExchangeTrust.kind=Secret
        --set-string steward.workloadExchangeTrust.caCertificate=ca.crt
      )
      ;;
    steward.triggerReceiver.taskApi.ca.name)
      prerequisites+=(
        "${trigger_prerequisites[@]}"
        --set-string steward.triggerReceiver.taskApi.ca.kind=Secret
        --set-string steward.triggerReceiver.taskApi.ca.key=ca.crt
      )
      ;;
    steward.triggerReceiver.databaseSecret.name|steward.triggerReceiver.triggers*)
      prerequisites+=("${trigger_prerequisites[@]}")
      ;;
    spire.spire.spire-server.dataStore.sql.externalSecret.name)
      prerequisites+=(
        --set-string spire.spire.spire-server.dataStore.sql.databaseType=postgres
        --set spire.spire.spire-server.dataStore.sql.externalSecret.enabled=true
        --set-string spire.spire.spire-server.dataStore.sql.externalSecret.key=database-password
      )
      ;;
  esac
  assert_secret_mismatch_refused "${path}" "${prerequisites[@]}"
  template_path="${path}"
  case "${path}" in
    'steward.triggerReceiver.triggers[0].signingSecret.name')
      template_path='steward.triggerReceiver.triggers[%d].signingSecret.name'
      ;;
    'litellm.litellm.environmentSecrets[0]')
      template_path='litellm.litellm.environmentSecrets[%d]'
      ;;
  esac
  grep -Fq "\"${template_path}\"" \
    "${root}/charts/steward-bundle/templates/validate.yaml" || {
    echo "steward-bundle validation omitted Secret path ${path}" >&2
    exit 1
  }
done

assert_refused 'Steward certificate creation in the bundle' \
  'The Steward bundle requires steward.tls.mode=customerSecret' \
  --set-string steward.tls.mode=certManager \
  --set-string steward.tls.issuerRef.name=example-issuer
assert_refused 'different OpenShell sandbox identity namespaces' \
  'SPIRE sandbox identities must select the OpenShell sandbox namespace' \
  --set-string "spire.spire.spire-server.controllerManager.identities.clusterSPIFFEIDs.openshell-sandboxes.namespaceSelector.matchLabels.kubernetes\\.io/metadata\\.name=other-sandboxes"

awk '
  found { sub(/^  /, ""); print }
  /^steward:$/ { found = 1 }
' "${root}/charts/steward-bundle/values.yaml" >"${fixture}/steward-values.yaml"
helm template bundle "${prepared}" --namespace steward-test --include-crds \
  "${all_disabled[@]}" --set components.steward.enabled=true \
  >"${fixture}/bundled-steward.yaml"
helm template bundle "${root}/charts/steward" --namespace steward-test --include-crds \
  --values "${fixture}/steward-values.yaml" >"${fixture}/standalone-steward.yaml"
sed 's|# Source: steward-bundle/charts/steward/|# Source: steward/|' \
  "${fixture}/bundled-steward.yaml" >"${fixture}/normalized-bundled-steward.yaml"
if ! cmp -s "${fixture}/normalized-bundled-steward.yaml" \
  "${fixture}/standalone-steward.yaml"; then
  echo 'steward-bundle changed the Steward chart render' >&2
  diff -u "${fixture}/standalone-steward.yaml" \
    "${fixture}/normalized-bundled-steward.yaml" >&2 || true
  exit 1
fi

chart_version="$(awk '$1 == "version:" { print $2; exit }' \
  "${root}/charts/steward-bundle/Chart.yaml")"
for chart in steward spire agent-sandbox openshell litellm arc; do
  dependency_version="$(awk -v chart="${chart}" '
    $1 == "-" && $2 == "name:" { active = ($3 == chart) }
    active && $1 == "version:" { print $2; exit }
  ' "${root}/charts/steward-bundle/Chart.yaml")"
  if [[ "${dependency_version}" != "${chart_version}" ]]; then
    echo "steward-bundle dependency ${chart} is not pinned to ${chart_version}" >&2
    exit 1
  fi
done

for mapping in \
  'steward.secrets.database.name' \
  'steward.secrets.managedInferenceDatabase.name' \
  'steward.secrets.managedInferenceEncryptionKey.name' \
  'steward.secrets.jira.name' \
  'steward.secrets.litellm.name' \
  'steward.secrets.openshellClient.name' \
  'steward.secrets.mint.name' \
  'steward.tls.api.secretName' \
  'steward.tls.webhook.secretName' \
  'steward.databaseTls.ca.name' \
  'steward.workloadExchangeTrust.name' \
  'steward.triggerReceiver.taskApi.ca.name' \
  'steward.browserAuth.google.clientSecret.name' \
  'steward.githubSource.privateKeySecret.name' \
  'steward.web.ingress.tlsSecretName' \
  'steward.triggerReceiver.databaseSecret.name' \
  'steward.triggerReceiver.triggers[].signingSecret.name' \
  'spire.spire.spire-server.dataStore.sql.externalSecret.name' \
  'openshell.openshell.server.externalDbSecret' \
  'openshell.openshell.server.tls.certSecretName' \
  'openshell.openshell.server.tls.clientCaSecretName' \
  'openshell.openshell.server.tls.clientTlsSecretName' \
  'openshell.openshell.server.sandboxJwt.signingSecretName' \
  'litellm.litellm.masterkeySecretName' \
  'litellm.litellm.environmentSecrets[]' \
  'litellm.litellm.db.secret.name'; do
  grep -Fq "\`${mapping}\`" "${root}/charts/steward-bundle/README.md" || {
    echo "steward-bundle README omitted Secret mapping ${mapping}" >&2
    exit 1
  }
done

for mapping in \
  'spire.spire.global.spire.namespaces.system.name' \
  'spire.spire.global.spire.namespaces.server.name' \
  'agentSandbox.agentSandbox.namespace.name' \
  'openshell.openshell.server.sandboxNamespace' \
  'arc.arc.namespaceOverride'; do
  grep -Fq "\`${mapping}\`" "${root}/charts/steward-bundle/README.md" || {
    echo "steward-bundle README omitted namespace mapping ${mapping}" >&2
    exit 1
  }
done

for prerequisite in \
  'master_key: os.environ/PROXY_MASTER_KEY' \
  'steward.tls.mode=customerSecret' \
  'both server and' \
  'client use' \
  'Ed25519 key'; do
  grep -Fq "${prerequisite}" "${root}/charts/steward-bundle/README.md" || {
    echo "steward-bundle README omitted prerequisite: ${prerequisite}" >&2
    exit 1
  }
done

for mapping in \
  'steward.images.apiserver.repository' \
  'steward.images.controller.repository' \
  'steward.images.mint.repository' \
  'steward.images.web.repository' \
  'steward.images.triggerReceiver.repository' \
  'spire.spire.spire-server.image.registry' \
  'spire.spire.spire-server.image.repository' \
  'spire.spire.spire-server.image.tag' \
  'spire.spire.spire-server.controllerManager.image.registry' \
  'spire.spire.spire-server.controllerManager.image.repository' \
  'spire.spire.spire-server.controllerManager.image.tag' \
  'spire.spire.spire-server.chown.image.registry' \
  'spire.spire.spire-server.chown.image.repository' \
  'spire.spire.spire-server.chown.image.tag' \
  'spire.spire.spire-agent.image.registry' \
  'spire.spire.spire-agent.image.repository' \
  'spire.spire.spire-agent.image.tag' \
  'spire.spire.spire-agent.fsGroupFix.image.registry' \
  'spire.spire.spire-agent.fsGroupFix.image.repository' \
  'spire.spire.spire-agent.fsGroupFix.image.tag' \
  'spire.spire.spire-agent.socketAlternate.image.registry' \
  'spire.spire.spire-agent.socketAlternate.image.repository' \
  'spire.spire.spire-agent.socketAlternate.image.tag' \
  'spire.spire.spiffe-csi-driver.image.registry' \
  'spire.spire.spiffe-csi-driver.image.repository' \
  'spire.spire.spiffe-csi-driver.image.tag' \
  'spire.spire.spiffe-csi-driver.nodeDriverRegistrar.image.registry' \
  'spire.spire.spiffe-csi-driver.nodeDriverRegistrar.image.repository' \
  'spire.spire.spiffe-csi-driver.nodeDriverRegistrar.image.tag' \
  'agentSandbox.agentSandbox.image.repository' \
  'openshell.openshell.image.repository' \
  'openshell.openshell.supervisor.image.repository' \
  'openshell.openshell.server.sandboxImage' \
  'litellm.litellm.image.repository' \
  'arc.arc.image.repository'; do
  grep -Fq "\`${mapping}\`" "${root}/charts/steward-bundle/README.md" || {
    echo "steward-bundle README omitted image mapping ${mapping}" >&2
    exit 1
  }
done

echo 'steward-bundle chart contract passed'
