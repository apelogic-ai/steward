# Platform preflight bundle

Status: **Supported for Steward v0.2.3**

The release asset `steward-platform-preflight-0.2.3.tar.gz` contains a
dependency-free Python validator and generator, its input schema, and neutral
examples. It converts one reviewed non-secret input into deterministic Steward
Helm values, a Flux-compatible values `ConfigMap`, machine diagnostics, and a
human summary.

The input names every namespace, immutable image digest, public hostname,
Gateway parent, certificate DNS name, database CA reference, provider profile,
ARC controller service account, external Secret, template capability catalog,
and NetworkPolicy API/PostgreSQL destinations. Secret bodies are neither
accepted nor emitted. Existing infrastructure remains operator-owned.

```sh
tar -xzf steward-platform-preflight-0.2.3.tar.gz
tar -xzf steward-runtime-providers-0.2.3.tar.gz
cd platform-preflight/v1
./steward-platform-preflight generate \
  --input examples/governed-complete.json \
  --provider-profile-bundle ../../provider-profile-bundle/v1.2.0 \
  --chart /path/to/steward-chart \
  --output rendered
```

The command fails before rollout for mutable or placeholder images, a missing
database CA, namespace drift, incomplete references, a hostname not covered by
the configured certificate names, or Helm lint/render failure. The generated
`steward-values.json` can be passed directly to `helm --values`. The generated
`flux-values-configmap.yaml` can be committed and referenced by a Flux
`HelmRelease.valuesFrom` entry. `provider-profile-inputs.json` is accepted by
the released provider-profile installer. The preflight invokes that released
installer in validation mode and puts its computed profile digests into the
execution binding; deployment input cannot substitute unrelated profile
digests. The input embeds the exact
`steward.deployment-lock/v1` document produced by the released registry mirror
tool, so component, bridge, and coding-agent coordinates require no manual
translation. Governed execution, active execution bindings, and the
Connections bridge are enabled together from those immutable coordinates and
the explicitly supplied endpoints.

[`examples/governed-complete.json`](../../config/platform-preflight/v1/examples/governed-complete.json)
is the copy-ready governed input. It includes a non-empty capability catalog,
one immutable execution binding, exact Codex Responses endpoint, MCP-GW
endpoint, all required namespaces, browser-auth egress, Kubernetes API and
PostgreSQL CIDRs, and every immutable component/runtime coordinate. The
compact and separated examples exercise the same contract with different
namespace layouts.

The capability catalog is descriptive template-editor availability; it is not
runtime authority. The execution binding selects an immutable coding-agent
runtime. Provider profiles bind that runtime to deployment-owned inference and
tool connectivity. A User Envelope remains Steward's per-user admission limit
and does not replace any of those deployment settings.

`config.apiserver.inferenceEndpoint` is the exact OpenAI-compatible Responses
operation URL (`https://inference.example.test/v1/responses` in the examples).
`config.apiserver.anthropicInferenceEndpoint` is an Anthropic-compatible API
base URL, while `config.controller.litellmUrl` is the LiteLLM management API
base URL with no operation path.

## Post-install inspection

After installing the generated values, use the following read-only checks. Set
`STEWARD_NAMESPACE` to the generated Steward namespace and `STEWARD_RELEASE`
to the Helm release name.

```sh
helm get values "$STEWARD_RELEASE" --namespace "$STEWARD_NAMESPACE" --all
CAPABILITY_CONFIGMAP="$(kubectl get deployment steward-apiserver -n "$STEWARD_NAMESPACE" \
  -o jsonpath='{.spec.template.spec.volumes[?(@.name=="capability-catalog")].configMap.name}')"
kubectl get configmap "$CAPABILITY_CONFIGMAP" -n "$STEWARD_NAMESPACE" -o yaml
kubectl get deployment steward-apiserver -n "$STEWARD_NAMESPACE" -o yaml | \
  grep -E 'checksum/capability-catalog|/run/capability-catalog'
curl --fail-with-body \
  -H "Authorization: Bearer $STEWARD_ADMIN_TOKEN" \
  "https://steward.example.test/admin/api/v1/capabilities"
```

The first command shows the effective Helm values. The ConfigMap output
contains the content-addressed capability catalog. The Deployment output proves
the catalog mount and checksum-driven rollout wiring. The final authenticated
request reports the capability catalog exposed to the administration UI.

The compact example co-locates Steward, runtime, and provider resources. The
namespace map remains explicit even when values match so moving one component
does not leave hidden cross-namespace references.

`namespace-map.schema.json` is the single namespace contract. The separated
example assigns distinct namespaces to the control plane, runtimes, provider
profiles, edge, ARC, MCP gateway, inference gateway, identity exchange,
OpenShell, and DNS. Database Secret and CA objects remain in the Steward
namespace because Kubernetes Pod volume references cannot cross namespaces.
Generation emits `namespace-references.json` so external-object ownership and
every namespace-qualified reference can be reviewed before apply.

## DNS, certificate, and Gateway contract

An exact certificate SAN covers only that exact hostname. A wildcard such as
`*.example.test` covers `steward.example.test`, but it does not cover
`service.product.example.test`: TLS wildcards match exactly one DNS label.

