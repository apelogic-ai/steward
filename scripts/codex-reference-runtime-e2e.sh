#!/usr/bin/env bash
set -euo pipefail

release="${STEWARD_OPEN_SHELL_RELEASE:?STEWARD_OPEN_SHELL_RELEASE is required}"
image="${STEWARD_CODEX_REFERENCE_IMAGE:?STEWARD_CODEX_REFERENCE_IMAGE is required}"
run_id="${STEWARD_RUN_ID:?STEWARD_RUN_ID is required}"
kube_context="${STEWARD_TEST_KUBE_CONTEXT:?STEWARD_TEST_KUBE_CONTEXT is required}"
kubeconfig="${STEWARD_TEST_KUBECONFIG:?STEWARD_TEST_KUBECONFIG is required}"
workspace="codex-runtime"
sandbox="tool-check"
cli="${STEWARD_RUN_DIR:?STEWARD_RUN_DIR is required}/openshell"
refusal_container="steward-codex-refusal-${run_id}"
refusal_network="steward-codex-refusal-${run_id}"
cluster_name="${kube_context#kind-}"
if [[ "${cluster_name}" == "${kube_context}" || "${cluster_name}" != "steward-${run_id}" ]]; then
  echo "refusing non-run-owned kube context: ${kube_context}" >&2
  exit 1
fi
test -f "${kubeconfig}"

cleanup() {
  status="$1"
  trap - EXIT INT TERM
  docker stop --timeout 5 "${refusal_container}" >/dev/null 2>&1 || true
  docker rm "${refusal_container}" >/dev/null 2>&1 || true
  docker network rm "${refusal_network}" >/dev/null 2>&1 || true
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
  --retry 8 --retry-all-errors --retry-delay 5 --retry-max-time 300 \
  "${release_url}/${archive}" \
  --output "${STEWARD_RUN_DIR}/${archive}"
curl --fail --location --silent --show-error \
  --retry 8 --retry-all-errors --retry-delay 5 --retry-max-time 300 \
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

refusal_server="${STEWARD_RUN_DIR}/codex-refusal-server.py"
refusal_certificate="${STEWARD_RUN_DIR}/codex-refusal.crt"
refusal_private_key="${STEWARD_RUN_DIR}/codex-refusal.key"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=codex-refusal.test' \
  -addext 'subjectAltName=DNS:codex-refusal.test' \
  -keyout "${refusal_private_key}" \
  -out "${refusal_certificate}" \
  >/dev/null 2>&1
cat >"${refusal_server}" <<'PYTHON'
import json
import ssl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        try:
            length = int(self.headers.get("content-length", "0"))
            request = json.loads(self.rfile.read(length))
        except (ValueError, json.JSONDecodeError):
            request = {}
        authorization = self.headers.get("authorization", "")
        args = request.get("args")
        cli_refused = (
            self.path == "/cli/run"
            and authorization.startswith("Bearer ")
            and request.get("cli") == "kubectl"
            and args == ["delete", "pods"]
        )
        shell_refused = (
            self.path == "/shell/run"
            and authorization.startswith("Bearer ")
            and request.get("target") == "machine.test"
            and request.get("command") == "uname -a"
        )
        if not cli_refused and not shell_refused:
            self.send_response(500)
            self.end_headers()
            return
        reason = "This command is not approved for this run."
        if cli_refused:
            print(f"REFUSED kubectl delete: {reason}", flush=True)
        else:
            print(f"REFUSED remote shell: {reason}", flush=True)
        body = json.dumps({"message": reason}).encode()
        self.send_response(403)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        pass


server = ThreadingHTTPServer(("0.0.0.0", 8443), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain("/tls/tls.crt", "/tls/tls.key")
server.socket = context.wrap_socket(server.socket, server_side=True)
Path("/tmp/ready").touch()
server.serve_forever()
PYTHON

# OpenShell reconnects inspected HTTPS with its system trust roots. Keep the
# run-generated CA and refusal fixture on a run-owned Docker network so the
# packaged helper can be tested without changing cluster trust.
docker network create \
  --label "steward.test/run-id=${run_id}" \
  "${refusal_network}" >/dev/null
docker run --detach \
  --name "${refusal_container}" \
  --network "${refusal_network}" \
  --network-alias codex-refusal.test \
  --label "steward.test/run-id=${run_id}" \
  --volume "${refusal_server}:/srv/server.py:ro" \
  --volume "${refusal_certificate}:/tls/tls.crt:ro" \
  --volume "${refusal_private_key}:/tls/tls.key:ro" \
  python:3.13.5-alpine3.22@sha256:37b14db89f587f9eaa890e4a442a3fe55db452b69cca1403cc730bd0fbdc8aaf \
  python3 /srv/server.py >/dev/null
for attempt in {1..30}; do
  if docker exec "${refusal_container}" test -f /tmp/ready; then
    break
  fi
  if [[ "${attempt}" == 30 ]]; then
    echo "governed command refusal fixture did not become ready" >&2
    exit 1
  fi
  sleep 1
done

check_helper_refusal() {
  helper="$1"
  shift
  set +e
  output="$(docker run --rm \
    --network "${refusal_network}" \
    --label "steward.test/run-id=${run_id}" \
    --volume "${refusal_certificate}:/sandbox/codex-refusal-ca.crt:ro" \
    --env NODE_EXTRA_CA_CERTS=/sandbox/codex-refusal-ca.crt \
    --env MCP_GW_CLI_URL=https://codex-refusal.test:8443/cli/run \
    --env MCP_GW_SHELL_URL=https://codex-refusal.test:8443/shell/run \
    --entrypoint "/usr/local/bin/${helper}" \
    "${image}" "$@" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -ne 125 ]]; then
    echo "${helper} did not return exit code 125" >&2
    exit 1
  fi
  case "${output}" in
    *openshell-token-grant-placeholder*)
      echo "${helper} exposed its substitution marker" >&2
      exit 1
      ;;
  esac
}
check_helper_refusal remote-shell machine.test -- uname -a
check_helper_refusal kubectl delete pods

refusal_logs="$(docker logs "${refusal_container}")"
for expected in \
  'REFUSED kubectl delete: This command is not approved for this run.' \
  'REFUSED remote shell: This command is not approved for this run.'
do
  if ! grep -Fx "${expected}" <<<"${refusal_logs}" >/dev/null; then
    echo "governed command refusal fixture did not record the exact request" >&2
    exit 1
  fi
done

policy="${STEWARD_RUN_DIR}/codex-reference-runtime-policy.yaml"
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
  governed_cli_refusal:
    name: governed-cli-refusal
    endpoints:
      - host: codex-refusal.test
        port: 8443
        protocol: rest
        enforcement: enforce
        rules:
          - allow:
              method: POST
              path: /cli/run
          - allow:
              method: POST
              path: /shell/run
    binaries:
      - path: /usr/local/bin/mcp-gw-sandbox-helper
      - path: /usr/local/bin/kubectl
      - path: /usr/local/bin/remote-shell
YAML

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
"

# The packaged helper must accept its configuration inside the governed
# sandbox, not only in a plain container (steward #77). Command routes reach
# the agent as inherited environment variables; configuration is checked before
# any network use, so an unreachable route proves acceptance by failing only
# the request.
sandbox_exec /bin/sh -c "
id
ls -l /usr/local/bin/kubectl /usr/local/bin/remote-shell /usr/local/bin/mcp-gw-sandbox-helper
command -v sha256sum
sha256sum /usr/local/bin/kubectl /usr/local/bin/remote-shell || echo wrappers-unreadable
env | cut -d= -f1 | sort | tr '\n' ' '
echo
" || true
helper_configuration_failed=0
check_sandbox_helper_configuration() {
  label="$1"
  command="$2"
  set +e
  output="$(sandbox_exec /bin/sh -c "${command}" 2>&1)"
  status=$?
  set -e
  echo "sandbox helper ${label}: exit ${status}: $(printf '%s' "${output}" | tr '\n' '|')"
  case "${output}" in
    *"Sandbox helper configuration failed"*) helper_configuration_failed=1 ;;
  esac
}
cli_route="https://codex-refusal.test:8443/cli/run"
shell_route="https://codex-refusal.test:8443/shell/run"
check_sandbox_helper_configuration "inherited kubectl" \
  "export MCP_GW_CLI_URL=${cli_route}; kubectl get pods"
