#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

image_name="litellm"
image_prefix="berriai/${image_name}"
reference_pattern="${image_prefix}[^[:space:]\"',]*"
digest_pattern="^${image_prefix}[^@[:space:]]*@sha256:[0-9a-f]{64}$"
violations=0
chart_values="charts/litellm/values.yaml"

chart_image_field() {
  local field="$1" output="$2"
  awk -v field="${field}" -v output="${output}" '
    $0 == "litellm:" { in_litellm = 1; next }
    in_litellm && /^[^[:space:]]/ { exit }
    in_litellm && $0 == "  image:" { in_image = 1; next }
    in_image && /^  [^[:space:]]/ { exit }
    in_image && $1 == field ":" {
      count += 1
      value = $2
      line = NR
    }
    END {
      if (count != 1) exit 1
      if (output == "line") print line
      else print value
    }
  ' "${chart_values}"
}

vetted_reference="$(
  sed -nE 's/^LATEST_IMAGE="([^"]+)"$/\1/p' \
    "${root}/scripts/g4-upstream-conformance"
)"
if [[ ! "${vetted_reference}" =~ ^[^@[:space:]]+:[^@[:space:]]+@sha256:[0-9a-f]{64}$ ]]; then
  echo "scripts/g4-upstream-conformance: LATEST_IMAGE is not one vetted immutable reference" >&2
  exit 1
fi
vetted_tag="${vetted_reference#*:}"
vetted_digest="${vetted_tag##*@}"

if [[ ! -f "${chart_values}" ]]; then
  echo "${chart_values}: missing LiteLLM chart values" >&2
  violations=1
  chart_repository=""
  chart_repository_line=""
  chart_tag=""
elif ! chart_repository="$(chart_image_field repository value)" ||
  ! chart_repository_line="$(chart_image_field repository line)" ||
  ! chart_tag="$(chart_image_field tag value)"; then
  echo "${chart_values}: LiteLLM image repository and tag must each appear exactly once under litellm.image" >&2
  violations=1
  chart_repository=""
  chart_repository_line=""
  chart_tag=""
else
  if [[ ! "${chart_repository}" =~ ^[^@[:space:]]+$ ]]; then
    echo "${chart_values}: LiteLLM image repository must be a repository without a tag or digest" >&2
    violations=1
  fi
  if [[ ! "${chart_tag}" =~ ^[^@[:space:]]+@sha256:[0-9a-f]{64}$ ]]; then
    echo "${chart_values}: LiteLLM image tag must include an immutable @sha256 digest" >&2
    violations=1
  elif [[ "${chart_tag##*@}" != "${vetted_digest}" ]]; then
    echo "${chart_values}: LiteLLM image digest differs from the vetted conformance pin" >&2
    violations=1
  elif [[ "${chart_tag%%@*}" != "${vetted_tag%%@*}" ]]; then
    echo "${chart_values}: LiteLLM image tag differs from the vetted conformance pin" >&2
    violations=1
  fi
fi

while IFS=: read -r path line reference; do
  if [[ "${path}" == "${chart_values}" && "${line}" == "${chart_repository_line}" ]]; then
    continue
  fi
  if [[ ! "${reference}" =~ ${digest_pattern} ]]; then
    printf '%s:%s: LiteLLM image references must include an immutable @sha256 digest: %s\n' \
      "${path}" "${line}" "${reference}" >&2
    violations=1
  fi
done < <(
  git grep -n -o -E "${reference_pattern}" -- \
    scripts \
    .github/workflows \
    ':(glob)**/*compose*.yml' \
    ':(glob)**/*compose*.yaml' \
    ':(glob)charts/**/values*.yml' \
    ':(glob)charts/**/values*.yaml' || true
)

if (( violations != 0 )); then
  exit 1
fi

echo "check-litellm-image-pins: all LiteLLM image references use immutable digests"
