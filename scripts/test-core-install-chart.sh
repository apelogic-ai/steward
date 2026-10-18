#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rendered="$(mktemp)"
missing_namespace_error="$(mktemp)"
receiver_startup_config="$(mktemp)"
receiver_startup_output="$(mktemp)"
receiver_stub_port="$(mktemp)"
receiver_stub_kubeconfig="$(mktemp)"
receiver_stub_pid=""
receiver_process_pid=""
cleanup() {
  status="$?"
  trap - EXIT INT TERM
  if [[ -n "${receiver_process_pid}" ]]; then
    kill "${receiver_process_pid}" 2>/dev/null || true
    wait "${receiver_process_pid}" 2>/dev/null || true
  fi
  if [[ -n "${receiver_stub_pid}" ]]; then
    kill "${receiver_stub_pid}" 2>/dev/null || true
    wait "${receiver_stub_pid}" 2>/dev/null || true
  fi
  rm -f "${rendered}" "${missing_namespace_error}" "${receiver_startup_config}" \
    "${receiver_startup_output}" "${receiver_stub_port}" "${receiver_stub_kubeconfig}"
  exit "${status}"
}
trap cleanup EXIT INT TERM

customer_images=(
  --set-string images.repository=registry.example.test/customer/steward
  --set-string images.apiserver.tag=test-apiserver
  --set-string images.apiserver.digest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  --set-string images.controller.tag=test-controller
  --set-string images.controller.digest=sha256:1111111111111111111111111111111111111111111111111111111111111111
)
helm_template() {
  helm template steward "${root}/charts/steward" --namespace steward "${customer_images[@]}" "$@"
}

if helm_template \
  --set-string images.controller.digest=sha256:0000000000000000000000000000000000000000000000000000000000000000 \
  --set-string tls.webhook.caBundlePem=public-test-ca > /dev/null 2>&1; then
  echo 'core install must reject an all-zero controller image digest' >&2
  exit 1
fi

helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca > "${rendered}"
if grep -qF 'steward-trigger-receiver' "${rendered}"; then
  echo 'core install must not render the Jira Trigger receiver by default' >&2
  exit 1
fi
if grep -qF 'STEWARD_STARTER_TASK_JSON' "${rendered}"; then
  echo 'unset starter task must use the apiserver built-in without an environment override' >&2
  exit 1
fi
bash "${root}/scripts/customer-core-install-e2e.sh" \
  --lint-source-chart "${root}/charts/steward"

starter_task_json='{"taskDefinition":{"schemaVersion":"steward.task-definition/v2","name":"hello-world","version":2,"runtime":{"agentRef":"codex@0.140.0"},"promptText":"Write hello world to out/hello.txt.","outputs":[{"path":"out/hello.txt","kind":"file","required":true}]},"inputs":{"greeting":"hello"},"executionLog":"full","packagePath":".steward/tasks/hello-world/task-definition.json","title":"Hello world","description":"A deployment-owned starter task.","git":{"repository":"https://github.com/example-org/agentic-ops.git","revision":"git:ref:main","path":"catalog/hello/task-definition.json"},"publishedWorkflow":"repo-summary@2"}'
helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-json "config.apiserver.starterTask=${starter_task_json}" > "${rendered}"
grep -qF 'STEWARD_STARTER_TASK_JSON' "${rendered}"
grep -qF '.steward/tasks/hello-world/task-definition.json' "${rendered}"
grep -qF 'Write hello world to out/hello.txt' "${rendered}"

if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set-json "config.apiserver.starterTask=${starter_task_json}" \
  --set-string config.apiserver.starterTask.packagePath=catalog/hello/task-definition.json > /dev/null 2>&1; then
  echo 'starter task must reject a non-canonical repository package path' >&2
  exit 1
fi

