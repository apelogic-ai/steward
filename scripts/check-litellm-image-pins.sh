#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

image_name="litellm"
image_prefix="berriai/${image_name}"
reference_pattern="${image_prefix}[^[:space:]\"',]*"
digest_pattern="^${image_prefix}[^@[:space:]]*@sha256:[0-9a-f]{64}$"
violations=0

while IFS=: read -r path line reference; do
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
