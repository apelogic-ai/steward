#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  echo "usage: g1-preload-kind-base-image.sh <owned-g1-cluster> <pinned-image-digest>" >&2
  exit 2
fi

cluster="$1"
image="$2"
if [[ ! "${cluster}" =~ ^steward-g1-[a-z0-9-]+$ ]]; then
  echo "G-1 preload requires an owned G-1 Kind cluster name" >&2
  exit 2
fi
if [[ ! "${image}" =~ ^ghcr\.io/nvidia/openshell-community/sandboxes/base@sha256:[a-f0-9]{64}$ ]]; then
  echo "G-1 preload requires the immutable upstream base-image digest" >&2
  exit 2
fi

node="${cluster}-control-plane"
actual_nodes="$(kind get nodes --name "${cluster}")"
if [[ "${actual_nodes}" != "${node}" ]]; then
  echo "G-1 owned Kind node mismatch: expected ${node}" >&2
  exit 1
fi

# Pull inside the run-owned node rather than the host Docker cache. The sandbox
# readiness probe begins only after CRI confirms the exact pinned reference.
docker exec "${node}" crictl pull "${image}"
docker exec "${node}" crictl inspecti --output json "${image}" |
  jq -e --arg image "${image}" \
    '.status.repoDigests | type == "array" and index($image) != null' >/dev/null || {
      echo "G-1 pinned base-image digest is absent from the owned Kind node" >&2
      exit 1
    }