for forbidden in \
  'kind: ClusterSPIFFEID' \
  'name: steward-mint' \
  'STEWARD_OPENSHELL_' \
  'STEWARD_WORKLOAD_EXCHANGE_' \
  'STEWARD_LITELLM_' \
  'STEWARD_TASK_INFERENCE_ENDPOINT' \
  'SPIFFE_ENDPOINT_SOCKET' \
  'secretName: steward-openshell-client' \
  'name: steward-litellm'
do
  if grep -qF "${forbidden}" "${rendered}"; then
    echo "core-only chart retained an execution prerequisite: ${forbidden}" >&2
    exit 1
  fi
done

receiver_trigger='[{"id":"Jira_read","enabled":true,"source":"jira","workflow":"triage@1","service":"event-worker","ownerUserId":"usr_11111111111111111111111111111111","envelopeDigest":"steward:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","filters":{"event":"jira:issue_updated","project":"PROJ","issueType":"10001"},"ignoredAccountIds":["acct-1"],"concurrencyLimit":2,"cooldownSeconds":60,"signingSecret":{"name":"steward-jira-webhook","key":"secret"}}]'
receiver_inputs=(
  --set-string tls.webhook.caBundlePem=public-test-ca
  --set triggerReceiver.enabled=true
  --set-string images.triggerReceiver.tag=test-trigger-receiver
  --set-string images.triggerReceiver.digest=sha256:3333333333333333333333333333333333333333333333333333333333333333
  --set-json "triggerReceiver.triggers=${receiver_trigger}"
  --set-string config.apiserver.jiraBaseUrl=https://jira.example.com
  --set-string triggerReceiver.databaseSecret.name=steward-trigger-database
  --set-string triggerReceiver.databaseSecret.key=url
  --set-string triggerReceiver.workloadExchange.endpoint=https://identity.identity-exchange.svc.cluster.local:8443/v1/workload/exchange
  --set-string triggerReceiver.workloadExchange.serverName=identity.identity-exchange.svc.cluster.local
  --set-string triggerReceiver.workloadExchange.audience=apelogic-workload-exchange
  --set-string triggerReceiver.taskApi.ca.kind=ConfigMap
  --set-string triggerReceiver.taskApi.ca.name=steward-apiserver-ca
  --set-string triggerReceiver.taskApi.ca.key=ca.crt
  --set-string networkPolicy.ingressNamespace=edge-system
  --set-string 'networkPolicy.kubeApiCidrs[0]=192.0.2.1/32'
  --set-string 'networkPolicy.postgresCidrs[0]=192.0.2.2/32'
)
helm_template "${receiver_inputs[@]}" > "${rendered}"
for required in \
  'name: steward-trigger-receiver' \
  'resourceNames: ["steward-trigger-receiver"]' \
  'resources: ["serviceaccounts/token"]' \
  'checksum/trigger-config:' \
  'STEWARD_TRIGGER_CONFIG_FILE' \
  'STEWARD_CAPABILITY_CATALOG_FILE' \
  'STEWARD_WORKLOAD_EXCHANGE_ENDPOINT' \
  'STEWARD_API_CA_CERTIFICATE_FILE' \
  'STEWARD_TRIGGER_MAX_CONCURRENCY' \
  'name: steward-trigger-database' \
  'name: steward-jira-webhook' \
  'name: steward-trigger-receiver-ingress' \
  'name: steward-trigger-receiver-egress'
do
  grep -qF "${required}" "${rendered}" || {
    echo "enabled Jira Trigger receiver omitted: ${required}" >&2
    exit 1
  }
done
grep -qF 'https://steward-apiserver.steward.svc.cluster.local:443' "${rendered}"
grep -qF 'steward-apiserver.steward.svc.cluster.local' "${rendered}"
grep -qF '/run/trigger-signing/Jira_read' "${rendered}"
apiserver_ingress="$(awk '/^metadata: \{ name: steward-apiserver-ingress \}/ { found = 1 } found { print } found && /^---$/ { exit }' "${rendered}")"
grep -qF 'app.kubernetes.io/component: trigger-receiver' <<< "${apiserver_ingress}"

