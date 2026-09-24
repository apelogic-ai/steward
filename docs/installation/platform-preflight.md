# Platform preflight bundle

Status: **Supported for Steward v0.2.2**

The release asset `steward-platform-preflight-0.2.2.tar.gz` contains a
dependency-free Python validator and generator, its input schema, and neutral
examples. It converts one reviewed non-secret input into deterministic Steward
Helm values, a Flux-compatible values `ConfigMap`, machine diagnostics, and a
human summary.

The input names every namespace, immutable image digest, public hostname,
Gateway parent, certificate DNS name, database CA reference, provider profile,
ARC controller service account, and external Secret. Secret bodies are neither
accepted nor emitted. Existing infrastructure remains operator-owned.

```sh
tar -xzf steward-platform-preflight-0.2.2.tar.gz
tar -xzf steward-runtime-providers-0.2.2.tar.gz
cd platform-preflight/v1
./steward-platform-preflight generate \
  --input examples/compact.json \
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
