# Steward Helm chart

Current release contract: chart `0.2.3` and application `0.2.3`.

This chart installs the Steward apiserver, controller/webhook, and
`AgentRuntime` CRD. Mint and governed execution are opt-in; the web
presentation and every edge route also default to disabled. The chart can
render an explicitly configured Gateway API `HTTPRoute`, but never installs a
Gateway, Gateway API CRDs, an Ingress controller, DNS, a certificate issuer,
or private edge topology.

The checked-in image repository, tags, and digests are empty by design: there
is no assumed vendor registry or preselected release. Supply every enabled
component's immutable coordinates from the same chosen source revision. Fork
operators set a fork-owned repository and publish their own images and chart:

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
    tag: "" # set tag and digest when execution.enabled=true
    digest: ""
  web:
    tag: "" # set tag and digest when web.enabled=true
    digest: ""
```

Never set a tag without the matching digest or use a mutable image reference.
The chart rejects the all-zero SHA-256 sentinel: it is a placeholder, not a
released immutable digest. The same rejection applies to execution-binding
images, provider-profile digests, and enabled bridge images.
Set `images.web.tag` and `images.web.digest` to empty strings if web is disabled
and no web image is published. Only enabled workloads are rendered.

The release handoff attached to every GitHub release records these component
digests and the OCI chart digest published to GHCR.

## Installation contract

Steward is a Kubernetes control plane, not a self-contained database or
identity bundle. A core-only installation needs Kubernetes 1.30 or later,
an external PostgreSQL database, immutable images, and TLS for the API and
webhook. It does **not** need Jira, a model endpoint, LiteLLM, OpenShell,
SPIRE, a sandbox RuntimeClass, or a Mint Secret. Set `execution.enabled=true`
only after supplying and verifying those governed-execution dependencies;
`jira.enabled=true` separately opts in to Jira. See the
[installation guide](../../docs/installation/installation-guide.md) for the
complete prerequisite, Secret, procedure, and delivery-test matrix.

The chart creates no Secret values, PVCs, database, ingress controller,
cert-manager issuer, or SPIRE control plane. Choose TLS mode before rendering:
`customerSecret` requires two pre-existing TLS Secrets and a public webhook
CA bundle; `certManager` requires cert-manager and an explicit issuer. Render
first, then install with a reviewed values file containing only configuration
and existing object names:

```console
helm lint ./charts/steward --values steward-values.yaml --set-file tls.webhook.caBundlePem=webhook-public-ca.pem
helm template steward ./charts/steward --namespace steward --values steward-values.yaml --set-file tls.webhook.caBundlePem=webhook-public-ca.pem > steward-rendered.yaml
helm --kubeconfig "$CLUSTER_KUBECONFIG" --kube-context "$CLUSTER_CONTEXT" upgrade --install steward ./charts/steward --namespace steward --create-namespace --atomic --wait --values steward-values.yaml --set-file tls.webhook.caBundlePem=webhook-public-ca.pem
```

The commands above show `customerSecret`; omit `--set-file` and set
`tls.mode=certManager` plus `tls.issuerRef` for the other mode. Supply exact
image coordinates, approved network CIDRs, and names/keys of externally
managed Secrets. The checked-in defaults are not a usable installation
values file. A successful render proves chart structure only; follow the
installation guide's live delivery tests before hand-off.

### Verified PostgreSQL TLS

The API server and controller can mount the same operator-managed PostgreSQL
CA from an existing `ConfigMap` or `Secret`. The chart never creates or rotates
that object. Enable the projection and reference its fixed read-only path from
the database URL:

```yaml
databaseTls:
  mode: verify-full
  ca:
    kind: ConfigMap # or Secret
    name: steward-postgres-ca
    key: ca.pem
