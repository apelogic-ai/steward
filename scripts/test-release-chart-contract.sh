#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -f "${fixture}/Chart.yaml"; rmdir "${fixture}"' EXIT

assert_mode() {
  local expected="$1"
  local actual
  actual="$(bash "${root}/scripts/release-chart-contract.sh" "${fixture}/Chart.yaml")"
  if [[ "${actual}" != "${expected}" ]]; then
    echo "chart contract mode ${actual} did not match ${expected}" >&2
    exit 1
  fi
}

assert_rejected() {
  if bash "${root}/scripts/release-chart-contract.sh" "${fixture}/Chart.yaml" >/dev/null 2>&1; then
    echo "unsupported chart contract unexpectedly passed" >&2
    exit 1
  fi
}

printf 'apiVersion: v2\nname: steward\nversion: 0.1.17\n' > "${fixture}/Chart.yaml"
assert_mode legacy

printf 'apiVersion: v2\nname: steward\nversion: 0.1.18\n' > "${fixture}/Chart.yaml"
assert_rejected

printf 'apiVersion: v2\nname: steward\nversion: 0.1.17\nannotations:\n  steward.apelogic.ai/customer-install-contract: steward.customer-install/v1\n' > "${fixture}/Chart.yaml"
assert_rejected

printf 'apiVersion: v2\nname: steward\nversion: 0.1.18\nannotations:\n  steward.apelogic.ai/customer-install-contract: steward.customer-install/v1\n' > "${fixture}/Chart.yaml"
assert_mode customer-v1

printf 'apiVersion: v2\nname: steward\nversion: 0.1.18\nannotations:\n  steward.apelogic.ai/customer-install-contract: unknown/v1\n' > "${fixture}/Chart.yaml"
assert_rejected

printf 'apiVersion: v2\nname: steward\nversion: 0.2.0\nannotations:\n  steward.apelogic.ai/customer-install-contract: steward.customer-install/v1\n' > "${fixture}/Chart.yaml"
assert_mode customer-v1

echo 'release chart contract transition passed'