receiver_config="$(
  awk '/^metadata: \{ name: steward-trigger-receiver-config/ { found = 1 } found && /^  config.json: / { sub(/^  config.json: /, ""); print; exit }' "${rendered}" |
    jq -r .
)"
inline_env_value() {
  sed -n "/name: ${1},/s/.*value: \"\([^\"]*\)\".*/\1/p" "${rendered}" | head -1
}
receiver_numeric_env_names=(
  STEWARD_TRIGGER_MAX_CONFIG_BYTES
  STEWARD_TRIGGER_MAX_BODY_BYTES
  STEWARD_TRIGGER_MAX_IDENTIFIER_BYTES
  STEWARD_TRIGGER_MAX_TRIGGERS
  STEWARD_TRIGGER_MAX_IGNORED_ACCOUNTS
  STEWARD_TRIGGER_MAX_CONCURRENCY
  STEWARD_TRIGGER_MAX_COOLDOWN_SECONDS
  STEWARD_TRIGGER_MAX_RECENT_DELIVERIES
  STEWARD_TRIGGER_MAX_ISSUE_COOLDOWNS
  STEWARD_TRIGGER_MAX_SIGNING_SECRET_BYTES
  STEWARD_TRIGGER_DATABASE_ACQUIRE_TIMEOUT_MILLISECONDS
  STEWARD_WORKLOAD_SOURCE_TOKEN_SECONDS
  STEWARD_WORKLOAD_EXCHANGE_CONNECT_TIMEOUT_MILLISECONDS
  STEWARD_WORKLOAD_EXCHANGE_REQUEST_TIMEOUT_MILLISECONDS
  STEWARD_WORKLOAD_EXCHANGE_MAX_REPLY_BYTES
  STEWARD_TRIGGER_TASK_DEADLINE_MILLISECONDS
  STEWARD_TRIGGER_POLL_INITIAL_MILLISECONDS
  STEWARD_TRIGGER_POLL_MAX_MILLISECONDS
  STEWARD_TRIGGER_API_CONNECT_TIMEOUT_MILLISECONDS
  STEWARD_TRIGGER_API_REQUEST_TIMEOUT_MILLISECONDS
  STEWARD_TRIGGER_MAX_TASK_REPLY_BYTES
)
assert_receiver_numeric_environment() {
  local settings='{}' name value
  for name in "${receiver_numeric_env_names[@]}"; do
    value="$(inline_env_value "${name}")"
    if [[ ! "${value}" =~ ^[0-9]+$ ]]; then
      echo "receiver numeric environment value must contain decimal digits only: ${name}=${value}" >&2
      return 1
    fi
    settings="$(jq -c --arg name "${name}" --arg value "${value}" \
      '. + {($name): $value}' <<< "${settings}")"
  done
  STEWARD_CHART_RUNTIME_SETTINGS_JSON="${settings}" \
    cargo test --quiet --package steward-trigger-receiver \
      --bin steward-trigger-receiver \
      tests::chart_rendered_environment_is_parseable -- --exact
}
assert_receiver_numeric_environment