```

The database URL stored under `secrets.database` must include
`sslmode=verify-full&sslrootcert=/run/database-tls/ca.crt`. An incomplete CA
source fails chart validation. The default `disabled` mode mounts nothing and
preserves existing installations.

## Workload defaults and platform integration

The chart creates fixed service accounts for enabled components because
controller-to-API identity is part of the Steward authority contract. Core
mode creates apiserver and controller accounts; governed mode also creates
Mint's account. These accounts have Kubernetes API tokens; the optional web
service account does not. `serviceAccounts.<component>.annotations` supports workload identity
integration such as EKS IRSA without inserting credentials into values.
`imagePullSecrets` names pre-existing registry credentials, and
`podAnnotations.<component>` is available for platform-owned metadata.

Every workload is non-root, uses the RuntimeDefault seccomp profile, drops all
Linux capabilities, disallows privilege escalation, and uses a read-only root
filesystem. Explicit resource requests/limits and readiness/liveness probe
timings are under `resources` and `probes`, respectively. Review those values
for the target capacity rather than relying on scheduler defaults.

Steward owns no persistent volume: all durable state is external Postgres
selected by `secrets.database`. `persistence.enabled` is intentionally fixed to
`false`; setting it true is a Helm error rather than silently creating an
unreviewed storage contract.

No metrics or ServiceMonitor resource is rendered because the currently
documented component interfaces do not expose a stable Prometheus contract.
Use the Kubernetes deployment/probe state and the platform's approved log and
event collection until a separately versioned observability interface exists.

For a shared Gateway API platform, set `web.httpRoute.enabled=true`. The chart
then renders two `HTTPRoute` objects and one `BackendTLSPolicy`:
`steward-api` targets `steward-apiserver` port `https` (443, targeting the
apiserver's TLS listener on 8443), `steward-web` targets `steward-web` port
`http` (3000), and the policy requires TLS for the apiserver backend. Their
`parentRefs`, hostname, API paths, web paths, and public-CA ConfigMap have no
defaults and must be supplied explicitly.

The backend policy is a same-namespace direct attachment to
`steward-apiserver` port section `https`. Its hostname is fixed to the full
Service DNS identity `steward-apiserver.<release-namespace>.svc.<cluster-domain>`;
the chart validates this rather than allowing an arbitrary SNI. The referenced
ConfigMap must have a non-empty public PEM CA at `data.ca.crt`. Gateway API does
not select an alternate ConfigMap key, so the chart requires that exact key.
The ConfigMap is public trust material only: never put `tls.key`, any private
key, or a TLS Secret in it.

Set `networkPolicy.ingressNamespace` to the explicit Envoy Gateway data-plane
namespace from which the route reaches the apiserver. It defaults to empty, so
no edge access is assumed. The chart intentionally does not declare a Gateway,
Gateway API CRDs, DNS, or certificate policy. For example:

```yaml
web:
  enabled: true
  host: steward.example.com
  httpRoute:
    enabled: true
    parentRefs:
      - name: shared-gateway
        namespace: envoy-gateway-system
        sectionName: https
    hostname: steward.example.com
    apiPaths:
      - { type: PathPrefix, value: /admin/api }
      - { type: PathPrefix, value: /admin/auth }
      - { type: Exact, value: /admin/connections/github/callback }
      - { type: PathPrefix, value: /app/api }
      - { type: PathPrefix, value: /v1 }
    webPaths:
      - { type: PathPrefix, value: / }
    backendTls:
      hostname: steward-apiserver.steward.svc.cluster.local
      caConfigMap:
        name: steward-apiserver-ca
        key: ca.crt
networkPolicy:
  ingressNamespace: envoy-gateway-system
