#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OPEN_SHELL_RELEASE="v0.1.2"

STEWARD_OPEN_SHELL_RELEASE="${OPEN_SHELL_RELEASE}" \
bash "${ROOT}/scripts/openshell-testbed.sh" \
  cargo test \
    --manifest-path "${ROOT}/e2e/Cargo.toml" \
    --test openshell_adapter_v012 \
    adapter_round_trip_is_authenticated_with_default_runtime_and_cleanup \
    -- \
    --exact
