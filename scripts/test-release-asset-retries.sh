#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary="$(mktemp -d)"
trap 'find "${temporary}" -depth -delete 2>/dev/null || true' EXIT INT TERM

covered_sources=(
  '.github/actions/setup-tools/action.yml'
  'build/claude-code-reference.Dockerfile'
  'build/codex-reference.Dockerfile'
  'scripts/claude-code-reference-runtime-e2e.sh'
  'scripts/codex-reference-runtime-e2e.sh'
  'scripts/g1-upstream-conformance'
  'scripts/g1-upstream-conformance-inside.sh'
  'scripts/g2-upstream-conformance.sh'
  'scripts/governed-connections-inside.sh'
  'scripts/openshell-adapter-e2e.sh'
  'scripts/openshell-testbed.sh'
  'scripts/prepare-governed-dependency-charts.sh'
)
failures=0

extract_curls() {
  awk '
    function emit(line, count, parts, item, command) {
      count = split(line, parts, /&&|;/)
      for (item = 1; item <= count; item++) {
        command = parts[item]
        sub(/^[[:space:]]+/, "", command)
        sub(/^RUN[[:space:]]+/, "", command)
        if (command !~ /^for[[:space:]]/ &&
            command ~ /(^|[[:space:]])curl[[:space:]]/) {
          print command
        }
      }
    }
    {
      if (logical == "") {
        logical = $0
      } else {
        logical = logical " " $0
      }
      if (logical ~ /\\[[:space:]]*$/) {
        sub(/\\[[:space:]]*$/, "", logical)
        next
      }
      emit(logical)
      logical = ""
    }
    END {
      if (logical != "") {
        emit(logical)
      }
    }
  ' "$1"
}

check_value_option() {
  local command="$1" option="$2" expected="$3"
  local token next index matches=0 valid=0
  local -a tokens
  read -r -a tokens <<<"${command}"

  for ((index = 0; index < ${#tokens[@]}; index++)); do
    token="${tokens[index]}"
    next="${tokens[index + 1]:-}"
    if [[ "${token}" == "${option}" ]]; then
      matches=$((matches + 1))
      [[ "${next}" == "${expected}" ]] && valid=$((valid + 1))
    elif [[ "${token}" == "${option}="* ]]; then
      matches=$((matches + 1))
      [[ "${token}" == "${option}=${expected}" ]] && valid=$((valid + 1))
    fi
  done

  [[ "${matches}" -eq 1 && "${valid}" -eq 1 ]]
}

check_boolean_option() {
  local command="$1" option="$2" token matches=0
  local -a tokens
  read -r -a tokens <<<"${command}"

  for token in "${tokens[@]}"; do
    [[ "${token}" == "${option}" ]] && matches=$((matches + 1))
  done

  [[ "${matches}" -eq 1 ]]
}

check_surface() {
  local relative="$1" expected="$2" marker="$3"
  local commands="${temporary}/commands" command count=0
  extract_curls "${root}/${relative}" >"${commands}"

  while IFS= read -r command; do
    if [[ "${marker}" != '*' && "${command}" != *"${marker}"* ]]; then
      continue
    fi
    count=$((count + 1))
    if ! check_value_option "${command}" '--retry' '8' ||
      ! check_boolean_option "${command}" '--retry-all-errors' ||
      ! check_value_option "${command}" '--retry-delay' '5' ||
      ! check_value_option "${command}" '--retry-max-time' '300'; then
      printf '%s: curl must contain each exact retry option once: %s\n' \
        "${relative}" "${command}" >&2
      failures=$((failures + 1))
    fi
  done <"${commands}"

  if [[ "${count}" -ne "${expected}" ]]; then
    printf '%s: found %s covered curl commands; expected %s\n' \
      "${relative}" "${count}" "${expected}" >&2
    failures=$((failures + 1))
  fi
}

is_covered_source() {
  local candidate="$1" covered
  for covered in "${covered_sources[@]}"; do
    [[ "${candidate}" == "${covered}" ]] && return 0
  done
  return 1
}

while IFS= read -r -d '' candidate; do
  relative="${candidate#"${root}/"}"
  [[ "${relative}" == 'scripts/test-release-asset-retries.sh' ]] && continue
  if grep -Eq \
    'releases/download|api\.github\.com/repos/.*/tarball|github\.com/.*/archive|launchpad\.net/ubuntu/\+archive|archive\.ubuntu\.com/ubuntu/pool|dl\.k8s\.io/release|get\.helm\.sh|ziglang\.org/download|openpolicyagent\.org/downloads' \
    "${candidate}" && ! is_covered_source "${relative}"; then
    printf '%s: release-download source is missing from the retry contract inventory\n' \
      "${relative}" >&2
    failures=$((failures + 1))
  fi
done < <(find "${root}/.github" "${root}/build" "${root}/scripts" -type f -print0)

check_surface '.github/actions/setup-tools/action.yml' 14 '*'
check_surface 'build/codex-reference.Dockerfile' 8 '*'
check_surface 'build/claude-code-reference.Dockerfile' 7 '*'
check_surface 'scripts/g1-upstream-conformance-inside.sh' 2 \
  'github.com/NVIDIA/OpenShell/releases/download'
check_surface 'scripts/g2-upstream-conformance.sh' 1 \
  'github.com/apelogic-ai/mcp-gw/archive'
check_surface 'scripts/codex-reference-runtime-e2e.sh' 2 "\${release_url}"
check_surface 'scripts/claude-code-reference-runtime-e2e.sh' 2 "\${release_url}"
check_surface 'scripts/governed-connections-inside.sh' 2 \
  'github.com/NVIDIA/OpenShell/releases/download'
check_surface 'scripts/prepare-governed-dependency-charts.sh' 2 '*'

if [[ "${failures}" -ne 0 ]]; then
  echo 'release-asset retry contract failed' >&2
  exit 1
fi

echo 'release-asset retry contract passed'