```

Gateway API `v1.4.0` or later and a controller whose selected `GatewayClass`
reports `BackendTLSPolicy` support are required. Envoy Gateway `v1.9.1` is the
currently supported, tested controller line for this chart. The chart does not
install Gateway API CRDs or a controller.

Publish the CA ConfigMap through the platform's public trust-distribution
controller (for example, a trust-manager `Bundle` whose ConfigMap target is in
the Steward release namespace). That controller, not a human edit, must update
`data.ca.crt` when the issuing CA rotates. The chart never copies the
apiserver TLS Secret or its private key to the Gateway.

`web.ingress.enabled=true` remains a legacy, portable Kubernetes `Ingress`
interface for installations that explicitly choose it. Its annotation maps are
empty by default and controller-specific. It is mutually exclusive with
`web.httpRoute.enabled`. In either edge mode, `web.host` must exactly match the
browser origin; the legacy Ingress additionally requires an existing TLS Secret.

Execution bindings default to `config.apiserver.executionBindingsMode: staged`. An upgrade from a
pre-binding release must roll out all new binaries in that mode before a second Helm operation sets
the mode to `active`; see the repository's execution-binding upgrade note. This prevents either
side of a mixed-version rollout from interpreting a Task under the other version's contract.

Durable Task orchestration separately defaults to `config.taskOrchestrationMode: staged`. In this
mode the apiserver rejects new public and internal Task submissions, and the controller does not run
the Task lifecycle owner or approval dispatcher. Roll every apiserver and controller replica with
`staged`, verify that no legacy writer remains, and then use a separate Helm operation to set the
shared value to `active`. Existing Task reads and exact idempotent retries remain available during
the staged deployment.

## Required existing references

The chart references existing objects and never creates secret values:

| Existing reference | Key(s) | Required when |
|---|---|---|
| `steward-database` Secret | `url` | Always; apiserver and controller |
| API and webhook TLS Secrets | `tls.crt`, `tls.key` | Always; supplied by customer or cert-manager |
| `steward-jira` Secret | `token` | `jira.enabled=true`; apiserver |
| `steward-litellm` Secret | `master-key` | `execution.enabled=true`; controller |
| `steward-openshell-client` Secret | `ca.crt`, `tls.crt`, `tls.key` | `execution.enabled=true`; controller |
| `steward-mint` Secret | `signing-key`, `introspection-credential` | `execution.enabled=true`; mint |
| browser-auth Secret | configured client-secret key | `browserAuth.enabled=true`; apiserver |
| GitHub source App Secret | configured PEM private-key key | `githubSource.enabled=true`; apiserver |

In governed mode the controller also requires the public workload-exchange CA bundle selected
by `workloadExchangeTrust.kind`, `workloadExchangeTrust.name`, and
`workloadExchangeTrust.caCertificate`. `ConfigMap` is the default; `Secret`
supports cert-manager-managed local trust bundles while projecting only the
named CA key into the workload.

The mint Secret is not referenced by either the apiserver or controller
Deployment. TLS mode `customerSecret` consumes existing TLS Secrets and a
nonempty public webhook CA; `certManager` renders two Certificate resources
using `tls.issuerRef`. The binaries accept PEM certificate chains and private
keys in either mode.

## Browser authentication

`browserAuth` defaults to disabled. It is an atomic apiserver-only contract:
when disabled, every Google/OIDC value and Secret reference must be empty and
the chart renders no browser-auth environment variables or Secret projection.
When enabled, the chart requires an exact HTTPS browser origin, Google client
ID, hosted Workspace domain, Steward organization ID, and an existing Secret
name/key for the Google client secret. The chart never creates the Secret or a
public edge. A deployment adapter supplies the HTTPS route, certificate and
network policy appropriate to its platform (for example, a local Kind adapter
or a DEV gateway); those controls do not belong in this portable chart.
When the legacy `web.ingress.enabled=true` interface is selected, `web.host`
must exactly match the browser origin host and the Ingress class and TLS Secret
are required. With it disabled, those Ingress-only inputs may be empty and no
Ingress resources are rendered. A Gateway API deployment uses the chart's
explicit `web.httpRoute` interface. It retains ownership of the Gateway,
Gateway API CRDs, controller, edge certificate, and public-CA distribution;
the chart owns the TLS policy that attaches that public CA to its apiserver
Service.

When `networkPolicy.enabled=true`, browser authentication additionally requires
at least one `networkPolicy.browserAuthEgressCidrs` entry. The chart allows
apiserver HTTPS egress only to those platform-managed CIDRs; it does not open
unrestricted internet egress or encode Google IP ranges. The deployment
adapter is responsible for maintaining the approved resolver/egress policy as
Google's endpoints evolve. A local isolated lane may instead set
`networkPolicy.enabled=false`; that is not a substitute for a production
egress policy.

## Direct package source resolution

`githubSource` defaults to disabled. Enabling it is an atomic apiserver-only
configuration: the chart requires a positive GitHub App ID, an existing Secret
name and key containing its PEM private key, and at least one
`networkPolicy.githubApiCidrs` entry while NetworkPolicy is enabled. The private
key is mounted read-only and its bytes never enter Helm values or an environment
variable. Steward uses the App only to resolve exact Git objects; it does not
accept caller-uploaded package bytes.

`githubSource.bindings` authorizes exact caller-to-source repository pairs by
stable GitHub owner and repository IDs. The chart renders that non-secret
catalog into an immutable content-addressed ConfigMap and rolls the apiserver
when it changes. Repository names remain audit metadata and cannot substitute
for these IDs. The deployment adapter resolves and maintains the approved
GitHub API CIDRs; the portable chart opens HTTPS egress only to those entries.

## Governed provider connections

`connectionsBridge` is disabled by default. Enabling it requires browser
authentication, an immutable bridge image, an explicit artifact-trust contract,
the exact MCP-GW origin, and a dedicated runtime namespace. Authority v1 also
requires the named `connectionsBridge.mcpGatewayAuthorityContract` selector:
`steward.connections.github/v1` for the legacy status route or
`steward.connections.github/v2` for the lifecycle status contract used by
MCP-GW 0.4.9 through 0.4.11. The deprecated `mcpGatewayVersion` input remains
available for an existing values file and must not be set together with the
named selector. Another configured contract fails closed. The apiserver records
the frozen internal authority snapshot on each operation; the controller
verifies the same snapshot before creating or executing the short-lived
`steward-connections` runtime.

`connectionsBridge.artifactTrust.mode` defaults to `github-attestation`, the
recommended mode for released Steward artifacts. It requires the existing
signer identity, GitHub source repository, exact source commit, and public
artifact-attestation bundle; startup still fails if provenance cannot be
verified.

`operator-pinned` must be selected explicitly. It accepts only a canonical OCI
reference ending in an exact lowercase `sha256` digest, renders no attestation
bundle or attestation environment, and verifies immutability only. The operator
is responsible for verifying the source revision, build system, vulnerability
scan, and promotion policy before supplying that digest. Any non-empty GitHub
attestation field is rejected in this mode; this is not a general verification
bypass and it applies only to the governed Connections bridge. `stableBridge`
retains its existing GitHub-attestation contract.

Switch an existing installation to `operator-pinned` in two stages. First
deploy the new binaries and chart while retaining `github-attestation`, and
wait until every old apiserver and controller pod has terminated. Only then
change the configured mode and digest to `operator-pinned`. The forward schema
migration keeps old writers compatible during the first stage; an old
controller is not expected to understand operator-pinned configuration. Fresh
installations may select either mode immediately.

New operations use the new binding. Existing operations retain their persisted
execution snapshot and fail closed if current controller configuration differs;
cached status and mutation results are never reused across that boundary. A
pending OAuth URL from the previous binding remains conflicting until its real
expiry or another proven terminal transition and is never returned under the
new binding.

The browser never receives a HOP-1 token and the apiserver never calls MCP-GW
directly. OpenShell attaches MCP-GW to the one-shot runtime and obtains its
ordinary Steward Mint identity. The bridge has no inference provider, uses one
fixed provider-control grant, and is finalized through the normal controller
lifecycle. Changing a configured trust mode, image, endpoint, MCP-GW authority contract,
namespace, or runtime class does not reinterpret an existing operation; it
fails closed.

The globally bound controller and mint ClusterRoles have no Secret verbs.
Runtime Secret access is granted by namespaced Roles and RoleBindings only for
names listed in `runtimeNamespaces`. The default is an empty list, so a release
consumer must explicitly authorize every runtime namespace; namespaces outside
that allowlist remain inaccessible to both service accounts.

## Runtime configuration

- `config.apiserver.kubernetesTokenReviewAudience` is the required, non-empty
  Kubernetes API server audience used by delegated TokenReview, including the
  Task API. Its chart default
  is `https://kubernetes.default.svc`; replace it if the target cluster's
  delegated TokenReview audience differs. This is distinct from the exchanged
  JWT's `steward-task-api` audience. The Task API is enabled on the apiserver
  service; its internal port is `services.apiserverPort`.
