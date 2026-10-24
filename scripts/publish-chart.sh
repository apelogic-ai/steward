#!/usr/bin/env bash
set -euo pipefail

if (($# < 4)); then
  echo "usage: $0 CHART VERSION OCI_REGISTRY OUTPUT_DIRECTORY [HELM_PUSH_ARGUMENT ...]" >&2
  exit 2
fi

chart="$1"
version="$2"
registry="$3"
output_directory="$4"
shift 4

for tool in awk helm; do
  command -v "${tool}" >/dev/null || {
    echo "missing required chart publication tool: ${tool}" >&2
    exit 2
  }
done

case "${chart}" in
  steward) chart_directory=charts/steward ;;
  steward-bundle) chart_directory=charts/steward-bundle ;;
  spire) chart_directory=charts/spire ;;
  agent-sandbox) chart_directory=charts/agent-sandbox ;;
  openshell) chart_directory=charts/openshell ;;
  litellm) chart_directory=charts/litellm ;;
  arc) chart_directory=charts/arc ;;
  *)
    echo "unsupported release chart: ${chart}" >&2
    exit 2
    ;;
esac
chart_name="$(awk '$1 == "name:" { print $2; exit }' "${chart_directory}/Chart.yaml")"
if [[ "${chart_name}" != "${chart}" ]]; then
  echo "chart name ${chart_name:-<missing>} does not match ${chart}" >&2
  exit 2
fi

destination="$(cd "${output_directory}" && pwd -P)"
archive="${destination}/${chart_name}-${version}.tgz"
case "${chart}" in
  steward) helm package charts/steward --destination "${destination}" --version "${version}" >&2 ;;
  steward-bundle) helm package charts/steward-bundle --destination "${destination}" --version "${version}" >&2 ;;
  spire) helm package charts/spire --destination "${destination}" --version "${version}" >&2 ;;
  agent-sandbox) helm package charts/agent-sandbox --destination "${destination}" --version "${version}" >&2 ;;
  openshell) helm package charts/openshell --destination "${destination}" --version "${version}" >&2 ;;
  litellm) helm package charts/litellm --destination "${destination}" --version "${version}" >&2 ;;
  arc) helm package charts/arc --destination "${destination}" --version "${version}" >&2 ;;
esac
if [[ ! -s "${archive}" ]]; then
  echo "chart archive was not created at ${archive}" >&2
  exit 1
fi

push_status=0
push_output="$(helm push "${archive}" "${registry}" "$@" 2>&1)" || push_status="$?"
printf '%s\n' "${push_output}" >&2
if ((push_status != 0)); then
  exit "${push_status}"
fi

digest="$(printf '%s\n' "${push_output}" | awk '$1 == "Digest:" { value = $2 } END { print value }')"
if [[ ! "${digest}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "chart publication returned an invalid digest: ${digest:-<missing>}" >&2
  exit 1
fi

printf '%s\n' "${digest}"