After static validation, prove the declared parent against a live cluster with
a read-only lookup:

```sh
./steward-platform-preflight gateway-check \
  --input examples/compact.json \
  --kubeconfig "$KUBECONFIG_FILE" \
  --context "$KUBECONFIG_CONTEXT"
```

The command uses `kubectl get` only. It verifies the exact Gateway namespace
and name, HTTPS listener section, listener hostname, referenced TLS Secret,
ARC controller ServiceAccount, external Secrets, workload-exchange trust
ConfigMap, and database CA source. It requests metadata only for Secret and
ConfigMap existence checks and does not retrieve their bodies. It does not
create DNS records, certificates, Gateways, or routes.

## Gateway backend TLS

The chart requires Gateway API `v1.4.0` or later and a `GatewayClass` that
reports the extended `BackendTLSPolicy` feature. The platform owns those CRDs,
the controller, and CA distribution; the chart owns the `BackendTLSPolicy`
attached to the apiserver HTTPS Service. The released preflight bundle also
contains `steward-gateway-backend-tls-check`, which performs the complete
read-only validation without printing Secret contents or private keys:

```sh
./steward-gateway-backend-tls-check \
  --kubeconfig "$CLUSTER_KUBECONFIG" \
  --context "$CLUSTER_CONTEXT" \
  --namespace steward \
  --gateway-class <gateway-class> \
  --ca-config-map steward-apiserver-ca \
  --api-tls-secret steward-apiserver-tls \
  --public-url https://steward.example.test
```

For an initial platform inspection, use the following read-only checks:

```sh
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  get crd backendtlspolicies.gateway.networking.k8s.io

kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  get gatewayclass <gateway-class> \
  -o json | jq -e '
    [.status.supportedFeatures[]? | if type == "string" then . else .name end]
    | index("BackendTLSPolicy")
  ' >/dev/null

kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward get configmap steward-apiserver-ca \
  -o jsonpath='{.data.ca\\.crt}' >/dev/null

kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward get service steward-apiserver \
  -o jsonpath='{.spec.ports[?(@.name=="https")].port}{"\\n"}'

kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward get httproute steward-api
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward get backendtlspolicy steward-apiserver
```

The API route and policy must both report `Accepted=True` and
`ResolvedRefs=True` for the selected Gateway controller. The apiserver policy
must target `Service/steward-apiserver`, section `https`, use the full Service
DNS name, and reference the same-namespace public CA ConfigMap at `ca.crt`.
Use a trust-distribution controller to keep that ConfigMap current when the
issuer rotates; never copy `tls.key` or the apiserver TLS Secret into the
Gateway namespace.

Finally, make a bounded request to the public session path. A Steward-owned
`200`, `401`, or `403` proves the request reached the application. A `502` or
`503` is a Gateway/backend failure, not an OAuth response:

```sh
curl --silent --show-error --output /dev/null \
  --write-out '%{http_code}\n' \
  https://steward.example.test/admin/api/v1/session
```

For diagnostics, distinguish: no ready `EndpointSlice` entries (backend has no
ready Pods); `HTTPRoute ResolvedRefs=False` (wrong Service/port or route
reference); `BackendTLSPolicy ResolvedRefs=False` (missing or invalid CA
reference); policy `Accepted=False` (the controller does not support or accept
the policy); and a public `502`/`503` with accepted objects (CA, SNI, or
backend TLS protocol mismatch). Do not work around any of these by changing
the apiserver Service to plaintext.

## NetworkPolicy enforcement on EKS

The supported EKS prerequisite is the Amazon VPC CNI configuration documented
by AWS: the `kube-system/aws-node` DaemonSet must contain the
`aws-network-policy-agent` container with network policy enabled. AWS documents
the feature and its version prerequisites at
<https://docs.aws.amazon.com/eks/latest/userguide/cni-network-policy-configure.html>.
The preflight reports the observed agent image, but it does not approve a
cluster from a version string alone.

First perform the read-only check:

```sh
./steward-platform-preflight network-check \
  --kubeconfig "$KUBECONFIG_FILE" \
  --context "$KUBECONFIG_CONTEXT"
```

Then prove enforcement using an immutable, reviewed image that provides `sh`,
`httpd`, `sleep`, and `wget`:

```sh
./steward-platform-preflight network-smoke \
  --kubeconfig "$KUBECONFIG_FILE" \
  --context "$KUBECONFIG_CONTEXT" \
  --run-id smoke-1 \
  --probe-image registry.example.test/team-a/network-probe@sha256:<digest>
```

The smoke creates one run-owned namespace and labeled probe resources, first
proves the Service and its DNS/endpoints work without a policy, then proves an
actual deny through consecutive failed probes, applies a narrowly scoped allow
policy, proves the authorized connection, and deletes only the namespace whose
run label and immutable UID still match the create-only result. A pre-existing
namespace is never adopted. Cleanup is attempted on success, failure, SIGINT,
and SIGTERM. An absent or
disabled policy agent, an unproven deny, an unproven allow, or failed owned
namespace cleanup is blocking; cleanup failures include the exact retry command.