- `config.apiserver.capabilityCatalog` is the bounded, deployment-owned model/tool
  catalog used by the template editor. The chart validates it, renders it into an
  immutable content-addressed ConfigMap, mounts it read-only, and rolls the apiserver
  when its checksum changes. It is descriptive availability data and never Task authority.

  ```yaml
  config:
    apiserver:
      capabilityCatalog:
        schemaVersion: steward.capability-catalog/v1
        models:
          - provider: openai
            model: gpt-5.4
        tools:
          - provider: github
            resource: actions_get
            action: read
  ```

  Authority, budget, TTL, template, and user fields are intentionally not part of this catalog.
- `config.apiserver.executionBindings` is the structured, deployment-owned coding-agent
  catalog. The default `bindings: []` advertises no agents and creates no fallback.
  The chart validates it, renders it into an immutable content-addressed ConfigMap,
  mounts it read-only in the apiserver, and rolls the apiserver when its checksum
  changes. See [Execution bindings](../../docs/installation/execution-bindings.md).
- `config.apiserver.inferenceEndpoint` is the OpenAI-compatible Responses endpoint for
  `codex-v1`; `config.apiserver.anthropicInferenceEndpoint` is the Anthropic-compatible API
  base URL for `claude-code-v1`. Agent images, packages, and bindings cannot override them.
