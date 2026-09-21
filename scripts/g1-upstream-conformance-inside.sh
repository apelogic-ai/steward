#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE="${STEWARD_OPEN_SHELL_RELEASE:?STEWARD_OPEN_SHELL_RELEASE is required}"
WORKSPACE="g1-egress"
SANDBOX="egress-check"
CLI="${STEWARD_RUN_DIR}/openshell"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  if [[ -x "${CLI}" ]]; then
    "${CLI}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
      --workspace "${WORKSPACE}" sandbox delete "${SANDBOX}" >/dev/null 2>&1 || true
    "${CLI}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
      workspace delete "${WORKSPACE}" >/dev/null 2>&1 || true
  fi
  exit "${status}"
}
trap 'cleanup "$?"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

case "$(uname -s):$(uname -m)" in
  Darwin:arm64) target="aarch64-apple-darwin" ;;
  Linux:arm64 | Linux:aarch64) target="aarch64-unknown-linux-musl" ;;
  Linux:x86_64 | Linux:amd64) target="x86_64-unknown-linux-musl" ;;
  *)
    echo "unsupported OpenShell CLI platform: $(uname -s) $(uname -m)" >&2
    exit 2
    ;;
esac

archive="openshell-${target}.tar.gz"
curl -fsSL \
  "https://github.com/NVIDIA/OpenShell/releases/download/${RELEASE}/${archive}" \
  -o "${STEWARD_RUN_DIR}/${archive}"
curl -fsSL \
  "https://github.com/NVIDIA/OpenShell/releases/download/${RELEASE}/openshell-checksums-sha256.txt" \
  -o "${STEWARD_RUN_DIR}/openshell-checksums-sha256.txt"
(
  cd "${STEWARD_RUN_DIR}"
  expected="$(awk -v archive="${archive}" '$2 == archive { print $1 }' openshell-checksums-sha256.txt)"
  actual="$(openssl dgst -sha256 -r "${archive}" | awk '{ print $1 }')"
  if [[ -z "${expected}" || "${actual}" != "${expected}" ]]; then
    echo "OpenShell CLI checksum mismatch" >&2
    exit 1
  fi
  tar -xzf "${archive}"
)

policy="${STEWARD_RUN_DIR}/g1-policy.yaml"
cat >"${policy}" <<'YAML'
version: 1
filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /proc, /dev/urandom, /app, /etc, /var/log]
  read_write: [/sandbox, /tmp, /dev/null]
landlock:
  compatibility: best_effort
process:
  run_as_user: sandbox
  run_as_group: sandbox
network_policies:
  github_api:
    name: github-api-readonly
    endpoints:
      - host: api.github.com
        port: 443
        protocol: rest
        enforcement: enforce
        access: read-only
    binaries:
      - path: /usr/bin/curl
YAML

"${CLI}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  workspace create --name "${WORKSPACE}"
if ! "${CLI}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  --workspace "${WORKSPACE}" sandbox create \
  --name "${SANDBOX}" \
  --from "${STEWARD_G1_BASE_IMAGE:?G-1 pinned base image is required}" \
  --policy "${policy}" \
  --no-tty \
  -- true
then
  # v0.0.90 can race its initial exec against the sandbox readiness update.
  # The bounded probe below distinguishes that race from a failed provision.
  :
fi

for attempt in {1..120}; do
  if "${CLI}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    --workspace "${WORKSPACE}" sandbox exec --name "${SANDBOX}" --no-tty -- \
    true >/dev/null 2>&1
  then
    break
  fi
  if [[ "${attempt}" == 120 ]]; then
    echo "OpenShell sandbox did not become ready for the G-1 probe" >&2
    exit 1
  fi
  sleep 1
done

"${CLI}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  --workspace "${WORKSPACE}" sandbox exec --name "${SANDBOX}" --no-tty -- \
  curl -sS --max-time 20 -H 'User-Agent: steward-conformance/1.0' https://api.github.com/zen >/dev/null

set +e
denied_connect_status="$(
  "${CLI}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    --workspace "${WORKSPACE}" sandbox exec --name "${SANDBOX}" --no-tty -- \
    curl -sS --max-time 10 --output /dev/null --write-out '%{http_connect}' https://docs.rs 2>/dev/null
)"
denied_exit="$?"
set -e
if [[ "${denied_exit}" -ne 56 || "${denied_connect_status}" != "403" ]]; then
  echo "OpenShell unlisted HTTPS probe returned CONNECT ${denied_connect_status:-none} with curl exit ${denied_exit}; expected CONNECT 403 with exit 56" >&2
  exit 1
fi