assert_receiver_startup() {
  local settings='{}' name value status=0 stub_port
  local -a startup_environment=()
  for name in "${receiver_numeric_env_names[@]}"; do
    value="$(inline_env_value "${name}")"
    settings="$(jq -c --arg name "${name}" --arg value "${value}" \
      '. + {($name): $value}' <<< "${settings}")"
  done
  while IFS=$'\t' read -r name value; do
    startup_environment+=("${name}=${value}")
  done < <(jq -r 'to_entries[] | [.key, .value] | @tsv' <<< "${settings}")
  jq '.triggers |= map(.enabled = false)' <<< "${receiver_config}" > "${receiver_startup_config}"

  python3 -c '
import socket
import struct
import time

def receive_exact(connection, length):
    value = b""
    while len(value) < length:
        chunk = connection.recv(length - len(value))
        if not chunk:
            return value
        value += chunk
    return value

with socket.socket() as server:
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    print(server.getsockname()[1], flush=True)
    connection, _ = server.accept()
    with connection:
        startup_length = receive_exact(connection, 4)
        if len(startup_length) == 4:
            receive_exact(connection, struct.unpack("!I", startup_length)[0] - 4)
            connection.sendall(b"R" + struct.pack("!II", 8, 0))
            connection.sendall(b"K" + struct.pack("!III", 12, 1, 1))
            connection.sendall(b"Z" + struct.pack("!I", 5) + b"I")
        time.sleep(10)
' > "${receiver_stub_port}" &
  receiver_stub_pid="$!"
  for _ in {1..50}; do
    [[ -s "${receiver_stub_port}" ]] && break
    sleep 0.1
  done
  if [[ ! -s "${receiver_stub_port}" ]]; then
    echo 'receiver startup database stub did not publish its port' >&2
    return 1
  fi
  stub_port="$(cat "${receiver_stub_port}")"
  cat > "${receiver_stub_kubeconfig}" <<'EOF'
apiVersion: v1
kind: Config
clusters:
  - name: synthetic
    cluster:
      server: https://127.0.0.1:1
      insecure-skip-tls-verify: true
contexts:
  - name: synthetic
    context:
      cluster: synthetic
      user: synthetic
current-context: synthetic
users:
  - name: synthetic
    user:
      token: synthetic-token
EOF

  cargo build --quiet --locked --package steward-trigger-receiver --bin steward-trigger-receiver
  env -i \
    PATH="${PATH}" \
    CARGO_HOME="${CARGO_HOME:-${HOME}/.cargo}" \
    HOME="${HOME}" \
    KUBECONFIG="${receiver_stub_kubeconfig}" \
    "${startup_environment[@]}" \
    STEWARD_TRIGGER_CONFIG_FILE="${receiver_startup_config}" \
    STEWARD_CAPABILITY_CATALOG_FILE="${rendered}" \
    STEWARD_DATABASE_URL="postgresql://steward@127.0.0.1:${stub_port}/steward?sslmode=disable" \
    STEWARD_TRIGGER_NAMESPACE=steward-test \
    STEWARD_TRIGGER_SERVICE_ACCOUNT=steward-trigger-receiver \
    STEWARD_TRIGGER_BIND=127.0.0.1:0 \
    STEWARD_WORKLOAD_SOURCE_AUDIENCE=steward-workload-exchange \
    STEWARD_WORKLOAD_EXCHANGE_ENDPOINT=https://exchange.example.test:1/v1/workload/exchange \
    STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME=exchange.example.test \
    STEWARD_WORKLOAD_EXCHANGE_CA_CERTIFICATE_FILE="${rendered}" \
    STEWARD_API_ORIGIN=https://steward.example.test:1 \
    STEWARD_API_SERVER_NAME=steward.example.test \
    STEWARD_API_CA_CERTIFICATE_FILE="${rendered}" \
    "${root}/target/debug/steward-trigger-receiver" \
      > "${receiver_startup_output}" 2>&1 &
  receiver_process_pid="$!"
  local completed=0
  for _ in {1..200}; do
    if ! kill -0 "${receiver_process_pid}" 2>/dev/null; then
      completed=1
      break
    fi
    sleep 0.1
  done
  if [[ "${completed}" == 0 ]]; then
    kill "${receiver_process_pid}" 2>/dev/null || true
    wait "${receiver_process_pid}" 2>/dev/null || true
    receiver_process_pid=""
    echo 'receiver startup exceeded its deadline' >&2
    return 1
  fi
  if wait "${receiver_process_pid}"; then
    status=0
  else
    status="$?"
  fi
  receiver_process_pid=""
  kill "${receiver_stub_pid}" 2>/dev/null || true
  wait "${receiver_stub_pid}" 2>/dev/null || true
  receiver_stub_pid=""
  if [[ "${status}" == 0 ]]; then
    echo 'receiver startup unexpectedly connected to its loopback database stub' >&2
    return 1
  fi
  if grep -Fq 'Could not automatically determine the process-level CryptoProvider' \
    "${receiver_startup_output}"
  then
    echo 'receiver startup did not install the Rustls crypto provider' >&2
    cat "${receiver_startup_output}" >&2
    return 1
  fi
  if grep -Eq "panicked at|thread '[^']+' panicked" "${receiver_startup_output}"; then
    echo 'receiver panicked during startup' >&2
    cat "${receiver_startup_output}" >&2
    return 1
  fi
  grep -Eq 'certificate|PEM' \
    "${receiver_startup_output}" || {
      echo 'receiver startup did not pass the stub database into TLS client construction' >&2
      cat "${receiver_startup_output}" >&2
      return 1
    }
}
assert_receiver_startup

