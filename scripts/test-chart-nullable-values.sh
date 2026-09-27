#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
chart="${root}/charts/steward"
if [[ "$(bash "${root}/scripts/release-chart-contract.sh" "${chart}/Chart.yaml")" != customer-v1 ]]; then
  exit 0
fi

temporary_directory="$(mktemp -d)"
cleanup() {
  status="$?"
  trap - EXIT INT TERM
  rm -rf -- "${temporary_directory}"
  exit "${status}"
}
trap cleanup EXIT INT TERM

rendered="${temporary_directory}/rendered.yaml"
omitted_chart="${temporary_directory}/chart"
digest0="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
digest1="sha256:1111111111111111111111111111111111111111111111111111111111111111"
base_values=(
  --set-string images.repository=registry.example.test/customer/steward
  --set images.apiserver.tag=validation-apiserver
  --set "images.apiserver.digest=${digest0}"
  --set images.controller.tag=validation-controller
  --set "images.controller.digest=${digest1}"
  --set-string tls.webhook.caBundlePem=public-validation-ca
)
browser_values=(
  --set browserAuth.enabled=true
  --set-string browserAuth.google.clientId=google-client-id
  --set-string browserAuth.google.origin=https://steward.example.test
  --set-string browserAuth.google.workspaceDomain=example.test
  --set-string browserAuth.google.organizationId=org_example
  --set-string browserAuth.google.clientSecret.name=steward-google-oidc
  --set-string browserAuth.google.clientSecret.key=client-secret
  --set-string 'networkPolicy.browserAuthEgressCidrs[0]=203.0.113.0/24'
)
valid_custom_ceiling='{"revision":7,"spec":{"budget":{"currency":"USD","monthlyLimit":"25.00","singleRunLimit":"5.00"},"llms":[{"model":"gpt-5.4","provider":"openai"}],"runner":{"compute":"4","memory":"8Gi","platforms":["linux"],"storage":"20Gi"},"runtimeMinutesLimit":"45","tools":[{"action":"read","provider":"github","resource":"actions_get"}],"ttl":"4h"}}'
valid_steward_run_release='{"actionCommit":"4444444444444444444444444444444444444444","governedJobContainerImage":"registry.example.test/steward-run@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","manifestSchemaVersion":3,"version":"0.7.0","workflowCommit":"3333333333333333333333333333333333333333","workflowRepository":"example-org/steward-run"}'

render_chart() {
  local chart_path="$1"
  shift
  helm template steward "${chart_path}" --namespace steward "${base_values[@]}" "$@"
}

assert_environment_absent() {
  local environment_name="$1"
  local manifest="$2"
  if grep -Fq "name: ${environment_name}" "${manifest}"; then
    echo "nullable chart value unexpectedly projected ${environment_name}" >&2
    exit 1
  fi
}

assert_exact_json_environment() {
  local environment_name="$1"
  local expected_json="$2"
  local manifest="$3"
  local escaped_json="${expected_json//\"/\\\"}"
  local expected_line="            - { name: ${environment_name}, value: \"${escaped_json}\" }"
  if ! grep -Fxq "${expected_line}" "${manifest}"; then
    echo "${environment_name} did not contain the exact configured JSON" >&2
    exit 1
  fi
}

# The published null defaults must coalesce and render with browser administration disabled.
render_chart "${chart}" > "${rendered}"
assert_environment_absent STEWARD_CUSTOM_ENVELOPE_SAFETY_CEILING_JSON "${rendered}"
assert_environment_absent STEWARD_RUN_RELEASE_JSON "${rendered}"

# The same disabled configuration must work if a downstream chart omits both optional defaults.
cp -R "${chart}" "${omitted_chart}"
awk '
  $0 != "    customEnvelopeSafetyCeiling: null" &&
  $0 != "    stewardRunRelease: null" { print }
' "${chart}/values.yaml" > "${omitted_chart}/values.yaml"
render_chart "${omitted_chart}" > "${rendered}"
assert_environment_absent STEWARD_CUSTOM_ENVELOPE_SAFETY_CEILING_JSON "${rendered}"
assert_environment_absent STEWARD_RUN_RELEASE_JSON "${rendered}"

# An explicit null ceiling preserves the fail-closed runtime default and projects no environment.
render_chart "${chart}" \
  --set-json config.apiserver.customEnvelopeSafetyCeiling=null > "${rendered}"
assert_environment_absent STEWARD_CUSTOM_ENVELOPE_SAFETY_CEILING_JSON "${rendered}"

# A complete ceiling is accepted and projected without mutation.
render_chart "${chart}" \
  --set-json "config.apiserver.customEnvelopeSafetyCeiling=${valid_custom_ceiling}" > "${rendered}"
assert_exact_json_environment \
  STEWARD_CUSTOM_ENVELOPE_SAFETY_CEILING_JSON "${valid_custom_ceiling}" "${rendered}"

if render_chart "${chart}" \
  --set-json 'config.apiserver.customEnvelopeSafetyCeiling={"revision":0,"spec":{}}' \
  >/dev/null 2>&1
then
  echo 'chart accepted a malformed custom Envelope safety ceiling' >&2
  exit 1
fi

# Browser administration must fail closed for an absent or explicitly null release projection.
if render_chart "${omitted_chart}" "${browser_values[@]}" >/dev/null 2>&1; then
  echo 'browser administration accepted an absent steward-run release projection' >&2
  exit 1
fi
if render_chart "${chart}" "${browser_values[@]}" \
  --set-json config.apiserver.stewardRunRelease=null >/dev/null 2>&1
then
  echo 'browser administration accepted a null steward-run release projection' >&2
  exit 1
fi

# A complete release projection remains valid and reaches the apiserver exactly.
render_chart "${chart}" "${browser_values[@]}" \
  --set-json "config.apiserver.stewardRunRelease=${valid_steward_run_release}" > "${rendered}"
assert_exact_json_environment STEWARD_RUN_RELEASE_JSON "${valid_steward_run_release}" "${rendered}"

if render_chart "${chart}" \
  --set-json 'config.apiserver.stewardRunRelease={"manifestSchemaVersion":3}' \
  >/dev/null 2>&1
then
  echo 'chart accepted a malformed steward-run release projection' >&2
  exit 1
fi
