#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: steward-gateway-backend-tls-check.sh \
  --kubeconfig PATH --context NAME --namespace NAME --gateway-class NAME \
  --ca-config-map NAME --api-tls-secret NAME --public-url HTTPS_URL \
  [--cluster-domain DOMAIN] [--kubectl PATH] [--curl PATH]
EOF
  exit 2
}

kubeconfig=""
context=""
namespace=""
gateway_class=""
ca_config_map=""
api_tls_secret=""
public_url=""
cluster_domain="cluster.local"
kubectl_bin="kubectl"
curl_bin="curl"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --kubeconfig) kubeconfig="${2:-}"; shift 2 ;;
    --context) context="${2:-}"; shift 2 ;;
    --namespace) namespace="${2:-}"; shift 2 ;;
    --gateway-class) gateway_class="${2:-}"; shift 2 ;;
    --ca-config-map) ca_config_map="${2:-}"; shift 2 ;;
    --api-tls-secret) api_tls_secret="${2:-}"; shift 2 ;;
    --public-url) public_url="${2:-}"; shift 2 ;;
    --cluster-domain) cluster_domain="${2:-}"; shift 2 ;;
    --kubectl) kubectl_bin="${2:-}"; shift 2 ;;
    --curl) curl_bin="${2:-}"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

for required in kubeconfig context namespace gateway_class ca_config_map api_tls_secret public_url cluster_domain; do
  if [[ -z "${!required}" ]]; then
    echo "--${required//_/-} is required" >&2
    usage
  fi
