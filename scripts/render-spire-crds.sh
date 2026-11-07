#!/usr/bin/env bash
set -euo pipefail

if (($# != 2)); then
  echo "usage: $0 UPSTREAM_CHART OUTPUT_DIRECTORY" >&2
  exit 2
fi

upstream_chart="$1"
output="$2"
mkdir -p "${output}"

for crd in \
  spire.spiffe.io_clusterfederatedtrustdomains.yaml \
  spire.spiffe.io_clusterspiffeids.yaml \
  spire.spiffe.io_clusterstaticentries.yaml; do
  helm template steward-spire-crds "${upstream_chart}" \
    --show-only "templates/${crd}" >"${output}/${crd}"
done
