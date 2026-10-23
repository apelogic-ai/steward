#!/usr/bin/env bash
set -euo pipefail

release="${STEWARD_OPEN_SHELL_RELEASE:?STEWARD_OPEN_SHELL_RELEASE is required}"
image="${STEWARD_CLAUDE_CODE_REFERENCE_IMAGE:?STEWARD_CLAUDE_CODE_REFERENCE_IMAGE is required}"
workspace="claude-runtime"
sandbox="tool-check"
cli="${STEWARD_RUN_DIR:?STEWARD_RUN_DIR is required}/openshell"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  if [[ -x "${cli}" ]]; then
    "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
      --workspace "${workspace}" sandbox delete "${sandbox}" >/dev/null 2>&1 || true
    "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
      workspace delete "${workspace}" >/dev/null 2>&1 || true
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
release_url="https://github.com/NVIDIA/OpenShell/releases/download/${release}"
curl --fail --location --silent --show-error \
  "${release_url}/${archive}" \
  --output "${STEWARD_RUN_DIR}/${archive}"
curl --fail --location --silent --show-error \
  "${release_url}/openshell-checksums-sha256.txt" \
  --output "${STEWARD_RUN_DIR}/openshell-checksums-sha256.txt"
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

policy="${STEWARD_RUN_DIR}/claude-code-reference-runtime-policy.yaml"
printf '%s\n' \
  'version: 1' \
  'filesystem_policy:' \
  '  include_workdir: true' \
  '  read_only: [/usr, /lib, /proc, /dev/urandom, /app, /etc, /var/log]' \
  '  read_write: [/sandbox, /tmp, /dev/null]' \
  'landlock:' \
  '  compatibility: best_effort' \
  'process:' \
  '  run_as_user: sandbox' \
  '  run_as_group: sandbox' \
  >"${policy}"

"${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  workspace create --name "${workspace}"
if ! "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
  --workspace "${workspace}" sandbox create \
  --name "${sandbox}" \
  --from "${image}" \
  --policy "${policy}" \
  --no-tty \
  -- true
then
  :
fi

for attempt in {1..120}; do
  if "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    --workspace "${workspace}" sandbox exec --name "${sandbox}" --no-tty -- \
    true >/dev/null 2>&1
  then
    break
  fi
  if [[ "${attempt}" == 120 ]]; then
    echo "Claude Code reference runtime did not become ready in OpenShell" >&2
    exit 1
  fi
  sleep 1
done

sandbox_exec() {
  "${cli}" --gateway-endpoint "${STEWARD_OPENSHELL_ENDPOINT}" \
    --workspace "${workspace}" sandbox exec --name "${sandbox}" --no-tty -- "$@"
}

sandbox_exec rg --version
sandbox_exec jq --version
sandbox_exec fd --version
sandbox_exec tree --version
sandbox_exec /usr/bin/claude --version

sandbox_exec /bin/sh -c "
set -eu
test \"\$(id -u)\" -ne 0
/usr/local/libexec/steward/claude-code-image-state-check \"\$HOME\" /etc/claude-code/managed-settings.json
test -x /usr/local/bin/claude
test \"\$(/usr/bin/claude --version)\" = \"2.1.222 (Claude Code)\"

test -z \"\${ANTHROPIC_API_KEY+x}\"
test -z \"\${ANTHROPIC_AUTH_TOKEN+x}\"
test -z \"\${CLAUDE_CODE_OAUTH_TOKEN+x}\"
test -z \"\${CLAUDE_CODE_OAUTH_REFRESH_TOKEN+x}\"

check_refusal() {
  name=\$1
  shift
  set +e
  output=\$(env -i HOME=\"\$HOME\" PATH=/usr/local/bin:/usr/bin:/bin \"\$@\" /usr/local/bin/claude --version 2>&1)
  status=\$?
  set -e
  test \"\$status\" -eq 125
  test \"\$output\" = \"Claude Code configuration failed\"
  case \"\$output\" in
    *openshell-token-grant-placeholder*) echo \"\$name exposed the placeholder\" >&2; exit 1 ;;
  esac
}

check_refusal missing-base-url \
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
  DISABLE_AUTOUPDATER=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
check_refusal missing-placeholder \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  DISABLE_AUTOUPDATER=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
check_refusal wrong-placeholder \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  ANTHROPIC_API_KEY=not-the-edge-placeholder \
  DISABLE_AUTOUPDATER=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
check_refusal updates-enabled \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
  DISABLE_AUTOUPDATER=0 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
check_refusal nonessential-traffic-enabled \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
  DISABLE_AUTOUPDATER=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=0
check_refusal alternate-auth \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
  DISABLE_AUTOUPDATER=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
  ANTHROPIC_AUTH_TOKEN=untrusted
check_refusal legacy-update-flag \
  ANTHROPIC_BASE_URL=https://inference.example.test \
  ANTHROPIC_API_KEY=openshell-token-grant-placeholder \
  DISABLE_UPDATES=1 \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
"
