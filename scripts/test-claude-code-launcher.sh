#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
launcher="${ROOT}/build/claude-code-launcher.sh"
temporary_directory="$(mktemp -d)"
trap 'find "${temporary_directory}" -depth -delete' EXIT INT TERM

if [[ ! -x "${launcher}" ]]; then
  echo "Claude Code fail-closed launcher is missing or not executable" >&2
  exit 1
fi

check_refusal() {
  local name="$1"
  shift
  local output status
  set +e
  output="$(env -i \
    HOME="${temporary_directory}" \
    PATH="/usr/bin:/bin" \
    "$@" \
    "${launcher}" --version 2>&1)"
  status=$?
  set -e
  if [[ "${status}" != 125 ]]; then
    echo "${name}: expected exit 125, got ${status}" >&2
    exit 1
  fi
  if [[ "${output}" != "Claude Code configuration failed" ]]; then
    echo "${name}: launcher did not return its generic refusal" >&2
    exit 1
  fi
  if [[ "${output}" == *openshell-token-grant-placeholder* ]]; then
    echo "${name}: launcher exposed the placeholder" >&2
    exit 1
  fi
}

common=(
  ANTHROPIC_BASE_URL=https://inference.example.test
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder
  DISABLE_UPDATES=1
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
)

check_refusal missing-base-url \
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
  DISABLE_UPDATES=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
check_refusal missing-placeholder \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  DISABLE_UPDATES=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
check_refusal wrong-placeholder \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  ANTHROPIC_API_KEY=not-the-edge-placeholder \
  DISABLE_UPDATES=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
check_refusal updates-enabled \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
  DISABLE_UPDATES=0 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
check_refusal nonessential-traffic-enabled \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
  DISABLE_UPDATES=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=0

for endpoint in \
  ftp://inference.example.test \
  https://alice@inference.example.test \
  'https://inference.example.test?target=other' \
  'https://inference.example.test#other' \
  http://inference.example.test:0 \
  http://:4000 \
  https:///v1 \
  https://inference.example.test:notaport \
  https://inference.example.test:65536
do
  check_refusal malformed-base-url \
    ANTHROPIC_BASE_URL="${endpoint}" \
    ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
    DISABLE_UPDATES=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
done

for variable in \
  ANTHROPIC_AUTH_TOKEN \
  CLAUDE_CODE_OAUTH_TOKEN \
  CLAUDE_CODE_OAUTH_REFRESH_TOKEN \
  ANTHROPIC_CUSTOM_HEADERS \
  CLAUDE_CODE_USE_BEDROCK \
  CLAUDE_CODE_USE_VERTEX \
  CLAUDE_CODE_USE_FOUNDRY \
  CLAUDE_CODE_USE_MANTLE \
  CLAUDE_CODE_USE_ANTHROPIC_AWS
do
  check_refusal alternate-auth "${common[@]}" "${variable}=untrusted"
done
