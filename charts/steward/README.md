# Steward Helm chart

Current release contract: chart `0.3.13` and application `0.3.13`.

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
identity bundle. A core-only installation needs Kubernetes 1.32 or later,
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
      - { type: Exact, value: /.well-known/oauth-protected-resource }
      - { type: PathPrefix, value: /admin/api }
      - { type: PathPrefix, value: /admin/auth }
      - { type: Exact, value: /admin/connections/github/callback }
      - { type: PathPrefix, value: /admin/operator }
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

Connection start and disconnect are asynchronous: each POST returns HTTP 202
with an operation identifier and server-owned polling deadline. The browser
polls an owner-scoped resource until authorization can continue or disconnect
finishes. It uses a short, locally bounded observation grace after the execution
deadline so the durable deadline result can be read, then stops with a retry
action. Neither mutation requires a long-lived edge request while its governed
runtime starts.
Rerun may
still spend up to 40 seconds waiting for the governed operation, so an edge
that exposes rerun should allow at least 60 seconds (60–90 seconds is the
recommended operator range). Configure that timeout on the platform-owned
Gateway policy because the portable Steward chart does not own the
controller-specific policy.

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
the staged deployment. Treat this as stage 1; `active` is stage 2. A configured Connections bridge
may still serve its runtime-free status read in stage 1, but authorize, reauthorize, disconnect, and
rerun mutations are deliberately refused with `connections.orchestration_not_active`. Verify that
GitHub Connect works only after every replica is in stage 2.

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
name/key for the Google client secret. The organization ID is a Steward-chosen
stable namespace, not a Google, cloud, or identity-provider organization ID. It
must be 5–64 characters, start with `org_`, and contain only lowercase ASCII
letters, digits, `_`, or `-` (for example, `org_example`). The chart never
creates the Secret or a public edge. A deployment adapter supplies the HTTPS
route, certificate and network policy appropriate to its platform (for
example, a local Kind adapter or a DEV gateway); those controls do not belong
in this portable chart.
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
variable. Steward uses the App to resolve exact Git objects; it does not
accept caller-uploaded package bytes. Starting in 0.3.13, the apiserver
also uses it to list the admitted source repositories for the browser: with
at least one binding, a blank `GET /app/api/v1/github/repositories` query
resolves each distinct bound source repository ID through the App, using a
token scoped to that repository with only `metadata: read` and revoked after
its one read, and caches the result in process for 10 minutes, revalidating it
in the background. The App installation must include each
admitted repository; it needs no additional permission. A repository it cannot
resolve is omitted and counted in the `x-steward-unresolved-repositories`
response header; a failed refresh keeps a previously resolved entry for up to
20 minutes (counted as unresolved), and a definitive rejection removes it. Only
when none has ever resolved does the listing return HTTP 503 with reason
`source_app_unavailable`. This lists only operator-configured repositories to
authenticated users and grants nothing; publication and dispatch still use the
governed connection.

`githubSource.bindings` authorizes exact caller-to-source repository pairs by
stable GitHub owner and repository IDs. The chart renders that non-secret
catalog into an immutable content-addressed ConfigMap and rolls the apiserver
when it changes. Repository names remain audit metadata and cannot substitute
for these IDs. The deployment adapter resolves and maintains the approved
GitHub API CIDRs; the portable chart opens HTTPS egress only to those entries.

An in-repository Task package therefore requires all of the following before
submission: `githubSource.enabled=true`, a read-only Contents GitHub App
installed on the invoking repository, the referenced Secret, and nonempty
`networkPolicy.githubApiCidrs` when NetworkPolicy is enabled. A package in the
invoking repository needs no cross-repository binding. A package in another
repository additionally needs its exact caller/source ID pair in
`githubSource.bindings`. When protected-resource discovery is configured, it
advertises the resulting capability as `steward_direct_packages_supported` and
advertises same-repository two-file invocation as
`steward_package_path_supported`. steward-run 0.8.0 or later requires the
second field before it submits `package-path`; when either underlying
capability is false, a direct submission returns
`task.direct_package_source_disabled` without reserving a Task.

## Governed provider connections

`connectionsBridge` is disabled by default. Enabling it requires browser
authentication, an immutable bridge image, an explicit artifact-trust contract,
the exact MCP-GW origin, and a dedicated runtime namespace. Authority v1 also
requires the named `connectionsBridge.mcpGatewayAuthorityContract` selector:
`steward.connections.github/v1` for the legacy status route or
`steward.connections.github/v2` for the lifecycle status contract used by
MCP-GW 0.4.9 through 0.5.5. The deprecated `mcpGatewayVersion` input remains
available for an existing values file and must not be set together with the
named selector. Another configured contract fails closed. The apiserver records
the frozen internal authority snapshot on each operation; the controller
verifies the same snapshot before creating or executing the short-lived
`steward-connections` runtime.

