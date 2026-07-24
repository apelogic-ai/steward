#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STEWARD_E2E_SLICE=s4 exec "${root}/scripts/s3-envelope-e2e.sh"
