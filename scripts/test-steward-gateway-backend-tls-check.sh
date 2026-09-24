#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT INT TERM

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=steward-test-ca' \
  -keyout "$temporary/ca.key" -out "$temporary/ca.crt" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes \
  -subj '/CN=steward-apiserver.steward.svc.cluster.local' \
  -addext 'subjectAltName=DNS:steward-apiserver.steward.svc.cluster.local' \
  -keyout "$temporary/server.key" -out "$temporary/server.csr" >/dev/null 2>&1
openssl x509 -req -days 1 -in "$temporary/server.csr" \
  -CA "$temporary/ca.crt" -CAkey "$temporary/ca.key" -CAcreateserial \
  -copy_extensions copy -out "$temporary/server.crt" >/dev/null 2>&1

ca_base64="$(base64 < "$temporary/ca.crt" | tr -d '\n')"
certificate_base64="$(base64 < "$temporary/server.crt" | tr -d '\n')"
cat > "$temporary/kubectl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
arguments="\$*"
case "\$arguments" in
  *'get crd backendtlspolicies.gateway.networking.k8s.io'*'spec.versions'*) printf 'v1\\n' ;;
  *'get crd backendtlspolicies.gateway.networking.k8s.io'*'bundle-version'*) printf 'v1.4.0' ;;
  *'get gatewayclass public-gateway-class'*) printf '%s\\n' "\${TLS_CHECK_FEATURE:-BackendTLSPolicy}" ;;
  *'get service steward-apiserver'*'spec.ports'*) printf '443' ;;
  *'get endpointslices -l kubernetes.io/service-name=steward-apiserver'*) printf 'true\\n' ;;
  *'get backendtlspolicy steward-apiserver'*'targetRefs'*) printf 'Service/steward-apiserver#https' ;;
  *'get backendtlspolicy steward-apiserver'*'validation.hostname'*) printf 'steward-apiserver.steward.svc.cluster.local' ;;
  *'get backendtlspolicy steward-apiserver'*'caCertificateRefs'*) printf 'ConfigMap/steward-apiserver-ca' ;;
  *'get httproute/steward-api'*) printf 'Accepted=True\\nResolvedRefs=True\\n' ;;
  *'get backendtlspolicy/steward-apiserver'*) printf 'Accepted=True\\nResolvedRefs=True\\n' ;;
  *'get configmap steward-apiserver-ca'*) printf '%s' "\${TLS_CHECK_CA_BASE64:-${ca_base64}}" | base64 -D ;;
  *'get secret steward-apiserver-tls'*) printf '%s' '${certificate_base64}' ;;
  *) echo "unexpected kubectl invocation: \$arguments" >&2; exit 1 ;;
esac
EOF
chmod +x "$temporary/kubectl"
cat > "$temporary/curl" <<'EOF'
#!/usr/bin/env bash
printf '%s' "${TLS_CHECK_HTTP_STATUS:-401}"
EOF
chmod +x "$temporary/curl"

"$root/scripts/steward-gateway-backend-tls-check.sh" \
  --kubeconfig "$temporary/kubeconfig" \
  --context steward-test \
  --namespace steward \
  --gateway-class public-gateway-class \
  --ca-config-map steward-apiserver-ca \
  --api-tls-secret steward-apiserver-tls \
  --public-url https://steward.example.test \
  --kubectl "$temporary/kubectl" \
  --curl "$temporary/curl" >/dev/null

if "$root/scripts/steward-gateway-backend-tls-check.sh" \
  --kubeconfig "$temporary/kubeconfig" \
  --context steward-test \
  --namespace wrong \
  --gateway-class public-gateway-class \
  --ca-config-map steward-apiserver-ca \
  --api-tls-secret steward-apiserver-tls \
  --public-url https://steward.example.test \
  --kubectl "$temporary/kubectl" \
  --curl "$temporary/curl" >/dev/null 2>&1
then
  echo 'backend TLS checker accepted a certificate with an incorrect Service DNS identity' >&2
  exit 1
fi

if TLS_CHECK_FEATURE=HTTPRoute "$root/scripts/steward-gateway-backend-tls-check.sh" \
  --kubeconfig "$temporary/kubeconfig" \
  --context steward-test \
  --namespace steward \
  --gateway-class public-gateway-class \
  --ca-config-map steward-apiserver-ca \
  --api-tls-secret steward-apiserver-tls \
  --public-url https://steward.example.test \
  --kubectl "$temporary/kubectl" \
  --curl "$temporary/curl" >/dev/null 2>&1
then
  echo 'backend TLS checker accepted a GatewayClass without BackendTLSPolicy support' >&2
  exit 1
fi

if TLS_CHECK_CA_BASE64="$(printf 'not a certificate' | base64 | tr -d '\n')" "$root/scripts/steward-gateway-backend-tls-check.sh" \
  --kubeconfig "$temporary/kubeconfig" \
  --context steward-test \
  --namespace steward \
  --gateway-class public-gateway-class \
  --ca-config-map steward-apiserver-ca \
  --api-tls-secret steward-apiserver-tls \
  --public-url https://steward.example.test \
  --kubectl "$temporary/kubectl" \
  --curl "$temporary/curl" >/dev/null 2>&1
then
  echo 'backend TLS checker accepted an invalid public CA' >&2
  exit 1
fi

if TLS_CHECK_HTTP_STATUS=502 "$root/scripts/steward-gateway-backend-tls-check.sh" \
  --kubeconfig "$temporary/kubeconfig" \
  --context steward-test \
  --namespace steward \
  --gateway-class public-gateway-class \
  --ca-config-map steward-apiserver-ca \
  --api-tls-secret steward-apiserver-tls \
  --public-url https://steward.example.test \
  --kubectl "$temporary/kubectl" \
  --curl "$temporary/curl" >/dev/null 2>&1
then
  echo 'backend TLS checker accepted a Gateway-generated 502 response' >&2
  exit 1
fi

echo 'Gateway backend TLS checker contract passed'