Steward automatically selects internal authority v4 for browser GitHub repository
automation. It adds only the operation-specific grants needed to list repositories,
read the generated caller workflow, publish the exact two-file package on a new branch,
dispatch that caller, and observe its run. Operators do not select v4 in product
configuration. Repository publication additionally requires the target repository's
stable IDs to appear as a source in `githubSource.bindings`, a reviewed `steward-run`
release at v0.8.0 or later, and GitHub OAuth App access to the repository. Upgrading
adds migration 0064; existing v1-v3 governed connection rows remain valid.

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

The browser never receives a HOP-1 token. For display-only connection status,
the apiserver mints a short-lived `connections_status` control-plane token and
makes one metadata-only authenticated `GET /connections/github/status` request
to the private MCP-GW origin with a one-second deadline. Configure apiserver
egress and MCP-GW routing for that private call. Connect, reauthorize,
disconnect, and rerun mutations still execute through the one-shot governed
OpenShell bridge runtime. The bridge has no inference provider, uses one fixed
provider-control grant, and is finalized through the normal controller
lifecycle. Changing a configured trust mode, image, endpoint, MCP-GW authority
contract, namespace, or runtime class does not reinterpret an existing
operation; it fails closed.

With MCP-GW 0.5.5+, Steward requests connection-status v2 and displays the
GitHub login plus immutable numeric account ID. When federated Task identity is
enabled, `taskIdentity.federatedSubjects.autoAssociateFromConnections=true`
(the default) uses only that numeric ID to associate
`github-actions:actor:<id>` with the signed-in canonical user. Login, display
name, and email are never identity keys. Set the value to `false` to retain
manual association. Disconnecting GitHub does not remove the association;
administrators revoke it explicitly by disabling the federated subject.

### Troubleshooting a failed Connect operation

The Connect API returns a bounded error body. `gateway_http_error` includes
the upstream HTTP status, a safe MCP-GW JSON `code` of at most 100 bytes, and a
sanitized human `error` detail of at most 200 bytes when those fields are
present. Steward does not assume the upstream body is an RFC 9457 problem
document. It never copies a token, an unselected response field, or a URL
containing a query string into that response. The same evidence is retained in
`connection_operations.failure_category` and
`connection_operations.failure_detail`; the corresponding failed
`task_execution_attempts.execution_stderr` contains the bridge's fixed,
bounded diagnostic.

| Failure category | Meaning |
| --- | --- |
| `bridge-gateway-http` | MCP-GW returned a status other than the operation's exact success status (200 for status/start/rerun; 204 for disconnect), after the dedicated 401/403 and token-grant classifications. Read `failure_detail` for the status, stable code, and optional sanitized reason. |
| `bridge-runtime-authentication` | MCP-GW rejected the runtime credential. Re-authorize the connection, then verify runtime credential injection if it continues. |
| `bridge-proxy-policy` | OpenShell denied the provider request before MCP-GW handled it. |
| `bridge-runtime-authorization` | MCP-GW rejected the runtime's authority. |
| `bridge-token-grant` | OpenShell could not exchange the placeholder for the runtime-bound GitHub credential. Retry once, then inspect MCP-GW token-grant health. |
| `bridge-contract` | The bridge rejected the request Steward sent it: the invocation, the operation allowlist, or the operation's `request.json` contract. This indicates mismatched apiserver and bridge versions or a Steward defect, not a provider failure. Deploy the apiserver and Connections bridge from the same release. |
| `bridge-response-contract` | MCP-GW returned a response that did not satisfy Steward's pinned provider contract. A dispatch tool error reporting the workflow or ref as not found (or already existing) is also reported here: it is a definite rejection, not a queued run or an outage to retry. |
| `bridge-gateway-transport` | The governed runtime could not complete the transport request to MCP-GW. |
| `bridge-gateway-status` | Historical category from older bridges for an unexpected successful disconnect response. New invalid response bodies use `bridge-response-contract`. |
| `bridge-gateway-body` | The governed runtime could not read the bounded MCP-GW response body. |
| `bridge-gateway-unavailable` | MCP-GW was unavailable to the governed runtime. |
| `runtime_create_admission_rejected` | Kubernetes admission rejected creation of the governed connection runtime. Inspect Steward admission and controller events. |
| `runtime_start_failed` | The exact governed connection runtime entered its terminal failed phase before it became ready. Inspect the AgentRuntime and OpenShell sandbox. |
| `bridge_failed` | The bridge failed without a recognized safe diagnostic. |
| `deadline_exceeded` | The governed connection operation did not finish before its response deadline. |
| `invalid_bridge_result` | The bridge exited successfully but its output violated the fixed result contract. |
| `bridge_result_too_large` | The bridge exited successfully but its output exceeded Steward's bound for that operation's result (128 KiB for a repository listing or a run status, 32 KiB otherwise). This is Steward's own limit, not a provider contract failure. |

