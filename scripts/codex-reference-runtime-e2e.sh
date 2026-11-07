#!/usr/bin/env bash
set -euo pipefail

release="${STEWARD_OPEN_SHELL_RELEASE:?STEWARD_OPEN_SHELL_RELEASE is required}"
image="${STEWARD_CODEX_REFERENCE_IMAGE:?STEWARD_CODEX_REFERENCE_IMAGE is required}"
workspace="codex-runtime"
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

policy="${STEWARD_RUN_DIR}/codex-reference-runtime-policy.yaml"
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
  # OpenShell can race the initial command against sandbox readiness. The
  # bounded readiness probe below distinguishes that race from launch failure.
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
    echo "Codex reference runtime did not become ready in OpenShell" >&2
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

sandbox_exec /bin/sh -c "
set -eu
test -x /usr/local/bin/mcp-gw-sandbox-helper
grep -F -- '--operation kubectl' /usr/local/bin/kubectl >/dev/null
grep -F -- '--operation remote-shell' /usr/local/bin/remote-shell >/dev/null
test -z \"\${MCP_GW_RUN_TOKEN+x}\"
case \"\$(env)\" in
  *openshell-token-grant-placeholder*) exit 1 ;;
esac

# Both helpers must fail closed with exit code 125 when a usable edge grant is
# unavailable. The fixed wrappers add the substitution marker only to the
# helper child process, so it is absent from the agent environment and output.
check_helper_refusal() {
  expected=\$1
  shift
  set +e
  output=\$(env \
    MCP_GW_CLI_URL=http://127.0.0.1:1/cli/run \
    MCP_GW_SHELL_URL=http://127.0.0.1:1/shell/run \
    \"\$@\" 2>&1)
  status=\$?
  set -e
  test \"\$status\" -eq 125
  last_line=\$(printf '%s\\n' \"\$output\" | tail -n 1)
  test \"\$last_line\" = \"\$expected\"
  case \"\$output\" in
    *openshell-token-grant-placeholder*) exit 1 ;;
  esac
}
check_helper_refusal \"Remote shell request failed\" \
  remote-shell machine.test -- uname -a

rm -f /tmp/steward-cli-refusal-ready /tmp/steward-cli-refusal-reason
node <<'NODE' &
const fs = require('node:fs');
const http = require('node:http');
const server = http.createServer((request, response) => {
  let body = '';
  request.setEncoding('utf8');
  request.on('data', (chunk) => { body += chunk; });
  request.on('end', () => {
    let parsed;
    try {
      parsed = JSON.parse(body);
    } catch {
      parsed = {};
    }
    const refused = request.method === 'POST'
      && request.url === '/cli/run'
      && request.headers.authorization === 'Bearer openshell-token-grant-placeholder'
      && parsed.cli === 'kubectl'
      && Array.isArray(parsed.args)
      && parsed.args[0] === 'delete';
    if (!refused) {
      response.writeHead(500);
      response.end();
      return;
    }
    const reason = 'This command is not approved for this run.';
    fs.writeFileSync('/tmp/steward-cli-refusal-reason', reason);
    response.writeHead(403, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: 'task_not_authorized', message: reason }));
  });
});
server.listen(18080, '127.0.0.1', () => {
  fs.writeFileSync('/tmp/steward-cli-refusal-ready', 'ready');
});
NODE
server_pid=\$!
trap 'kill \"\${server_pid}\" 2>/dev/null || true' EXIT INT TERM
for attempt in \$(seq 1 100); do
  test -f /tmp/steward-cli-refusal-ready && break
  sleep 0.01
done
test -f /tmp/steward-cli-refusal-ready
set +e
output=\$(MCP_GW_CLI_URL=http://127.0.0.1:18080/cli/run kubectl delete pods 2>&1)
status=\$?
set -e
kill \"\${server_pid}\"
wait \"\${server_pid}\" 2>/dev/null || true
trap - EXIT INT TERM
test \"\${status}\" -eq 125
last_line=\$(printf '%s\\n' \"\${output}\" | tail -n 1)
test \"\${last_line}\" = \"CLI request failed\"
test \"\$(cat /tmp/steward-cli-refusal-reason)\" = \"This command is not approved for this run.\"
case \"\${output}\" in
  *openshell-token-grant-placeholder*) exit 1 ;;
esac
"