STEWARD_CHART_RECEIVER_CONFIG="${receiver_config}" \
STEWARD_CHART_API_ORIGIN="$(inline_env_value STEWARD_API_ORIGIN)" \
STEWARD_CHART_API_SERVER_NAME="$(inline_env_value STEWARD_API_SERVER_NAME)" \
STEWARD_CHART_EXCHANGE_ENDPOINT="$(inline_env_value STEWARD_WORKLOAD_EXCHANGE_ENDPOINT)" \
STEWARD_CHART_EXCHANGE_SERVER_NAME="$(inline_env_value STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME)" \
  cargo test --quiet --package steward-trigger-receiver \
    client::tests::chart_render_contract_from_environment -- --exact

receiver_ceiling_trigger="$(jq -c '.[0].cooldownSeconds = 31536000' <<< "${receiver_trigger}")"
receiver_ceiling_limits='{"maxConfigBytes":4194304,"maxBodyBytes":1048576,"maxIdentifierBytes":1024,"maxTriggers":4096,"maxIgnoredAccounts":4096,"maxConcurrency":1024,"maxCooldownSeconds":31536000,"maxRecentDeliveries":1000000,"maxIssueCooldowns":1000000,"maxSigningSecretBytes":65536,"databaseAcquireTimeoutMilliseconds":60000,"sourceTokenSeconds":86400,"exchangeConnectTimeoutMilliseconds":60000,"exchangeRequestTimeoutMilliseconds":300000,"exchangeMaxReplyBytes":65536,"taskDeadlineMilliseconds":86400000,"pollInitialMilliseconds":60000,"pollMaxMilliseconds":60000,"apiConnectTimeoutMilliseconds":60000,"apiRequestTimeoutMilliseconds":300000,"maxTaskReplyBytes":1048576}'
receiver_ceiling_inputs=(
  --set-json "triggerReceiver.limits=${receiver_ceiling_limits}"
  --set workspace.maxHistoryDepth=4294967295
  --set-json "triggerReceiver.triggers=${receiver_ceiling_trigger}"
)
helm_template "${receiver_inputs[@]}" "${receiver_ceiling_inputs[@]}" > "${rendered}"
assert_receiver_numeric_environment
grep -qF '\"maxHistoryDepth\":4294967295' "${rendered}" || {
  echo 'workspace history depth ceiling must render as a decimal JSON integer' >&2
  exit 1
}
ceiling_receiver_config="$(
  awk '/^metadata: \{ name: steward-trigger-receiver-config/ { found = 1 } found && /^  config.json: / { sub(/^  config.json: /, ""); print; exit }' "${rendered}" |
    jq -r .
)"
jq -e '.triggers[0].cooldownSeconds == 31536000' <<< "${ceiling_receiver_config}" > /dev/null || {
  echo 'receiver cooldown ceiling must render as a decimal JSON integer' >&2
  exit 1
}

helm_template "${receiver_inputs[@]}" \
  --set-string config.apiserver.jiraBaseUrl= > "${rendered}"
receiver_config_without_site="$(
  awk '/^metadata: \{ name: steward-trigger-receiver-config/ { found = 1 } found && /^  config.json: / { sub(/^  config.json: /, ""); print; exit }' "${rendered}" |
    jq -r .
)"
if jq -e 'has("jiraSiteUrl")' <<< "${receiver_config_without_site}" > /dev/null; then
  echo 'receiver configuration must omit the optional Jira site when it is unset' >&2
  exit 1