For a start failure whose reason says the OAuth redirect target is not allowed,
configure MCP-GW `redirectAfterAllowedOrigins` with Steward's exact public
origin. Do not add a wildcard or copy the one-time authorization URL into
configuration or logs.

The globally bound apiserver, controller, and mint ClusterRoles have no Secret
verbs and no Kubernetes user or group impersonation authority. The apiserver
and controller write AgentRuntime resources as their own exact service-account
identities. The validating webhook recognizes only those configured writers,
then independently enforces principal immutability, the bound member-role
annotation, Envelope and grant limits, and immutable authority before admitting
the write. A compromised Steward pod therefore cannot use its RBAC to
impersonate `system:masters` or another cluster principal.
Runtime Secret access is granted by namespaced Roles and RoleBindings only for
names listed in `runtimeNamespaces`. The default is an empty list, so a release
consumer must explicitly authorize every runtime namespace; namespaces outside
that allowlist remain inaccessible to both service accounts.

The controller creates Workflow Task runtimes in the fixed
`steward-workflows` namespace. That namespace must exist and must be included
in `runtimeNamespaces`, in addition to any Connections/runtime namespace. The
platform preflight emits both entries and never assumes that creating the
Steward release namespace also creates `steward-workflows`.

## Runtime configuration

- `config.apiserver.kubernetesTokenReviewAudience` is the required, non-empty
  Kubernetes API server audience used by delegated TokenReview, including the
  Task API. Its chart default
  is `https://kubernetes.default.svc`; replace it if the target cluster's
  delegated TokenReview audience differs. This is distinct from the exchanged
  JWT's `steward-task-api` audience. The Task API is enabled on the apiserver
  service; its internal port is `services.apiserverPort`.
- `taskIdentity.enabled=true` selects direct Identity-issued ES256 Task tokens
  instead of Kubernetes TokenReview. With `federatedSubjects.enabled=false`
  (the default), Steward accepts only `steward-task-v2`; `resource` may remain
  empty and no discovery document is published. To opt into
  `steward-task-v3`, set the exact public Steward origin in `resource` and set
  `federatedSubjects.enabled=true`:

  ```yaml
  taskIdentity:
    enabled: true
    issuer: https://identity.example.test
    audience: steward-task-api
    resource: https://steward.example.test
    federatedSubjects:
      enabled: true
    publicJwksConfigMap:
      name: steward-task-identity-jwks
      key: jwks.json
  ```

  `GET /.well-known/oauth-protected-resource` then advertises that resource,
  the exact issuer, bearer authentication, and accepted `steward-task-v2` and
  `steward-task-v3` contracts. A verified v3 subject is recorded but receives
  no authority until an administrator associates it with an existing active
  canonical user. User Envelope admission remains unchanged.
- `config.apiserver.capabilityCatalog` is the bounded, deployment-owned model/tool
  catalog used by the template editor. The chart validates it, renders it into an
  immutable content-addressed ConfigMap, mounts it read-only, and rolls the apiserver
  when its checksum changes. It is descriptive availability data and never Task authority.

  ```yaml
  config:
    apiserver:
      capabilityCatalog:
        schemaVersion: steward.capability-catalog/v2
        models:
          - provider: openai
            model: gpt-5.4
        tools:
          - provider: github
            resource: actions_get
            action: read
            accessClass: read
            # Unreleased presentation metadata.
            toolsets: [actions]
        catalogs:
          - provider: github
            catalogId: github-tools
            version: 1.6.0
            available: true
  ```

  Tool access classes and provider catalog availability are display metadata. Authority,
  budget, TTL, template, and user fields are intentionally not part of this catalog.

  **Unreleased:** optional `toolsets` are also presentation metadata. A tool may belong to at
  most 16 toolsets. The template editor groups tools only when these authoritative names are
  supplied; it does not infer groups from tool names or provider conventions. Existing v2
  entries without `toolsets` remain valid and appear in a deterministic fallback group.
  Steward does not generate this metadata from an MCP-GW release asset; the deployment owner
  supplies it with the catalog.
- `config.apiserver.customEnvelopeSafetyCeiling` is the optional deployment-owned maximum for
  template-free requests. It is a complete Envelope and its capabilities must exist in the
  catalog. Steward revalidates the current ceiling at both request creation and administrator
  approval, so tightening it also fences already-pending requests. The default `null` value makes
  custom requests fail closed.
