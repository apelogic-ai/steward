#!/usr/bin/env bash
set -euo pipefail

if (($# < 3)); then
  echo "usage: $0 OCI_CHART_REPOSITORY DIGEST DESTINATION [HELM_PULL_ARGUMENT ...]" >&2
  exit 2
fi

repository="$1"
digest="$2"
destination="$3"
shift 3

for tool in awk find helm; do
  command -v "${tool}" >/dev/null || {
    echo "missing required chart pull tool: ${tool}" >&2
    exit 2
  }
done

if [[ ! "${digest}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "invalid chart digest: ${digest}" >&2
  exit 2
fi
if [[ ! -d "${destination}" ]]; then
  echo "chart pull destination does not exist: ${destination}" >&2
  exit 2
fi
destination="$(cd "${destination}" && pwd -P)"
if [[ -n "$(find "${destination}" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
  echo "chart pull destination is not empty: ${destination}" >&2
  exit 2
fi

pull_status=0
pull_output="$(helm pull "${repository}@${digest}" --destination "${destination}" "$@" 2>&1)" \
  || pull_status="$?"
printf '%s\n' "${pull_output}" >&2
if ((pull_status != 0)); then
  exit "${pull_status}"
fi

actual_digest="$(printf '%s\n' "${pull_output}" \
  | awk '$1 == "Digest:" { value = $2 } END { print value }')"
if [[ "${actual_digest}" != "${digest}" ]]; then
  echo "chart pull returned digest ${actual_digest:-<missing>}, expected ${digest}" >&2
  exit 1
fi

archives=()
while IFS= read -r -d '' archive; do
  archives+=("${archive}")
done < <(find "${destination}" -maxdepth 1 -type f -name '*.tgz' -print0)
if ((${#archives[@]} != 1)); then
  echo "chart pull created ${#archives[@]} archives; expected exactly one" >&2
  exit 1
fi
if [[ ! -s "${archives[0]}" ]]; then
  echo "chart pull created an empty archive: ${archives[0]}" >&2
  exit 1
fi

printf '%s\n' "${archives[0]}"
