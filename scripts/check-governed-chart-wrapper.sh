#!/usr/bin/env bash
set -euo pipefail

if (($# != 1)); then
  echo "usage: $0 PREPARED_CHART" >&2
  exit 2
fi

chart="$1"
chart_name="$(sed -nE 's/^name:[[:space:]]*([^[:space:]]+).*$/\1/p' \
  "${chart}/Chart.yaml")"
if [[ -z "${chart_name}" ]]; then
  echo "chart name is missing: ${chart}/Chart.yaml" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'find "${work}" -depth -delete 2>/dev/null || true' EXIT INT TERM
mkdir -p "${work}/dependencies"

for archive in "${chart}"/charts/*.tgz; do
  [[ -e "${archive}" ]] || continue
  tar -xzf "${archive}" -C "${work}/dependencies"
done

template_definitions() {
  local directory="$1"
  [[ -d "${directory}" ]] || return 0
  find "${directory}" -type f \
    \( -name '*.tpl' -o -name '*.yaml' -o -name '*.yml' \) \
    -exec sed -nE \
    's/.*\{\{[- ]*define[[:space:]]+"([^"]+)".*/\1/p' {} + \
    | sort -u
}

template_definitions "${chart}/templates" >"${work}/wrapper-definitions"
{
  template_definitions "${chart}/.upstream"
  template_definitions "${work}/dependencies"
} | sort -u >"${work}/upstream-definitions"
while IFS= read -r definition; do
  [[ -n "${definition}" ]] || continue
  if grep -Fxq -- "${definition}" "${work}/upstream-definitions"; then
    echo "${chart_name} wrapper redefines an upstream template name" >&2
    exit 1
  fi
done <"${work}/wrapper-definitions"

rendered="${work}/rendered.yaml"
helm template wrapper-check "${chart}" --namespace steward-test >"${rendered}"
case "${chart_name}" in
  openshell)
    helm template wrapper-check "${chart}" --namespace steward-test \
      --set route.enabled=true \
      --set-string 'route.parentRefs[0].name=shared-gateway' \
      --set-string route.hostname=openshell.example.test >>"${rendered}"
    ;;
  litellm)
    helm template wrapper-check "${chart}" --namespace steward-test \
      --set route.enabled=true \
      --set-string 'route.parentRefs[0].name=shared-gateway' \
      --set-string route.hostname=litellm.example.test \
      --set-string route.timeouts.request=30s \
      --set-string route.timeouts.backendRequest=20s >>"${rendered}"
    ;;
esac

awk -v prefix="${chart_name}/templates/" '
  /^---$/ { source = ""; next }
  /^# Source: / { source = substr($0, 11); next }
  /^kind:[[:space:]]+/ && index(source, prefix) == 1 {
    kind = $2
    if (kind != "ConfigMap" && kind != "GRPCRoute" && kind != "HTTPRoute") {
      printf "%s wrapper rendered forbidden kind %s from %s\n", \
        prefix, kind, source > "/dev/stderr"
      rejected = 1
    }
  }
  END { exit rejected }
' "${rendered}"
