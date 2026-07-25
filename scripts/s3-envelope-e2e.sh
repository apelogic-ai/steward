#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
run_id="s3-$(date -u +%Y%m%d%H%M%S)-$$"
cluster="steward-s3-${run_id}"
context="kind-${cluster}"
run_dir="${root}/.steward-run/${run_id}"
kubeconfig="${run_dir}/kubeconfig"
image="steward-s3-e2e:${run_id}"
port_forward_pid=""
postgres_forward_pid=""

cleanup() {
  status=$?
  trap - EXIT INT TERM
  set +e
  if [[ -n "${port_forward_pid}" ]]; then
    kill "${port_forward_pid}" >/dev/null 2>&1
    wait "${port_forward_pid}" >/dev/null 2>&1
  fi
  if [[ -n "${postgres_forward_pid}" ]]; then
    kill "${postgres_forward_pid}" >/dev/null 2>&1
    wait "${postgres_forward_pid}" >/dev/null 2>&1
  fi
  kind delete cluster --name "${cluster}" >/dev/null 2>&1
  docker image rm "${image}" >/dev/null 2>&1
  rm -rf -- "${run_dir}"
  exit "${status}"
}
trap cleanup EXIT INT TERM

for tool in cargo curl docker kind kubectl openssl sed; do
  command -v "${tool}" >/dev/null 2>&1 || {
    echo "error: ${tool} is required for the S3 E2E" >&2
    exit 1
  }
done

cd "${root}"
cargo xtask dev doctor
mkdir -p "${run_dir}"
chmod 700 "${run_dir}"
cat >"${run_dir}/manifest" <<EOF
run_id=${run_id}
cluster=${cluster}
image=${image}
EOF

docker build \
  --file e2e/Dockerfile.s3 \
  --label "steward.test/run-id=${run_id}" \
  --tag "${image}" \
  .

kind create cluster \
  --name "${cluster}" \
  --image "kindest/node:v1.32.1@sha256:6afef2b7f69d627ea7bf27ee6696b6868d18e03bf98167c420df486da4662db6" \
  --kubeconfig "${kubeconfig}" \
  --wait 120s

kind load docker-image --name "${cluster}" "${image}"

openssl genpkey \
  -algorithm RSA \
  -pkeyopt rsa_keygen_bits:2048 \
  -out "${run_dir}/tls-key.pem" \
  >/dev/null 2>&1
openssl req \
  -new \
  -x509 \
  -key "${run_dir}/tls-key.pem" \
  -out "${run_dir}/tls-cert.pem" \
  -days 1 \
  -subj "/CN=steward-s3.test" \
  -addext "subjectAltName=DNS:steward-s3.test,DNS:steward-s3.steward-system.svc" \
  >/dev/null 2>&1
openssl x509 \
  -in "${run_dir}/tls-cert.pem" \
  -outform DER \
  -out "${run_dir}/tls-cert.der"
openssl pkcs8 \
  -topk8 \
  -nocrypt \
  -in "${run_dir}/tls-key.pem" \
  -outform DER \
  -out "${run_dir}/tls-key.der"
chmod 600 "${run_dir}/tls-key.pem" "${run_dir}/tls-key.der"

kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  apply -f manifests/agents.apelogic.ai_agentruntimes.yaml

kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  create namespace steward-system
kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  label namespace steward-system "steward.test/run-id=${run_id}"
kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  -n steward-system \
  create secret generic steward-s3-tls \
  --from-file=tls-cert.der="${run_dir}/tls-cert.der" \
  --from-file=tls-key.der="${run_dir}/tls-key.der"
kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  -n steward-system \
  label secret steward-s3-tls "steward.test/run-id=${run_id}"

cat <<EOF | kubectl --kubeconfig "${kubeconfig}" --context "${context}" apply -f -
apiVersion: v1
kind: ServiceAccount
metadata:
  name: steward-s3
  namespace: steward-system
  labels:
    steward.test/run-id: "${run_id}"
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: steward-s3-${run_id}
  labels:
    steward.test/run-id: "${run_id}"