- `config.apiserver.stewardRunRelease` is the exact verified steward-run handoff selected by the
  installation BOM: manifest schema, semantic version, reusable-workflow repository and commit,
  and action commit. The deprecated `governedJobContainerImage` coordinate remains accepted for
  compatibility but is not used by the active versioned workflow generator. The default `null`
  value is valid while browser
  administration is disabled. Enabling browser administration requires a complete v0.7.0-or-later
  object and fails schema validation when the value is absent, null, or malformed. Steward validates
  and renders these deployment coordinates; its source and chart defaults do not select them.
- `config.apiserver.stewardRunWorkflowInstallationMode` controls only how generated callers
  reference that verified release. `remote` is the default and renders the release handoff's exact
  repository and workflow commit. `vendored` renders
  `./.github/workflows/steward-task-vendored.yml`; select it only after each caller repository has
  installed the checksum- and signature-verified steward-run v0.7.6-or-later release asset at that
  exact path. Older release handoffs fail closed in this mode. The local mode does not add a PAT,
  checkout token, or mutable action reference. See
  [the installation guide](../../docs/installation/installation-guide.md#vendored-steward-run-workflow).
- `config.apiserver.executionBindings` is the structured, deployment-owned coding-agent
  catalog. The default `bindings: []` advertises no agents and creates no fallback.
  The chart validates it, renders it into an immutable content-addressed ConfigMap,
  mounts it read-only in the apiserver, and rolls the apiserver when its checksum
  changes. See [Execution bindings](../../docs/installation/execution-bindings.md).
- `config.apiserver.starterTask` is the optional deployment-owned default shown by
  **Get started** and **Run now**. It is a complete inline TaskDefinition plus its
  default inputs, execution-log choice, repository package path, and optional Git and
  published-Workflow examples. The configured `agentRef` must be advertised by
  `executionBindings`. Omitting `requires` keeps the existing behavior of deriving
  authority from the selected User Envelope. The default `null` value uses Steward's
  built-in Hello World task.

  ```yaml
  config:
    apiserver:
      starterTask:
        taskDefinition:
          schemaVersion: steward.task-definition/v2
          name: hello-world
          version: 1
          runtime:
            agentRef: codex@0.140.0
          promptText: >-
            Write the single line hello world to $STEWARD_OUTPUT_DIR/out/hello.txt
            using your shell, for example mkdir -p "$STEWARD_OUTPUT_DIR/out" &&
            printf 'hello world\n' > "$STEWARD_OUTPUT_DIR/out/hello.txt". Do not
            create any other files, do not use the network, and do not call any MCP
            or GitHub tools.
          outputs:
            - {path: out/hello.txt, kind: file, required: true}
        inputs: {}
        executionLog: "off"
        packagePath: .steward/tasks/hello-world/task-definition.json
        title: Hello world
        description: Run a governed coding agent and collect its exact output.
        git:
          repository: https://github.com/example-org/agentic-ops.git
          revision: git:ref:main
          path: catalog/hello/task-definition.json
        publishedWorkflow: hello-world@1
  ```

  `packagePath` is the canonical path used when the inline package is saved or
  published. Steward validates this setting at apiserver startup using the ordinary
  package and browser-input limits; invalid configuration fails startup. The browser
  reads the effective value from its authenticated, no-store onboarding endpoint, so
  a reviewed GitOps values change updates the pod template and rolls the apiserver;
  no Steward image rebuild is required.
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
- `spire.className` is required in governed mode and must exactly match the
  class watched by the installed SPIRE controller manager. `spire.csiDriver`
  and `spire.socketPath` mount the SPIFFE Workload API only in the mint pod.
  The chart creates class-bound `ClusterSPIFFEID` resources for Mint and, by
  default, stock OpenShell v0.0.98 sandboxes. The sandbox registration selects
  `networkPolicy.openshellNamespace`, the `openshell.ai/managed-by: openshell`
  pod label, and the `openshell.io/sandbox-id` annotation to issue
  `spiffe://<trust-domain>/openshell/sandbox/<sandbox-id>`. Set
  `spire.sandboxRegistration.enabled=false` only when the platform owns an
  equivalent registration. The OpenShell installation still owns
  `server.providerTokenGrants.spiffe` and its Workload API socket setting.

Both the apiserver and controller apply the embedded append-only Postgres
migration set on startup (currently through migration `0063`). They must
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

The supported stock OpenShell v0.0.98 deployment uses the sidecar supervisor
with process-binary-aware network policy. When provider token grants are
enabled, the Steward chart supplies the sandbox `ClusterSPIFFEID`; configure
the Workload API socket in OpenShell and use the `openshell.io/sandbox-id`
annotation contract. See
[OpenShell 0.0.98 governed execution](../../docs/installation/openshell-v0.0.98.md)
for the exact values, Kubernetes 1.35 sideload setting, and diagnostic command.

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
