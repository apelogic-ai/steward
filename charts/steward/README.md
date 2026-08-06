# Steward Helm chart

This chart installs the Steward apiserver, controller/webhook, mint, and the
`AgentRuntime` CRD. It intentionally creates no `Ingress`; all three services
are cluster-internal `ClusterIP` services.

The default values are fail-closed. A release consumer must set a non-empty tag
and immutable digest for every component before Helm will render the chart:

```yaml
images:
  repository: <registry>/<repository>
  pullPolicy: IfNotPresent
  apiserver:
    tag: <version>-apiserver
    digest: sha256:<digest>
  controller:
    tag: <version>-controller
    digest: sha256:<digest>
  mint:
    tag: <version>-mint
    digest: sha256:<digest>
```

The release handoff attached to every GitHub release records these three image
digests and the OCI chart digest. If repository variable
`ECR_PROMOTION_ENABLED` is `true`, the release workflow also copies those exact
manifests to the configured ECR repositories, verifies that the digests did not
change, waits for native ECR scanning, rejects critical findings, signs them,
and publishes a private-registry handoff artifact. Promotion is retry-safe: a
missing immutable tag is created, a matching tag is reused, and a tag pointing
to any other digest fails closed. For BuildKit OCI indexes, the workflow scans
exactly one runnable `linux/amd64` child manifest (excluding SBOM/provenance
attestations) and records both the release index digest and scanned platform
digest. A missing or ambiguous runnable child fails closed.

## Required secrets

The chart references five existing Secrets and never creates their values:

| Secret | Key(s) | Mounted by |
|---|---|---|
| `steward-database` | `url` | apiserver, controller |
| `steward-jira` | `token` | apiserver |
| `steward-litellm` | `master-key` | controller |
| `steward-openshell-client` | `ca.crt`, `tls.crt`, `tls.key`, `token` | controller |
| `steward-mint` | `signing-key`, `introspection-credential` | mint |

The mint Secret is not referenced by either the apiserver or controller
Deployment. `steward-apiserver-tls` and `steward-webhook-tls` are issued by
cert-manager from `tls.issuerRef`; the binaries accept cert-manager's PEM
certificate chains and private keys.

The globally bound controller and mint ClusterRoles have no Secret verbs.
Runtime Secret access is granted by namespaced Roles and RoleBindings only for
names listed in `runtimeNamespaces`. The default is an empty list, so a release
consumer must explicitly authorize every runtime namespace; namespaces outside
that allowlist remain inaccessible to both service accounts.

## Runtime configuration

- `config.apiserver.taskTokenAudience` is the required Kubernetes TokenReview
  audience for the Task API. The Task API is enabled on the apiserver service;
  its internal port is `services.apiserverPort`.
- `config.apiserver.taskWorkflowsJson` is the complete Task workflow catalog as
  a JSON array. Invalid JSON or an empty Task audience stops the apiserver.
- `config.apiserver.jiraBaseUrl`, `jiraProjectKey`, and `jiraAccountEmail` are
  mandatory startup inputs. The base URL is the public HTTPS Jira tenant root,
  the project must already exist, and the account email must correspond to the
  raw API token in the configured Jira Secret. The chart does not support a
  dummy credential or an implicit Jira-disabled mode.
- `config.controller.litellmUrl` and
  `config.controller.openshellEndpoint` are internal service endpoints.
- The OpenShell endpoint must use HTTPS. `openshellServerName` pins the TLS
  identity, while `secrets.openshellClient` supplies the trusted CA, client
  certificate/private key, and bearer token. Missing transport trust or caller
  authentication stops the controller; ambient workstation credentials and
  plaintext gRPC are not supported.
- `config.controller.openshellRuntimeClassName` must be `kata-qemu`, matching
  OpenShell's gateway-level `defaultRuntimeClassName`. Steward does not send a
  sandbox image or expose per-create driver/runtime overrides, so the gateway's
  configured image and runtime policy remain authoritative.
- `config.mint.issuer` is the issuer that must also be configured in MCP-GW.
  Steward publishes JWKS at `<issuer>/.well-known/jwks.json` and uses EdDSA.
- `config.mint.audience` defaults to `steward-mcp` and
  `config.mint.allowedScopes` defaults to `mcp inference`. These include the
  checked-in OpenShell provider contract (`audience=steward-mcp`, `scope=mcp`)
  and the inference exchange on the same Mint instance.
- `spire.csiDriver` and `spire.socketPath` mount the SPIFFE Workload API only in
  the mint pod. The chart creates a `ClusterSPIFFEID` selecting the release
  namespace and Mint pod labels, with trust domain
  `config.mint.spiffeTrustDomain` and stable path `spire.identityPath`
  (`/steward/mint` by default).

Both the apiserver and controller apply the embedded Postgres migration set on
startup, including migration `0011`. They must receive the same database URL.

The Task workflow's `AgentRuntime` spec must fit an envelope authorized for the
`steward-run` service principal. That envelope is governance data, not a Helm
value, and must exist before Task execution is enabled for callers.
`config/task/workflows.example.json` and its matching service envelope are the
authority-minimal copy-smoke contract: no LLMs, tools, LiteLLM calls, or MCP
calls. `scripts/bootstrap-task-copy-smoke.sh` installs that envelope
idempotently over authenticated HTTPS. It requires an externally issued,
short-lived administrator token; the chart deliberately has no administrator
token Secret input. DEV bootstrap remains blocked until Infra supplies that
credential through its EKS OIDC identity-provider association.

Run `cargo xtask e2e-openshell-adapter` to exercise the adapter against the
exact OpenShell `v0.0.98` chart in an ephemeral kind cluster. The test verifies
authenticated TLS failures, CA and server-name validation, the `kata-qemu`
contract, input/output SHA-256 equality, and sandbox-last cleanup.

## Network policy

`networkPolicy.enabled` defaults to `true`. The chart denies ingress and egress
for all Steward pods, then opens only these paths:

- Burble and ARC namespaces to the apiserver;
- configured Kubernetes API/VPC CIDRs to the validating webhook;
- OpenShell and MCP-GW namespaces to the mint;
- controller to LiteLLM, OpenShell, Postgres, and the Kubernetes API;
- apiserver to Jira, Postgres, and the Kubernetes API;
- mint to the Kubernetes API; and
- all Steward components to cluster DNS.

`kubeApiCidrs`, `postgresCidrs`, and `jiraCidrs` default to empty arrays. Empty
means denied, not unrestricted, so production values must supply the applicable
CIDRs. FQDN-aware egress policy, if used by the cluster, belongs in the GitOps
layer rather than this portable Kubernetes `NetworkPolicy` chart.

## Release configuration

Public releases use:

- images: `ghcr.io/<owner>/steward:<version>-<component>`;
- chart: `oci://ghcr.io/<owner>/charts/steward:<version>`.

Optional ECR promotion requires these repository variables:

- `ECR_PROMOTION_ENABLED=true`
- `AWS_RELEASE_ROLE_ARN`
- `AWS_REGION`
- `ECR_REGISTRY`
- `STEWARD_ECR_IMAGE_REPOSITORY`
- `STEWARD_ECR_CHART_REPOSITORY`

No AWS coordinate or credential is stored in the chart or repository.
