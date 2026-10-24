#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
script="${root}/scripts/pull-chart-by-digest.sh"
candidate_harness="${root}/scripts/release-candidate-acceptance.sh"
release_workflow="${root}/.github/workflows/release.yml"
scratch="$(mktemp -d)"
trap 'find "${scratch}" -depth -delete 2>/dev/null || true' EXIT

digest="sha256:$(printf '%064d' 1)"
mkdir -p "${scratch}/bin"

cat >"${scratch}/bin/helm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' "$*" >>"${FAKE_HELM_CALLS}"
[[ "$1" == pull ]]
shift
reference="$1"
shift
destination=''
while (($#)); do
  case "$1" in
    --destination) destination="$2"; shift 2 ;;
    *) shift ;;
  esac
done
test -n "${destination}"

case "${FAKE_HELM_ARCHIVE_COUNT:-1}" in
  0) ;;
  1) printf x >"${destination}/${FAKE_HELM_ARCHIVE_NAME}" ;;
  2)
    printf x >"${destination}/first.tgz"
    printf x >"${destination}/second.tgz"
    ;;
  *) exit 98 ;;
esac
printf 'Pulled: %s\n' "${reference}" >&2
if [[ "${FAKE_HELM_EMIT_DIGEST:-1}" == 1 ]]; then
  printf 'Digest: %s\n' "${FAKE_HELM_DIGEST}" >&2
fi
EOF
chmod 0755 "${scratch}/bin/helm"

expect_archive() {
  local name="$1" archive_name="$2" destination="${scratch}/success-${1}"
  mkdir "${destination}"
  destination="$(cd "${destination}" && pwd -P)"
  local archive status=0
  archive="$({
    FAKE_HELM_ARCHIVE_NAME="${archive_name}" \
      FAKE_HELM_CALLS="${scratch}/${name}.calls" \
      FAKE_HELM_DIGEST="${digest}" \
      PATH="${scratch}/bin:${PATH}" \
      bash "${script}" oci://registry.example.test/charts/steward-bundle \
        "${digest}" "${destination}"
  } 2>"${scratch}/${name}.log")" || status="$?"
  if ((status != 0)); then
    cat "${scratch}/${name}.log" >&2
    echo "digest pull failed for ${archive_name}" >&2
    exit 1
  fi
  [[ "${archive}" == "${destination}/${archive_name}" ]] || {
    echo "digest pull returned ${archive}, expected ${destination}/${archive_name}" >&2
    exit 1
  }
  test -s "${archive}"
  grep -Fq "pull oci://registry.example.test/charts/steward-bundle@${digest} --destination ${destination}" \
    "${scratch}/${name}.calls"
  grep -Fq "Digest: ${digest}" "${scratch}/${name}.log"
}

expect_refusal() {
  local count="$1" destination="${scratch}/refusal-${1}" status=0
  mkdir "${destination}"
  FAKE_HELM_ARCHIVE_COUNT="${count}" \
    FAKE_HELM_ARCHIVE_NAME=unused.tgz \
    FAKE_HELM_CALLS="${scratch}/refusal-${count}.calls" \
    FAKE_HELM_DIGEST="${digest}" \
    PATH="${scratch}/bin:${PATH}" \
    bash "${script}" oci://registry.example.test/charts/steward-bundle \
      "${digest}" "${destination}" \
      >"${scratch}/refusal-${count}.stdout" \
      2>"${scratch}/refusal-${count}.stderr" || status="$?"
  if ((status == 0)); then
    echo "digest pull accepted ${count} chart archives" >&2
    exit 1
  fi
  grep -Fq "chart pull created ${count} archives; expected exactly one" \
    "${scratch}/refusal-${count}.stderr"
}

expect_digest_refusal() {
  local name="$1" actual_digest="$2" emit_digest="$3" expected="$4"
  local destination="${scratch}/digest-refusal-${name}" status=0
  mkdir "${destination}"
  FAKE_HELM_ARCHIVE_NAME=steward-bundle.tgz \
    FAKE_HELM_CALLS="${scratch}/digest-refusal-${name}.calls" \
    FAKE_HELM_DIGEST="${actual_digest}" \
    FAKE_HELM_EMIT_DIGEST="${emit_digest}" \
    PATH="${scratch}/bin:${PATH}" \
    bash "${script}" oci://registry.example.test/charts/steward-bundle \
      "${digest}" "${destination}" \
      >"${scratch}/digest-refusal-${name}.stdout" \
      2>"${scratch}/digest-refusal-${name}.stderr" || status="$?"
  if ((status == 0)); then
    echo "digest pull accepted ${name} Helm digest output" >&2
    exit 1
  fi
  grep -Fq "chart pull returned digest ${expected}, expected ${digest}" \
    "${scratch}/digest-refusal-${name}.stderr"
}

expect_nonempty_destination_refusal() {
  local destination="${scratch}/nonempty-destination" status=0
  mkdir "${destination}"
  destination="$(cd "${destination}" && pwd -P)"
  printf stale >"${destination}/existing.txt"
  FAKE_HELM_ARCHIVE_NAME=steward-bundle.tgz \
    FAKE_HELM_CALLS="${scratch}/nonempty-destination.calls" \
    FAKE_HELM_DIGEST="${digest}" \
    PATH="${scratch}/bin:${PATH}" \
    bash "${script}" oci://registry.example.test/charts/steward-bundle \
      "${digest}" "${destination}" \
      >"${scratch}/nonempty-destination.stdout" \
      2>"${scratch}/nonempty-destination.stderr" || status="$?"
  if ((status == 0)); then
    echo 'digest pull accepted a nonempty destination' >&2
    exit 1
  fi
  grep -Fq "chart pull destination is not empty: ${destination}" \
    "${scratch}/nonempty-destination.stderr"
}

expect_archive digest-name \
  "steward-bundle@sha256-${digest#sha256:}.tgz"
expect_archive version-name steward-bundle-1.2.3.tgz
expect_refusal 0
expect_refusal 2
expect_digest_refusal mismatch "sha256:$(printf '%064d' 2)" 1 \
  "sha256:$(printf '%064d' 2)"
expect_digest_refusal missing "${digest}" 0 '<missing>'
expect_nonempty_destination_refusal

for caller in "${candidate_harness}" "${release_workflow}"; do
  grep -Fq 'scripts/pull-chart-by-digest.sh' "${caller}" || {
    echo "${caller} does not use the shared digest puller" >&2
    exit 1
  }
done

echo 'chart digest pull contract passed'