rules:
  - apiGroups: ["agents.apelogic.ai"]
    resources: ["agentruntimes"]
    verbs: ["get", "update"]
  - apiGroups: [""]
    resources: ["users"]
    resourceNames: ["alice@example.com"]
    verbs: ["impersonate"]
  - apiGroups: [""]
    resources: ["groups"]
    resourceNames: ["agents.apelogic.ai/member-role:engineer"]
    verbs: ["impersonate"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: steward-s3-${run_id}
  labels:
    steward.test/run-id: "${run_id}"
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: steward-s3-${run_id}
subjects:
  - kind: ServiceAccount
    name: steward-s3
    namespace: steward-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: steward-s3-acting-user-${run_id}
  labels:
    steward.test/run-id: "${run_id}"
rules:
  - apiGroups: ["agents.apelogic.ai"]
    resources: ["agentruntimes"]
    verbs: ["create", "get", "patch", "update"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: steward-s3-acting-user-${run_id}
  labels:
    steward.test/run-id: "${run_id}"
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: steward-s3-acting-user-${run_id}
subjects:
  - kind: Group
    name: agents.apelogic.ai/member-role:engineer
    apiGroup: rbac.authorization.k8s.io
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: postgres
  namespace: steward-system
  labels:
    steward.test/run-id: "${run_id}"
spec:
  replicas: 1
  selector:
    matchLabels:
      app: postgres
  template:
    metadata:
      labels:
        app: postgres
        steward.test/run-id: "${run_id}"
    spec:
      containers:
        - name: postgres
          image: postgres:16-alpine@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777
          env:
            - name: POSTGRES_USER
              value: steward
            - name: POSTGRES_DB
              value: steward
            - name: POSTGRES_HOST_AUTH_METHOD
              value: trust
          ports:
            - name: postgres
              containerPort: 5432
          readinessProbe:
            exec:
              command: ["pg_isready", "-U", "steward", "-d", "steward"]
            initialDelaySeconds: 2
            periodSeconds: 2
---
apiVersion: v1
kind: Service
metadata:
  name: postgres
  namespace: steward-system
  labels:
    steward.test/run-id: "${run_id}"
spec:
  selector:
    app: postgres
  ports:
    - name: postgres
      port: 5432
      targetPort: postgres
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: steward-s3
  namespace: steward-system
  labels:
    steward.test/run-id: "${run_id}"
spec:
  replicas: 1
  selector:
    matchLabels:
      app: steward-s3
  template:
    metadata:
      labels:
        app: steward-s3
        steward.test/run-id: "${run_id}"
    spec:
      serviceAccountName: steward-s3
      initContainers:
        - name: wait-postgres
          image: postgres:16-alpine@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777
          command:
            - sh
            - -c
            - until pg_isready -h postgres -U steward -d steward; do sleep 1; done
      containers:
        - name: steward-s3
          image: "${image}"
          imagePullPolicy: Never
          env:
            - name: STEWARD_TEST_DATABASE_URL
              value: postgres://steward@postgres.steward-system.svc/steward
            - name: STEWARD_TEST_ACTOR
              value: alice@example.com
            - name: STEWARD_TEST_MEMBER_ROLE
              value: engineer
            - name: STEWARD_TEST_ADMIN
              value: admin@example.com
            - name: STEWARD_TEST_TLS_CERT_DER
              value: /tls/tls-cert.der
            - name: STEWARD_TEST_TLS_KEY_DER
              value: /tls/tls-key.der
          ports:
            - name: https
              containerPort: 8080
          readinessProbe:
            tcpSocket:
              port: https
            initialDelaySeconds: 2
            periodSeconds: 2
          volumeMounts:
            - name: tls
              mountPath: /tls
              readOnly: true
      volumes:
        - name: tls
          secret:
            secretName: steward-s3-tls
            defaultMode: 0444
---
apiVersion: v1
kind: Service
metadata:
  name: steward-s3
  namespace: steward-system
  labels:
    steward.test/run-id: "${run_id}"
spec:
  selector:
    app: steward-s3
  ports:
    - name: https
      port: 443
      targetPort: https
EOF

kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  -n steward-system \
  rollout status deployment/postgres \
  --timeout=120s
kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  -n steward-system \
  rollout status deployment/steward-s3 \
  --timeout=180s

ca_bundle="$(base64 <"${run_dir}/tls-cert.pem" | tr -d '\n')"
cat <<EOF | kubectl --kubeconfig "${kubeconfig}" --context "${context}" apply -f -
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: steward-s3-${run_id}
  labels:
    steward.test/run-id: "${run_id}"
webhooks:
  - name: agentruntime.agents.apelogic.ai
    admissionReviewVersions: ["v1"]
    sideEffects: None
    failurePolicy: Fail
    timeoutSeconds: 10
    clientConfig:
      service:
        namespace: steward-system
        name: steward-s3
        path: /validate-agent-runtime
        port: 443
      caBundle: "${ca_bundle}"
    rules:
      - apiGroups: ["agents.apelogic.ai"]
        apiVersions: ["v1alpha1"]
        operations: ["CREATE", "UPDATE"]
        resources: ["agentruntimes"]
        scope: Namespaced
EOF

port_forward_log="${run_dir}/port-forward.log"
kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  -n steward-system \
  port-forward service/steward-s3 :443 \
  >"${port_forward_log}" 2>&1 &
port_forward_pid=$!

port=""
for _attempt in {1..30}; do
  port="$(sed -n 's/.*127\.0\.0\.1:\([0-9][0-9]*\).*/\1/p' "${port_forward_log}" | head -n 1)"
  if [[ -n "${port}" ]]; then
    break
  fi
  if ! kill -0 "${port_forward_pid}" >/dev/null 2>&1; then
    cat "${port_forward_log}" >&2
    exit 1
  fi
  sleep 1
done
if [[ -z "${port}" ]]; then
  echo "error: kubectl port-forward did not publish a local port" >&2
  exit 1
fi

postgres_forward_log="${run_dir}/postgres-port-forward.log"
kubectl \
  --kubeconfig "${kubeconfig}" \
  --context "${context}" \
  -n steward-system \
  port-forward service/postgres :5432 \
  >"${postgres_forward_log}" 2>&1 &
postgres_forward_pid=$!

postgres_port=""
for _attempt in {1..30}; do
  postgres_port="$(sed -n 's/.*127\.0\.0\.1:\([0-9][0-9]*\).*/\1/p' "${postgres_forward_log}" | head -n 1)"
  if [[ -n "${postgres_port}" ]]; then
    break
  fi
  if ! kill -0 "${postgres_forward_pid}" >/dev/null 2>&1; then
    cat "${postgres_forward_log}" >&2
    exit 1
  fi
  sleep 1
done
if [[ -z "${postgres_port}" ]]; then
  echo "error: Postgres port-forward did not publish a local port" >&2
  exit 1
fi

STEWARD_TEST_DATABASE_URL="postgres://steward@127.0.0.1:${postgres_port}/steward" \
  cargo test \
    --manifest-path e2e/Cargo.toml \
    --test s3_store

STEWARD_TEST_KUBE_CONTEXT="${context}" \
STEWARD_TEST_KUBECONFIG="${kubeconfig}" \
STEWARD_RUN_DIR="${run_dir}" \
STEWARD_TEST_TLS_CA="${run_dir}/tls-cert.pem" \
STEWARD_S3_URL="https://steward-s3.test:${port}" \
STEWARD_S3_RESOLVE="steward-s3.test:${port}:127.0.0.1" \
  cargo test \
    --manifest-path e2e/Cargo.toml \
    --test s3 \
    e2e_s3_composed_edits_rejected \
    -- \
    --exact