fi
STEWARD_CHART_RECEIVER_CONFIG="${receiver_config_without_site}" \
STEWARD_CHART_API_ORIGIN="$(inline_env_value STEWARD_API_ORIGIN)" \
STEWARD_CHART_API_SERVER_NAME="$(inline_env_value STEWARD_API_SERVER_NAME)" \
STEWARD_CHART_EXCHANGE_ENDPOINT="$(inline_env_value STEWARD_WORKLOAD_EXCHANGE_ENDPOINT)" \
STEWARD_CHART_EXCHANGE_SERVER_NAME="$(inline_env_value STEWARD_WORKLOAD_EXCHANGE_SERVER_NAME)" \
  cargo test --quiet --package steward-trigger-receiver \
    client::tests::chart_render_contract_from_environment -- --exact

for missing in \
  triggerReceiver.databaseSecret.name \
  triggerReceiver.workloadExchange.endpoint \
  triggerReceiver.workloadExchange.serverName \
  triggerReceiver.workloadExchange.audience \
  triggerReceiver.taskApi.ca.name \
  triggerReceiver.taskApi.ca.key \
  'triggerReceiver.triggers[0].signingSecret.name' \
  'triggerReceiver.triggers[0].signingSecret.key'
do
  if helm_template "${receiver_inputs[@]}" --set-string "${missing}=" > /dev/null 2>&1; then
    echo "enabled Jira Trigger receiver must reject missing ${missing}" >&2
    exit 1
  fi
done

duplicate_triggers="$(printf '%s' "${receiver_trigger}" | jq -c '. + [(.[] | .cooldownSeconds = 61)]')"
if helm_template "${receiver_inputs[@]}" \
  --set-json "triggerReceiver.triggers=${duplicate_triggers}" > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject duplicate Trigger ids' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set-string triggerReceiver.workloadExchange.serverName=other.example.test > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject a workload exchange server-name mismatch' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.maxIdentifierBytes=20 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must count the generated signing-secret path against the identifier limit' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.maxConcurrency=1025 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject concurrency above the binary ceiling' >&2
  exit 1
fi

for invalid_trigger in \
  'triggerReceiver.triggers[0].envelopeDigest=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
  'triggerReceiver.triggers[0].filters.project=proj lower' \
  'triggerReceiver.triggers[0].filters.issueType=not-numeric'
do
  if helm_template "${receiver_inputs[@]}" --set-string "${invalid_trigger}" > /dev/null 2>&1; then
    echo "Jira Trigger receiver must reject invalid ${invalid_trigger%%=*}" >&2
    exit 1
  fi
done

first_config_name="$(awk '/^metadata: \{ name: steward-trigger-receiver-config/ { print $4; exit }' "${rendered}" | tr -d '}')"
helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.triggers[0].cooldownSeconds=61 > "${rendered}"
second_config_name="$(awk '/^metadata: \{ name: steward-trigger-receiver-config/ { print $4; exit }' "${rendered}" | tr -d '}')"
if [[ -z "${first_config_name}" || "${first_config_name}" == "${second_config_name}" ]]; then
  echo 'Jira Trigger receiver config changes must select a new immutable ConfigMap' >&2
  exit 1
fi
helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.triggers[0].enabled=false > "${rendered}"
grep -qF 'optional: true' "${rendered}" || {
  echo 'a disabled Trigger must not require its signing Secret to exist' >&2
  exit 1
}

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.sourceTokenSeconds=599 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject a source token lifetime below 600 seconds' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.maxConcurrency=1 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject a Trigger above its configured concurrency limit' >&2
  exit 1
fi

