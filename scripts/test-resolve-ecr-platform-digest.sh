#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
resolver="${root}/scripts/resolve-ecr-platform-digest.sh"

if [[ ! -x "${resolver}" ]]; then
  echo "ECR platform resolver is missing or not executable: ${resolver}" >&2
  exit 1
fi

test_dir="$(mktemp -d)"
trap 'rm -rf "${test_dir}"' EXIT INT TERM
stub_dir="${test_dir}/bin"
mkdir -p "${stub_dir}"

cat > "${stub_dir}/oras" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$#" -ne 3 || "$1" != "manifest" || "$2" != "fetch" ]]; then
  echo "unexpected oras invocation: $*" >&2
  exit 2
fi
case "$3" in
  */one@*) fixture="oci-index-one-amd64.json" ;;
  */missing@*) fixture="oci-index-missing-amd64.json" ;;
  */ambiguous@*) fixture="oci-index-ambiguous-amd64.json" ;;
  *)
    echo "unexpected index reference: $3" >&2
    exit 2
    ;;
esac
cat "${TEST_FIXTURE_DIR}/${fixture}"
STUB
chmod +x "${stub_dir}/oras"

common_env=(
  "PATH=${stub_dir}:${PATH}"
  "TEST_FIXTURE_DIR=${root}/testdata/release"
)
index_digest="sha256:7777777777777777777777777777777777777777777777777777777777777777"

echo "one runnable linux/amd64 manifest plus SBOM and provenance attestations"
resolved="$(env "${common_env[@]}" "${resolver}" \
  "registry.example.test/example/one@${index_digest}" linux amd64)"
test "${resolved}" = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

echo "missing runnable linux/amd64 manifest fails closed"
if env "${common_env[@]}" "${resolver}" \
  "registry.example.test/example/missing@${index_digest}" linux amd64 \
  > "${test_dir}/missing.out" 2> "${test_dir}/missing.err"
then
  echo "resolver accepted an index without a runnable linux/amd64 manifest" >&2
  exit 1
fi
grep -q 'expected exactly one runnable linux/amd64 manifest.*found 0' "${test_dir}/missing.err"

echo "ambiguous runnable linux/amd64 manifests fail closed"
if env "${common_env[@]}" "${resolver}" \
  "registry.example.test/example/ambiguous@${index_digest}" linux amd64 \
  > "${test_dir}/ambiguous.out" 2> "${test_dir}/ambiguous.err"
then
  echo "resolver accepted an index with ambiguous linux/amd64 manifests" >&2
  exit 1
fi
grep -q 'expected exactly one runnable linux/amd64 manifest.*found 2' "${test_dir}/ambiguous.err"
