#!/usr/bin/env bash
set -euo pipefail

chart_metadata="${1:?Chart.yaml path is required}"
if [[ ! -f "${chart_metadata}" ]]; then
  echo "chart metadata is missing: ${chart_metadata}" >&2
  exit 1
fi

version_lines="$(awk '$1 == "version:" { print $2 }' "${chart_metadata}")"
marker_lines="$(awk '$1 == "steward.apelogic.ai/customer-install-contract:" { print $2 }' "${chart_metadata}")"
if [[ -z "${version_lines}" || "${version_lines}" == *$'\n'* || "${marker_lines}" == *$'\n'* ]]; then
  echo 'chart version or customer-install contract marker is missing or ambiguous' >&2
  exit 1
fi

if [[ -z "${marker_lines}" && "${version_lines}" == 0.1.17 ]]; then
  printf '%s\n' legacy
  exit 0
fi

if [[ "${marker_lines}" == steward.customer-install/v1 ]]; then
  if [[ "${version_lines}" == 0.2.0 ]] \
    || [[ "${version_lines}" =~ ^0\.1\.([0-9]+)$ && ${BASH_REMATCH[1]} -ge 18 ]]; then
    printf '%s\n' customer-v1
    exit 0
  fi
fi

echo "unsupported chart version/customer-install contract: ${version_lines} / ${marker_lines:-<none>}" >&2
exit 1