check_sandbox_helper_configuration "inline kubectl" \
  "MCP_GW_CLI_URL=${cli_route} kubectl get pods"
check_sandbox_helper_configuration "inherited remote shell" \
  "export MCP_GW_SHELL_URL=${shell_route}; remote-shell machine.test -- uname -a"
check_sandbox_helper_configuration "minimal environment kubectl" \
  "env -i PATH=/usr/local/bin:/usr/bin:/bin MCP_GW_CLI_URL=http://127.0.0.1:9/cli/run kubectl get pods"
# TEMPORARY diagnostic (steward #77): run an instrumented copy of the helper's
# configuration logic with the packaged helper acting as its own Bun runtime,
# inside the same sandbox, to name the failing check.
probe_source="${BASH_SOURCE[0]%/*}/diagnostics/governed-helper-config-probe.js"
probe_encoded="$(base64 <"${probe_source}" | tr -d '\n')"
sandbox_exec /bin/sh -c "printf '%s' '${probe_encoded}' | base64 -d >/tmp/helper-config-probe.js"
check_sandbox_helper_configuration "runtime argv" \
  "BUN_BE_BUN=1 /usr/local/bin/mcp-gw-sandbox-helper -e 'console.log(JSON.stringify({argv0: process.argv0, argv: process.argv, execPath: process.execPath}))' -- a b"
check_sandbox_helper_configuration "debug inherited kubectl" \
  "export MCP_GW_CLI_URL=${cli_route}; BUN_BE_BUN=1 MCP_GW_RUN_TOKEN=openshell-token-grant-placeholder /usr/local/bin/mcp-gw-sandbox-helper /tmp/helper-config-probe.js --operation kubectl get pods"
check_sandbox_helper_configuration "debug minimal environment kubectl" \
  "env -i PATH=/usr/local/bin:/usr/bin:/bin BUN_BE_BUN=1 MCP_GW_RUN_TOKEN=x MCP_GW_CLI_URL=http://127.0.0.1:9/cli/run /usr/local/bin/mcp-gw-sandbox-helper /tmp/helper-config-probe.js --operation kubectl get pods"
check_sandbox_helper_configuration "debug inherited remote shell" \
  "export MCP_GW_SHELL_URL=${shell_route}; BUN_BE_BUN=1 MCP_GW_RUN_TOKEN=openshell-token-grant-placeholder /usr/local/bin/mcp-gw-sandbox-helper /tmp/helper-config-probe.js --operation remote-shell machine.test -- uname -a"
if [[ "${helper_configuration_failed}" == "1" ]]; then
  echo "the packaged helper rejected its configuration inside the governed sandbox" >&2
  exit 1
fi