if helm_template "${receiver_inputs[@]}" \
  --set triggerReceiver.limits.apiConnectTimeoutMilliseconds=5001 \
  --set triggerReceiver.limits.apiRequestTimeoutMilliseconds=5000 > /dev/null 2>&1; then
  echo 'Jira Trigger receiver must reject inconsistent Task API time settings' >&2
  exit 1
fi

if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set execution.enabled=false \
  --set config.taskOrchestrationMode=active > /dev/null 2>&1; then
  echo 'core-only chart must reject active Task orchestration' >&2
  exit 1
fi

if helm_template \
  --set-string tls.webhook.caBundlePem=public-test-ca \
  --set execution.enabled=false \
  --set config.apiserver.executionBindingsMode=active > /dev/null 2>&1; then
  echo 'core-only chart must reject active execution bindings' >&2
  exit 1
fi

governed_inputs=(
  --set-string tls.webhook.caBundlePem=public-test-ca
  --set execution.enabled=true
  --set-string 'runtimeNamespaces[0]=steward-workflows'
  --set-string images.mint.tag=test-mint
  --set-string images.mint.digest=sha256:2222222222222222222222222222222222222222222222222222222222222222
  --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1/responses
  --set-string config.controller.openshellEndpoint=https://gateway.example.test:8080
  --set-string config.controller.openshellServerName=gateway.example.test
  --set-string config.controller.openshellRuntimeClassName=sandbox-vm
  --set-string config.controller.workloadExchangeEndpoint=https://identity.example.test/v1/workload/exchange
  --set-string config.controller.workloadExchangeServerName=identity.example.test
  --set-string config.controller.litellmUrl=https://litellm.example.test
  --set-string spire.className=steward
)
default_runtime_inputs=(
  --set-string tls.webhook.caBundlePem=public-test-ca
  --set execution.enabled=true
  --set-string images.mint.tag=test-mint
  --set-string images.mint.digest=sha256:2222222222222222222222222222222222222222222222222222222222222222
  --set-string config.apiserver.inferenceEndpoint=https://inference.example.test/v1/responses
  --set-string config.controller.openshellEndpoint=https://gateway.example.test:8080
  --set-string config.controller.openshellServerName=gateway.example.test
  --set-string config.controller.workloadExchangeEndpoint=https://identity.example.test/v1/workload/exchange
  --set-string config.controller.workloadExchangeServerName=identity.example.test
  --set-string config.controller.litellmUrl=https://litellm.example.test
  --set-string config.mint.issuer=https://mint.example.test
  --set-string config.mint.spiffeTrustDomain=customer.example.test
  --set-string config.mint.openshellNamespace=customer-openshell
  --set-string spire.className=steward
)
if helm_template "${default_runtime_inputs[@]}" > /dev/null 2>"${missing_namespace_error}"; then
  echo 'governed execution must reject a missing steward-workflows runtime namespace' >&2
  exit 1
fi
grep -Fq 'execution.enabled requires steward-workflows in runtimeNamespaces' \
  "${missing_namespace_error}"
helm_template "${default_runtime_inputs[@]}" \
  --set-string 'runtimeNamespaces[0]=steward-workflows' > "${rendered}"
if grep -qF 'STEWARD_OPENSHELL_RUNTIME_CLASS_NAME' "${rendered}"; then
  echo 'default runtime render must not request a RuntimeClass' >&2
  exit 1
fi
if helm_template "${governed_inputs[@]}" > /dev/null 2> "${rendered}"; then
  echo 'governed mode must not use an assumed Mint issuer or SPIFFE trust domain' >&2
  exit 1
fi
grep -qE 'mint\.issuer|spiffeTrustDomain' "${rendered}"
helm_template "${governed_inputs[@]}" \
  --set-string config.mint.issuer=https://mint.example.test \
  --set-string config.mint.spiffeTrustDomain=customer.example.test \
  --set-string config.mint.openshellNamespace=customer-openshell > "${rendered}"
grep -qF 'kind: ClusterSPIFFEID' "${rendered}"
grep -qF 'name: steward-mint' "${rendered}"

echo 'core-only chart dependency boundary passed'