- `jira.enabled` defaults to `false`. When enabled, `jiraBaseUrl`,
  `jiraProjectKey`, and `jiraAccountEmail` are required; the base URL must be
  HTTPS and the account email must correspond to the token in the existing
  Jira Secret. When disabled, the Secret projection and Jira egress are absent,
  and Jira-dependent decision operations reject without making a network call.
- `stableBridge` defaults to disabled. Enabling it requires all of a
  digest-pinned bridge image, GitHub signer identity, HTTPS GitHub source repository and
  exact source commit, controller service identity, and the public GitHub
  artifact-attestation bundle, and an exact HTTP(S) MCP-GW origin. The bundle is
  rendered into an immutable, content-addressed ConfigMap and mounted read-only
  in both apiserver and controller; it is provenance evidence, not a Secret.
  Missing or partial bridge configuration fails Helm validation, and either
  workload fails startup on an unreadable or unverified bundle.
- This chart wiring enables the session-protected stable bridge *resolver*.
  It does not invent sandbox artifact installation, a Kubernetes pod selector,
  or a direct pod copy/exec path. Enable it only after the compatible,
  immutable bridge image and provenance coordinates are available and verified.
- With `execution.enabled=false`, the controller starts with disabled sandbox
  and inference ports and does not read governed endpoint or credential
  configuration. The API does not construct a coding-agent adapter. Active
  Task orchestration and active execution bindings are rejected. With
  `execution.enabled=true`, `config.controller.litellmUrl` and
  `config.controller.openshellEndpoint` are required internal service endpoints.
- The OpenShell endpoint must use HTTPS. `openshellServerName` pins the TLS
  identity, while `secrets.openshellClient` supplies the trusted CA, client
  certificate, and private key.
- The controller projects a rotating, ten-minute Kubernetes service-account
  source credential with audience `apelogic-workload-exchange`. It sends that
  credential only as the Bearer authorization on an empty-body
  `POST /v1/workload/exchange`. The HTTPS endpoint is
  `config.controller.workloadExchangeEndpoint`; its host must exactly match
  `workloadExchangeServerName`, and the server certificate must chain to the
  `workloadExchangeTrust` ConfigMap. The exchange selects the subject,
  `openshell-api` audience, roles, signing algorithm, and at-most-120-second
  lifetime. Steward cannot request or override those claims.
