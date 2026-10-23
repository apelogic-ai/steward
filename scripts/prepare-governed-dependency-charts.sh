#!/usr/bin/env bash
set -euo pipefail

if (($# != 1)); then
  echo "usage: $0 OUTPUT_DIRECTORY" >&2
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output="$1"
work="$(mktemp -d)"
trap 'find "${work}" -depth -delete 2>/dev/null || true' EXIT INT TERM

if [[ -e "${output}" ]]; then
  echo "output directory already exists: ${output}" >&2
  exit 2
fi

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

verify_hash() {
  local path="$1" expected="$2" actual
  actual="$(hash_file "${path}")"
  if [[ "${actual}" != "${expected}" ]]; then
    echo "checksum mismatch for ${path}: expected ${expected}, got ${actual}" >&2
    exit 1
  fi
}

for tool in awk curl helm python3 tar; do
  command -v "${tool}" >/dev/null || {
    echo "missing required chart preparation tool: ${tool}" >&2
    exit 2
  }
done

mkdir -p "${output}"
for chart in spire agent-sandbox openshell litellm arc; do
  cp -R "${root}/charts/${chart}" "${output}/${chart}"
  cp "${root}/LICENSE" "${output}/${chart}/LICENSE"
  cp "${root}/THIRD_PARTY_NOTICES.md" "${output}/${chart}/THIRD_PARTY_NOTICES.md"
done

agent_manifest="${work}/agent-sandbox-v0.5.0.yaml"
curl -fsSL \
  https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v0.5.0/manifest.yaml \
  -o "${agent_manifest}"
verify_hash \
  "${agent_manifest}" \
  ccfe3649de8b33f0ee0ec635e2c7b40e1c468ad9fde6aac7c7379501327d9c4c
python3 "${root}/scripts/prepare-agent-sandbox-chart.py" \
  "${agent_manifest}" "${output}/agent-sandbox"

litellm_archive="${work}/litellm-v1.106.2.tar.gz"
curl -fsSL \
  https://github.com/BerriAI/litellm/archive/refs/tags/v1.106.2.tar.gz \
  -o "${litellm_archive}"
verify_hash \
  "${litellm_archive}" \
  c91c0ab3c176c6a60b60faf3097ea1f14a459112a0f49cd7bee31083539b6b1d
mkdir -p "${work}/litellm-source" "${output}/litellm/.upstream"
tar -xzf "${litellm_archive}" -C "${work}/litellm-source"
cp -R \
  "${work}/litellm-source/litellm-1.106.2/helm/litellm-helm" \
  "${output}/litellm/.upstream/litellm-helm"

export HELM_CACHE_HOME="${work}/helm/cache"
export HELM_CONFIG_HOME="${work}/helm/config"
export HELM_DATA_HOME="${work}/helm/data"
helm dependency update "${output}/spire" >/dev/null
verify_hash \
  "${output}/spire/charts/spire-0.29.0.tgz" \
  b73205c87c5edfdaa5dcb6d6de16685950e02a5b8f843cdc6e101d127ef7a7be
verify_hash \
  "${output}/spire/charts/spire-crds-0.5.0.tgz" \
  5979c6607bc7b62c798a9cd28cf3b1861db8439cd6f069384abfc62f89bf6bca

openshell_update="$(helm dependency update "${output}/openshell" 2>&1)"
grep -Fq \
  'Digest: sha256:891b09f7d60e9341af2bb545b3d1a4a6c39870f6f48a77d8bddca3f443c6a9e6' \
  <<<"${openshell_update}" || {
    echo 'OpenShell chart digest did not match the pinned artifact' >&2
    exit 1
  }

helm dependency update "${output}/litellm" >/dev/null

arc_update="$(helm dependency update "${output}/arc" 2>&1)"
grep -Fq \
  'Digest: sha256:20a369215b8c2ed981c4c0c75e56e6009564d51fffcbbd314d5da0ef82de253a' \
  <<<"${arc_update}" || {
    echo 'Actions Runner Controller chart digest did not match the pinned artifact' >&2
    exit 1
  }

echo "prepared governed dependency charts in ${output}"
