#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
promotion="${root}/scripts/promote-ecr-artifact.sh"

if [[ ! -x "${promotion}" ]]; then
  echo "promotion helper is missing or not executable: ${promotion}" >&2
  exit 1
fi

test_dir="$(mktemp -d)"
trap 'rm -rf "${test_dir}"' EXIT INT TERM
stub_dir="${test_dir}/bin"
state_file="${test_dir}/state"
oras_log="${test_dir}/oras.log"
mkdir -p "${stub_dir}"

cat > "${stub_dir}/aws" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" != "ecr describe-images --repository-name example/steward --output json" ]]; then
  echo "unexpected aws invocation: $*" >&2
  exit 2
fi
if [[ -s "${TEST_ECR_STATE}" ]]; then
  printf '{"imageDetails":[{"imageTags":["%s"],"imageDigest":"%s"}]}\n' \
    "${TEST_ECR_TAG}" "$(cat "${TEST_ECR_STATE}")"
else
  printf '{"imageDetails":[]}\n'
fi
STUB

cat > "${stub_dir}/oras" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$#" -ne 3 || "$1" != "cp" ]]; then
  echo "unexpected oras invocation: $*" >&2
  exit 2
fi
printf '%s\n' "$*" >> "${TEST_ORAS_LOG}"
printf '%s\n' "${2##*@}" > "${TEST_ECR_STATE}"
STUB
chmod +x "${stub_dir}/aws" "${stub_dir}/oras"

expected="sha256:1111111111111111111111111111111111111111111111111111111111111111"
different="sha256:2222222222222222222222222222222222222222222222222222222222222222"
source_ref="ghcr.io/example/steward@${expected}"
common_env=(
  "PATH=${stub_dir}:${PATH}"
  "TEST_ECR_STATE=${state_file}"
  "TEST_ECR_TAG=1.2.3-apiserver"
  "TEST_ORAS_LOG=${oras_log}"
)

echo "missing target"
: > "${state_file}"
: > "${oras_log}"
env "${common_env[@]}" "${promotion}" \
  "${source_ref}" registry.example.test example/steward 1.2.3-apiserver "${expected}"
test "$(cat "${state_file}")" = "${expected}"
grep -q '^cp ' "${oras_log}"

echo "matching target"
printf '%s\n' "${expected}" > "${state_file}"
: > "${oras_log}"
env "${common_env[@]}" "${promotion}" \
  "${source_ref}" registry.example.test example/steward 1.2.3-apiserver "${expected}"
test ! -s "${oras_log}"

echo "different digest"
printf '%s\n' "${different}" > "${state_file}"
: > "${oras_log}"
if env "${common_env[@]}" "${promotion}" \
  "${source_ref}" registry.example.test example/steward 1.2.3-apiserver "${expected}"
then
  echo "promotion accepted an immutable tag with a different digest" >&2
  exit 1
fi
test "$(cat "${state_file}")" = "${different}"
test ! -s "${oras_log}"
