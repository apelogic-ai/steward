#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENVELOPE="${ROOT}/config/task/steward-run-service-envelope.example.json"

for variable in \
  STEWARD_APISERVER_URL \
  STEWARD_APISERVER_CA_CERTIFICATE_FILE \
  STEWARD_SERVICE_ENVELOPE_BOOTSTRAP_TOKEN_FILE
do
  if [[ -z "${!variable:-}" ]]; then
    echo "${variable} is required" >&2
    exit 2
  fi
done

if [[ ! "${STEWARD_APISERVER_URL}" =~ ^https://[^/]+(/.*)?$ ]]; then
  echo "STEWARD_APISERVER_URL must use HTTPS" >&2
  exit 2
fi
for file in \
  "${STEWARD_APISERVER_CA_CERTIFICATE_FILE}" \
  "${STEWARD_SERVICE_ENVELOPE_BOOTSTRAP_TOKEN_FILE}" \
  "${ENVELOPE}"
do
  if [[ ! -s "${file}" ]]; then
    echo "required bootstrap input is missing or empty: ${file}" >&2
    exit 2
  fi
done

token="$(<"${STEWARD_SERVICE_ENVELOPE_BOOTSTRAP_TOKEN_FILE}")"
if [[ -z "${token}" || "${token}" == *$'\n'* || "${token}" == *$'\r'* || "${token}" == *'"'* ]]; then
  echo "STEWARD_SERVICE_ENVELOPE_BOOTSTRAP_TOKEN_FILE must contain one non-empty bearer token" >&2
  exit 2
fi

auth_config="$(mktemp "${TMPDIR:-/tmp}/steward-copy-smoke-auth.XXXXXX")"
response_body="$(mktemp "${TMPDIR:-/tmp}/steward-copy-smoke-response.XXXXXX")"
cleanup() {
  status="$?"
  trap - EXIT INT TERM
  find "${auth_config}" "${response_body}" -type f -delete 2>/dev/null || true
  exit "${status}"
}
trap cleanup EXIT INT TERM
chmod 600 "${auth_config}" "${response_body}"
printf 'header = "Authorization: Bearer %s"\n' "${token}" >"${auth_config}"
unset token

status="$({
  curl \
    --silent \
    --show-error \
    --config "${auth_config}" \
    --cacert "${STEWARD_APISERVER_CA_CERTIFICATE_FILE}" \
    --output "${response_body}" \
    --write-out '%{http_code}' \
    --request POST \
    --header 'Content-Type: application/json' \
    --data-binary "@${ENVELOPE}" \
    "${STEWARD_APISERVER_URL%/}/admin/service-envelopes/steward-run"
} 2>&1)" || {
  echo "Steward copy-smoke envelope bootstrap request failed" >&2
  exit 1
}

case "${status}" in
  200|201)
    echo "steward-run copy-smoke service envelope is ready"
    ;;
  *)
    echo "Steward rejected the copy-smoke service envelope with HTTP ${status}" >&2
    exit 1
    ;;
esac
