#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
script="${root}/scripts/publish-chart.sh"
candidate_harness="${root}/scripts/release-candidate-acceptance.sh"
release_workflow="${root}/.github/workflows/release.yml"
scratch="$(mktemp -d)"
trap 'find "${scratch}" -depth -delete 2>/dev/null || true' EXIT

mkdir -p "${scratch}/bin" "${scratch}/charts/steward" "${scratch}/output"
printf 'apiVersion: v2\nname: steward\nversion: 0.0.0\n' \
  >"${scratch}/charts/steward/Chart.yaml"

cat >"${scratch}/bin/helm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' "$*" >>"${FAKE_HELM_CALLS}"
case "$1" in
  package)
    destination=''
    version=''
    shift
    while (($#)); do
      case "$1" in
        --destination) destination="$2"; shift 2 ;;
        --version) version="$2"; shift 2 ;;
        *) shift ;;
      esac
    done
    test -n "${destination}"
    test -n "${version}"
    printf x >"${destination}/steward-${version}.tgz"
    ;;
  push)
    printf 'Pushed: registry.example.test/charts/steward:1.2.3\n' >&2
    printf 'Digest: %s\n' \
      "${FAKE_HELM_DIGEST:-sha256:0000000000000000000000000000000000000000000000000000000000000000}" >&2
    ;;
  *) exit 97 ;;
esac
EOF
chmod 0755 "${scratch}/bin/helm"

publish_status=0
digest="$({
  cd "${scratch}"
  FAKE_HELM_CALLS="${scratch}/helm-calls" \
    PATH="${scratch}/bin:${PATH}" \
    bash "${script}" steward 1.2.3 oci://registry.example.test/charts \
      "${scratch}/output" --plain-http
} 2>"${scratch}/publish.log")" || publish_status="$?"
if ((publish_status != 0)); then
  cat "${scratch}/publish.log" >&2
  echo "publish script failed with exit ${publish_status}" >&2
  exit 1
fi

expected_digest="sha256:$(printf '%064d' 0)"
if [[ "${digest}" != "${expected_digest}" ]]; then
  echo "publish script returned ${digest}, expected ${expected_digest}" >&2
  exit 1
fi
grep -Fq 'package ' "${scratch}/helm-calls"
grep -Eq 'push .*/steward-1\.2\.3\.tgz oci://registry\.example\.test/charts --plain-http' \
  "${scratch}/helm-calls"
grep -Fq "Digest: ${expected_digest}" "${scratch}/publish.log"

mkdir "${scratch}/invalid-output"
invalid_status=0
(
  cd "${scratch}"
  FAKE_HELM_CALLS="${scratch}/invalid-helm-calls" \
    FAKE_HELM_DIGEST=not-a-digest \
    PATH="${scratch}/bin:${PATH}" \
    bash "${script}" steward 1.2.3 oci://registry.example.test/charts \
      "${scratch}/invalid-output" --plain-http
) >"${scratch}/invalid.stdout" 2>"${scratch}/invalid.stderr" || invalid_status="$?"
if ((invalid_status == 0)); then
  echo 'publish script accepted an invalid digest' >&2
  exit 1
fi
grep -Fq 'chart publication returned an invalid digest: not-a-digest' \
  "${scratch}/invalid.stderr"

grep -Fq 'scripts/publish-chart.sh' "${candidate_harness}" || {
  echo 'release-candidate acceptance does not use the shared chart publisher' >&2
  exit 1
}
if grep -Eq '(^|[[:space:]])helm push([[:space:]]|$)' "${candidate_harness}"; then
  echo 'release-candidate acceptance still publishes a chart outside the shared script' >&2
  exit 1
fi
grep -Fq 'scripts/publish-chart.sh' "${release_workflow}" || {
  echo 'release workflow does not use the shared chart publisher' >&2
  exit 1
}
if grep -Eq '(^|[[:space:]])helm push([[:space:]]|$)' "${release_workflow}"; then
  echo 'release workflow still publishes a chart outside the shared script' >&2
  exit 1
fi

echo 'chart publish contract passed'