done
if [[ "${public_url}" != https://* ]]; then
  echo "--public-url must use HTTPS" >&2
  exit 2
fi

for required_command in "$kubectl_bin" "$curl_bin" openssl base64; do
  if ! command -v "$required_command" >/dev/null 2>&1; then
    echo "required command is unavailable: $required_command" >&2
    exit 2
  fi
done

kubectl() {
  "$kubectl_bin" --kubeconfig "$kubeconfig" --context "$context" "$@"
}

require_output() {
  local label="$1"
  shift
  local output
  if ! output="$("$@")"; then
    echo "${label}: query failed" >&2
    exit 1
  fi
  if [[ -z "$output" ]]; then
    echo "${label}: no matching value" >&2
    exit 1
  fi
  printf '%s' "$output"
}

condition_true() {
  local resource="$1"
  local condition_path="$2"
  local conditions
  conditions="$(require_output "$resource status" kubectl -n "$namespace" get "$resource" -o "jsonpath=${condition_path}")"
  if ! grep -Fxq 'Accepted=True' <<<"$conditions"; then
    echo "${resource}: Accepted is not True; inspect its status conditions" >&2
    exit 1
  fi
  if ! grep -Fxq 'ResolvedRefs=True' <<<"$conditions"; then
    echo "${resource}: ResolvedRefs is not True; inspect its references" >&2
    exit 1
  fi
}

served_versions="$(require_output 'BackendTLSPolicy CRD' kubectl get crd backendtlspolicies.gateway.networking.k8s.io -o 'jsonpath={range .spec.versions[?(@.served==true)]}{.name}{"\n"}{end}')"
if ! grep -Fxq v1 <<<"$served_versions"; then
  echo 'BackendTLSPolicy CRD must serve gateway.networking.k8s.io/v1' >&2
  exit 1
fi

bundle_version="$(require_output 'Gateway API bundle version' kubectl get crd backendtlspolicies.gateway.networking.k8s.io -o 'jsonpath={.metadata.annotations.gateway\.networking\.k8s\.io/bundle-version}')"
bundle_version="${bundle_version#v}"
IFS=. read -r bundle_major bundle_minor bundle_patch <<<"$bundle_version"
if [[ ! "$bundle_major" =~ ^[0-9]+$ || ! "$bundle_minor" =~ ^[0-9]+$ || ! "$bundle_patch" =~ ^[0-9]+$ ]] ||
  (( bundle_major < 1 || (bundle_major == 1 && bundle_minor < 4) )); then
  echo "Gateway API bundle ${bundle_version} is below the required 1.4.0" >&2
  exit 1
fi

gateway_features="$(require_output 'GatewayClass supported features' kubectl get gatewayclass "$gateway_class" -o 'jsonpath={range .status.supportedFeatures[*]}{.name}{"\n"}{end}')"
if ! grep -Fxq BackendTLSPolicy <<<"$gateway_features"; then
  echo "GatewayClass ${gateway_class} does not report BackendTLSPolicy support" >&2
  exit 1
fi

https_port="$(require_output 'apiserver HTTPS Service port' kubectl -n "$namespace" get service steward-apiserver -o 'jsonpath={.spec.ports[?(@.name=="https")].port}')"
if [[ "$https_port" != 443 ]]; then
  echo "steward-apiserver Service port section https must expose port 443, got ${https_port}" >&2
  exit 1
fi

ready_endpoints="$(require_output 'apiserver EndpointSlice' kubectl -n "$namespace" get endpointslices -l kubernetes.io/service-name=steward-apiserver -o 'jsonpath={range .items[*].endpoints[*]}{.conditions.ready}{"\n"}{end}')"
if ! grep -Fxq true <<<"$ready_endpoints"; then
  echo 'steward-apiserver has no ready EndpointSlice endpoint' >&2
  exit 1
fi

policy_target="$(require_output 'BackendTLSPolicy target' kubectl -n "$namespace" get backendtlspolicy steward-apiserver -o 'jsonpath={.spec.targetRefs[0].kind}{"/"}{.spec.targetRefs[0].name}{"#"}{.spec.targetRefs[0].sectionName}')"
if [[ "$policy_target" != 'Service/steward-apiserver#https' ]]; then
  echo "BackendTLSPolicy must target Service/steward-apiserver#https, got ${policy_target}" >&2
  exit 1
fi

expected_hostname="steward-apiserver.${namespace}.svc.${cluster_domain}"
policy_hostname="$(require_output 'BackendTLSPolicy hostname' kubectl -n "$namespace" get backendtlspolicy steward-apiserver -o 'jsonpath={.spec.validation.hostname}')"
if [[ "$policy_hostname" != "$expected_hostname" ]]; then
  echo "BackendTLSPolicy hostname must be ${expected_hostname}, got ${policy_hostname}" >&2
  exit 1
fi
policy_ca="$(require_output 'BackendTLSPolicy CA ConfigMap' kubectl -n "$namespace" get backendtlspolicy steward-apiserver -o 'jsonpath={.spec.validation.caCertificateRefs[0].kind}{"/"}{.spec.validation.caCertificateRefs[0].name}')"
if [[ "$policy_ca" != "ConfigMap/${ca_config_map}" ]]; then
  echo "BackendTLSPolicy must reference ConfigMap/${ca_config_map}, got ${policy_ca}" >&2
  exit 1
fi

condition_true httproute/steward-api '{range .status.parents[*].conditions[*]}{.type}{"="}{.status}{"\n"}{end}'
condition_true backendtlspolicy/steward-apiserver '{range .status.ancestors[*].conditions[*]}{.type}{"="}{.status}{"\n"}{end}'

umask 077
temporary_directory="$(mktemp -d)"
cleanup() {
  rm -rf "$temporary_directory"
}
trap cleanup EXIT INT TERM
ca_file="$temporary_directory/ca.crt"
certificate_file="$temporary_directory/apiserver.crt"

kubectl -n "$namespace" get configmap "$ca_config_map" -o 'jsonpath={.data.ca\.crt}' > "$ca_file"
if [[ ! -s "$ca_file" ]]; then
  echo "ConfigMap/${ca_config_map} has no non-empty data.ca.crt public CA" >&2
  exit 1
fi
encoded_certificate="$(kubectl -n "$namespace" get secret "$api_tls_secret" -o 'jsonpath={.data.tls\.crt}')"
if ! printf '%s' "$encoded_certificate" | base64 --decode > "$certificate_file" 2>/dev/null; then
  printf '%s' "$encoded_certificate" | base64 -D > "$certificate_file"
fi
if [[ ! -s "$certificate_file" ]]; then
  echo "Secret/${api_tls_secret} has no tls.crt certificate" >&2
  exit 1
fi
if ! openssl verify -CAfile "$ca_file" -verify_hostname "$expected_hostname" "$certificate_file" >/dev/null 2>&1; then
  echo "apiserver certificate is not CA-valid for ${expected_hostname}" >&2
  exit 1
fi

http_status="$("$curl_bin" --silent --show-error --output /dev/null --write-out '%{http_code}' "${public_url%/}/admin/api/v1/session")"
case "$http_status" in
  200|401|403) ;;
  502|503)
    echo "public session route returned Gateway ${http_status}; inspect BackendTLSPolicy CA, SNI, and backend protocol" >&2
    exit 1
    ;;
  *)
    echo "public session route returned unexpected HTTP ${http_status}" >&2
    exit 1
    ;;
esac

printf 'Gateway backend TLS validation passed for %s/%s\n' "$namespace" "$expected_hostname"