- Only the exchanged access token is sent to OpenShell. It is cached in memory
  until its refresh margin, never persisted, and refreshed from the current
  source-credential file. No token is stored in a Kubernetes Secret. Missing
  source identity, exchange trust, or exchange availability stops OpenShell
  reconciliation; HTTP, ambient workstation credentials, and direct raw
  service-account authentication are unsupported. This file-source boundary is
  deployment-neutral: non-Kubernetes deployments may mount another
  platform-approved source credential without changing sandbox/runtime code.
- `config.controller.openshellRuntimeClassName` is optional. When omitted,
  OpenShell uses the cluster default runtime. When set, it must be a valid
  Kubernetes RuntimeClass name matching OpenShell's gateway-level
  `defaultRuntimeClassName`. Steward does not send a sandbox image or expose
  per-create driver/runtime overrides, so the gateway's configured image and
  runtime policy remain authoritative.
- `config.controller.openshellTaskLogMode` defaults to `off`. Set it to `full`
  to mirror task-process stdout and stderr into controller logs while the task
  runs; each record includes `runtime_uid`, `workspace`, `sandbox`, `stream`,
  and the escaped output `message`, including prompts and responses emitted by
  the task process. **Warning:** `full` logging copies task-controlled output
  without redaction and may expose prompts, responses, credentials, or other
  sensitive information to anyone who can read or retain controller logs.
- In governed mode, `config.mint.issuer`, `spiffeTrustDomain`, and
  `openshellNamespace` must be supplied for the target environment; none has
  a usable default. The issuer must also be configured in MCP-GW.
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

Both the apiserver and controller apply the embedded append-only Postgres
migration set on startup (currently through migration `0039`). They must
receive the same database URL. Review the
[installation upgrade and backup procedure](../../docs/installation/installation-guide.md#upgrade-rollback-backup-and-removal)
before upgrading; a Helm rollback does not reverse database migrations.

Every external Task must fit the authenticated canonical user's exact active,
provisioned User Envelope. The Envelope is product governance data, not a Helm
value. Direct Git packages and immutable versioned Workflows are the supported
Task paths; the removed unversioned workflow catalog has no chart setting.

Run `cargo xtask e2e-openshell-adapter` to exercise the adapter against the
exact OpenShell `v0.0.98` chart in an ephemeral kind cluster. The test verifies
authenticated TLS failures, CA and server-name validation, the
cluster-default runtime is retained in the Sandbox pod template, input/output
SHA-256 equality, and sandbox-last cleanup. This lane proves functional
execution. It does not prove a VM isolation boundary.

## Network policy

`networkPolicy.enabled` defaults to `true`. The chart denies ingress and egress
for all Steward pods, then opens only these paths:

- caller namespaces listed in `networkPolicy.apiserverIngressNamespaces` to the
  apiserver (the default is empty and therefore denies all workload callers);
- configured Kubernetes API/VPC CIDRs to the validating webhook;
- OpenShell and MCP-GW namespaces to the mint only in governed mode;
- controller to Postgres and the Kubernetes API, plus LiteLLM, OpenShell, and
  workload exchange only in governed mode;
- apiserver to Postgres and the Kubernetes API, plus Jira only when enabled;
- mint to the Kubernetes API only in governed mode; and
- all Steward components to cluster DNS.

`kubeApiCidrs`, `postgresCidrs`, and `jiraCidrs` default to empty arrays. Empty
means denied, not unrestricted, so installation values must supply the
applicable approved CIDRs. A nonempty `jiraCidrs` array does not open an egress
rule unless `jira.enabled=true`. FQDN-aware egress policy, if used by the
cluster, belongs in the deployment adapter rather than this portable
Kubernetes `NetworkPolicy` chart.

A governed distribution must explicitly list its workload namespaces, for
example `networkPolicy.apiserverIngressNamespaces: ["my-runner"]`. The legacy
`burbleNamespace` and `arcNamespace` values remain only to render older
governed values files; both default to empty and new public installations should
not use them.

## Release configuration

Public releases use:

- images: `ghcr.io/<owner>/steward:<version>-<component>`;
- chart: `oci://ghcr.io/<owner>/charts/steward:<version>`.
