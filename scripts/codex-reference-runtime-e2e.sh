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
cluster_name="${kube_context#kind-}"
if [[ "${cluster_name}" == "${kube_context}" || "${cluster_name}" != "steward-${run_id}" ]]; then
  echo "refusing non-run-owned kube context: ${kube_context}" >&2
  exit 1
fi
KUBECTL=(
  kubectl
  --kubeconfig "${kubeconfig}"
  --context "${kube_context}"
)

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

refusal_server="${STEWARD_RUN_DIR}/codex-refusal-server.py"
refusal_certificate="${STEWARD_RUN_DIR}/codex-refusal.crt"
refusal_private_key="${STEWARD_RUN_DIR}/codex-refusal.key"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=codex-refusal.openshell.svc.cluster.local' \
  -addext 'subjectAltName=DNS:codex-refusal.openshell.svc.cluster.local' \
  -keyout "${refusal_private_key}" \
  -out "${refusal_certificate}" \
  >/dev/null 2>&1
cat >"${refusal_server}" <<'PYTHON'
import json
import ssl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


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
server.serve_forever()
PYTHON

"${KUBECTL[@]}" -n openshell create secret tls codex-refusal-tls \
  --cert="${refusal_certificate}" \
  --key="${refusal_private_key}" \
  --dry-run=client \
  -o yaml |
  "${KUBECTL[@]}" label --local -f - \
    "steward.test/run-id=${run_id}" \
    -o yaml |
  "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" -n openshell create configmap codex-refusal \
  --from-file=server.py="${refusal_server}" \
  --dry-run=client \
  -o yaml |
  "${KUBECTL[@]}" label --local -f - \
    "steward.test/run-id=${run_id}" \
    -o yaml |
  "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" apply -f - <<YAML
apiVersion: apps/v1
kind: Deployment
metadata:
  name: codex-refusal
  namespace: openshell
  labels:
    steward.test/run-id: ${run_id}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: codex-refusal
  template:
    metadata:
      labels:
        app: codex-refusal
        steward.test/run-id: ${run_id}
    spec:
      containers:
        - name: server
          image: python:3.13.5-alpine3.22@sha256:37b14db89f587f9eaa890e4a442a3fe55db452b69cca1403cc730bd0fbdc8aaf
          args: ["python3", "/srv/server.py"]
          env:
            - { name: PYTHONDONTWRITEBYTECODE, value: "1" }
          ports:
            - { name: https, containerPort: 8443 }
          readinessProbe:
            tcpSocket: { port: https }
          securityContext:
            allowPrivilegeEscalation: false
            capabilities: { drop: ["ALL"] }
            readOnlyRootFilesystem: true
            runAsNonRoot: true
            runAsUser: 65534
          volumeMounts:
            - { name: server, mountPath: /srv, readOnly: true }
            - { name: tls, mountPath: /tls, readOnly: true }
      volumes:
        - name: server
          configMap: { name: codex-refusal }
        - name: tls
          secret: { secretName: codex-refusal-tls }
---
apiVersion: v1
kind: Service
metadata:
  name: codex-refusal
  namespace: openshell
  labels:
    steward.test/run-id: ${run_id}
spec:
  selector:
    app: codex-refusal
  ports:
    - { name: https, port: 8443, targetPort: https }
YAML
"${KUBECTL[@]}" -n openshell rollout status deployment/codex-refusal --timeout=300s

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
      - host: codex-refusal.openshell.svc.cluster.local
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

refusal_certificate_base64="$(openssl base64 -A -in "${refusal_certificate}")"
sandbox_exec /bin/sh -c \
  "printf '%s' '${refusal_certificate_base64}' | base64 -d > /sandbox/codex-refusal-ca.crt"

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

# Both helpers must fail closed with exit code 125 when the governed route
# refuses a request. The fixed wrappers add the substitution marker only to the
# helper child process, so it is absent from the agent environment and output.
check_helper_refusal() {
  expected=\$1
  shift
  set +e
  output=\$(env \
    NODE_EXTRA_CA_CERTS=/sandbox/codex-refusal-ca.crt \
    MCP_GW_CLI_URL=https://codex-refusal.openshell.svc.cluster.local:8443/cli/run \
    MCP_GW_SHELL_URL=https://codex-refusal.openshell.svc.cluster.local:8443/shell/run \
    \"\$@\" 2>&1)
  status=\$?
  set -e
  if test \"\$status\" -ne 125; then
    echo \"remote shell helper did not return exit code 125\" >&2
    exit 1
  fi
  if ! printf '%s\\n' \"\$output\" | grep -Fx \"\$expected\" >/dev/null; then
    echo \"remote shell helper did not return its plain refusal\" >&2
    exit 1
  fi
  case \"\$output\" in
    *openshell-token-grant-placeholder*)
      echo \"remote shell helper exposed its substitution marker\" >&2
      exit 1
      ;;
  esac
}
check_helper_refusal \"Remote shell request failed\" \
  remote-shell machine.test -- uname -a

set +e
output=\$(env \
  NODE_EXTRA_CA_CERTS=/sandbox/codex-refusal-ca.crt \
  MCP_GW_CLI_URL=https://codex-refusal.openshell.svc.cluster.local:8443/cli/run \
  kubectl delete pods 2>&1)
status=\$?
set -e
if test \"\${status}\" -ne 125; then
  echo \"CLI helper did not return exit code 125\" >&2
  exit 1
fi
if ! printf '%s\\n' \"\${output}\" | grep -Fx \"CLI request failed\" >/dev/null; then
  echo \"CLI helper did not return its plain refusal\" >&2
  exit 1
fi
case \"\${output}\" in
  *openshell-token-grant-placeholder*)
    echo \"CLI helper exposed its substitution marker\" >&2
    exit 1
    ;;
esac
"

refusal_logs="$("${KUBECTL[@]}" -n openshell logs deployment/codex-refusal --container server)"
for expected in \
  'REFUSED kubectl delete: This command is not approved for this run.' \
  'REFUSED remote shell: This command is not approved for this run.'
do
  grep -Fx "${expected}" <<<"${refusal_logs}" >/dev/null
done
