#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if (($# > 1)); then
  echo "usage: $0 [BUNDLE_CHART]" >&2
  exit 2
fi
bundle_chart="${1:-}"
kind_node_image="kindest/node:v1.32.1@sha256:6afef2b7f69d627ea7bf27ee6696b6868d18e03bf98167c420df486da4662db6"
run_id="bundle-$(date -u +%Y%m%d%H%M%S)-$$"
cluster="steward-${run_id}"
context="kind-${cluster}"
temp_root="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
run_dir="$(mktemp -d "${temp_root%/}/steward-${run_id}.XXXXXX")"
kubeconfig="${run_dir}/kubeconfig"
prepared="${run_dir}/steward-bundle"
rendered="${run_dir}/rendered.yaml"
inventory="${run_dir}/ownership.txt"
cluster_created=0

cleanup() {
  local status="$?"
  trap - EXIT INT TERM
  set +e
  if [[ "${cluster_created}" == 1 ]]; then
    KUBECONFIG="${kubeconfig}" kind delete cluster --name "${cluster}" >&2 || true
  fi
  find "${run_dir}" -depth -delete 2>/dev/null || status=1
  clusters=''
  cluster_check_failed=0
  if [[ "${cluster_created}" == 1 ]] \
    && ! clusters="$(kind get clusters 2>/dev/null)"; then
    echo "steward-bundle smoke cleanup could not be verified; Kind clusters could not be listed" >&2
    if [[ "${status}" == 0 ]]; then
      status=1
    fi
    cluster_check_failed=1
  fi
  if { [[ "${cluster_created}" == 1 && "${cluster_check_failed}" == 0 ]] \
      && grep -Fxq "${cluster}" <<<"${clusters}"; } \
    || [[ -e "${run_dir}" ]]; then
    echo "steward-bundle smoke cleanup left ${cluster} or ${run_dir}" >&2
    status=1
  fi
  exit "${status}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

for tool in awk grep helm kind kubectl; do
  command -v "${tool}" >/dev/null || {
    echo "missing required steward-bundle smoke tool: ${tool}" >&2
    exit 2
  }
done

printf '%s\n' \
  "run_id=${run_id}" \
  "cluster=${cluster}" \
  "context=${context}" \
  "kubeconfig=${kubeconfig}" \
  "run_dir=${run_dir}" \
  'registry=none' \
  "container=${cluster}-control-plane" \
  'network=kind (shared, not owned)' \
  'storage=none' \
  'ports=none' \
  'namespaces=steward-test,spire-system,spire-server,agent-sandbox-system,openshell-sandboxes' \
  'unlabeled=all Helm release objects are isolated inside the run-owned cluster' \
  >"${inventory}"

cluster_created=1
kind create cluster --name "${cluster}" --kubeconfig "${kubeconfig}" \
  --image "${kind_node_image}" --wait 120s
chmod 600 "${kubeconfig}"
if [[ "$(kubectl --kubeconfig "${kubeconfig}" config current-context)" != "${context}" ]]; then
  echo 'disposable Kind cluster selected an unexpected context' >&2
  exit 1
fi

K=(kubectl --kubeconfig "${kubeconfig}" --context "${context}")
"${K[@]}" wait --for=condition=Ready nodes --all --timeout=120s >/dev/null
for namespace in steward-test spire-system spire-server agent-sandbox-system openshell-sandboxes; do
  "${K[@]}" create namespace "${namespace}" >/dev/null
  "${K[@]}" label namespace "${namespace}" \
    "steward.test/run-id=${run_id}" >/dev/null
done

if [[ -z "${bundle_chart}" ]]; then
  bash "${root}/scripts/prepare-steward-bundle-chart.sh" "${prepared}"
  bundle_chart="${prepared}"
elif [[ ! -e "${bundle_chart}" ]]; then
  echo "steward-bundle smoke chart does not exist: ${bundle_chart}" >&2
  exit 2
fi
helm template bundle "${bundle_chart}" --namespace steward-test \
  --include-crds >"${rendered}"
helm --kubeconfig "${kubeconfig}" --kube-context "${context}" \
  install bundle "${bundle_chart}" --namespace steward-test --timeout 5m

if ! helm --kubeconfig "${kubeconfig}" --kube-context "${context}" \
  status bundle --namespace steward-test | grep -Fq 'STATUS: deployed'; then
  echo 'steward-bundle Helm release did not reach deployed status' >&2
  exit 1
fi

awk '
  /^---$/ { crd = 0; metadata = 0 }
  /^kind: CustomResourceDefinition$/ { crd = 1 }
  crd && /^metadata:$/ { metadata = 1 }
  crd && metadata && /^  name:/ { print $2; crd = 0; metadata = 0 }
' "${rendered}" >"${run_dir}/crds.txt"
if [[ ! -s "${run_dir}/crds.txt" ]]; then
  echo 'steward-bundle render contained no CRDs' >&2
  exit 1
fi
while IFS= read -r crd; do
  "${K[@]}" get customresourcedefinition "${crd}" >/dev/null
done <"${run_dir}/crds.txt"

if ! "${K[@]}" get clusterspiffeids.spire.spiffe.io -o name \
  | grep -Fq 'clusterspiffeid.spire.spiffe.io/'; then
  echo 'steward-bundle installed SPIRE CRDs but no governed identity object' >&2
  exit 1
fi

"${K[@]}" get deployments,statefulsets,daemonsets,pods -A \
  -o custom-columns='KIND:.kind,NAMESPACE:.metadata.namespace,NAME:.metadata.name,READY:.status.readyReplicas' \
  --no-headers >"${run_dir}/workloads.txt"
sed -n '1,80p' "${run_dir}/workloads.txt"
printf 'steward-bundle fresh-cluster install passed: run=%s context=%s\n' \
  "${run_id}" "${context}"
