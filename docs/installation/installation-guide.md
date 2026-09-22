# Steward installation guide

Release contract: chart `0.1.22`. The release workflow pulls the published OCI
chart and every published component image by digest, renders the complete chart,
and installs the core profile into a clean disposable cluster before creating
the GitHub release. Use chart and image digests from the same release handoff.

## Choose the installation mode

| Mode | What starts | Additional prerequisites |
|---|---|---|
| Core (default, `execution.enabled=false`) | API, admission webhook, and AgentRuntime controller with Task orchestration staged | None of Jira, an inference endpoint, LiteLLM, OpenShell, SPIRE, a Mint Secret, or a RuntimeClass |
| Governed execution (`execution.enabled=true`) | Core plus Mint, OpenShell reconciliation, LiteLLM inference, and workload identity | OpenShell gateway/client mTLS, LiteLLM, workload exchange, SPIRE CSI and ClusterSPIFFEID, Mint signing material and trust |

Jira is a separate opt-in decision-channel integration (`jira.enabled=true`) in
either mode. With Jira disabled, decisions that need it fail closed; the chart
does not mount its Secret or grant Jira egress. Core mode is for a **new**
installation, not a switch that cleans up existing governed sandboxes. Do not
turn execution off on an installation with live AgentRuntimes or Tasks.

## Prerequisites

1. Kubernetes 1.30 or newer, Helm 3.17.0 or newer, `kubectl`, and cluster-admin
   authority for the Steward CRD, cluster roles, and validating webhook. Select
   an explicit kubeconfig and context; do not use an ambient or unrelated cluster.
2. A reachable, separately operated PostgreSQL database and an existing
   `steward-database` Secret with key `url` in the installation namespace.
   PostgreSQL 16 is the tested line. Give a dedicated database role `CONNECT`
   plus the schema DDL/DML authority needed to run Steward's embedded,
   append-only migrations; both binaries migrate on startup, so a read-only
   application role is insufficient. Use a URI with the customer's required
   TLS verification mode and CA location, stored only in the Secret file.
   Steward creates no database, PVC, backup, or database credential.
3. Immutable apiserver and controller images from the same source revision as
   the chart. Supply their exact repository, tags, and `sha256` digests from a
   verified release handoff. If using a fork, build and publish to a registry
   the cluster can pull, then set `images.repository` to that fork-owned path.
   Configure `imagePullSecrets` if the registry is private. For governed mode,
   also supply the Mint image; for the optional web UI, supply its image. The
   fork release workflow publishes to `ghcr.io/<fork-owner>/steward` and
   `oci://ghcr.io/<fork-owner>/charts/steward` from a validated version tag;
   do not substitute an upstream owner's coordinates in a customer handoff.
4. HTTPS service certificates for `steward-apiserver` and `steward-webhook`.
   Choose exactly one chart TLS mode:
   - `customerSecret` (default): pre-create the two named `kubernetes.io/tls`
     Secrets and supply the **public** CA bundle that verifies the webhook
     service certificate with `--set-file tls.webhook.caBundlePem=...`. The
     certificate must cover the service DNS names shown in the chart's
     `Certificate` templates, including the chosen namespace and cluster
     domain. An empty bundle is rejected before installation.
   - `certManager`: install cert-manager and an approved `Issuer` or
     `ClusterIssuer` first; set `tls.mode=certManager` and the exact
     `tls.issuerRef`. Steward creates two `Certificate` resources, not an
     issuer. cert-manager must write the named TLS Secrets, and its CA injector
     must populate the webhook before admission traffic is sent.
5. Review network policy for the target CNI. The chart enables NetworkPolicy
   and denies unspecified egress. Set the approved API-server and PostgreSQL
   CIDRs/ports under `networkPolicy`; do not disable policy merely to make a
   failed readiness check green. Edge routing, DNS, ingress/Gateway, external
   authentication, and their certificates are operator-owned opt-ins.

For governed execution, add the OpenShell gateway URL/server name/client
certificate Secret, LiteLLM URL/master-key Secret,
workload exchange URL/server name/public CA projection, SPIRE CSI driver and
`ClusterSPIFFEID` API, and the Mint Secret. OpenShell uses the cluster default
runtime unless the operator supplies the optional RuntimeClass override.
This release proves functional sandbox separation and makes no VM-isolation
claim. See [chart configuration](../../charts/steward/README.md)
and [execution bindings](execution-bindings.md) before activating Tasks.

### Tested versions and integration boundaries

These are the versions exercised or declared by this repository, not a promise
that every other version works. Pin each external product and prove its contract
again in the customer's cluster before enabling governed execution.

| Component | Supported / tested now |
|---|---|
| Kubernetes | Chart requires 1.30+; release acceptance uses Kind 1.32.1. |
| Helm | 3.17+; release acceptance uses 3.17.1. |
| PostgreSQL | 16. |
| OpenShell | 0.0.98. |
| agent-sandbox | 0.5.0. |
| Runtime | Cluster/OpenShell default; functional isolation only, no VM claim. |
| MCP-GW | Authority v1: 0.3.2; authority v2: 0.4.9. |
| SPIRE, LiteLLM, cert-manager, browser identity, GitHub, edge gateway | Operator-supplied; record the exact version tested for the selected deployment. |

Runtime support is the Kubernetes/OpenShell default. Operators may set
`config.controller.openshellRuntimeClassName` only when their platform requires
an explicit class. That optional override is deployment configuration, not a
separate Steward-supported runtime or an isolation certification.

Integration ownership is explicit: core requires only PostgreSQL, Kubernetes
TokenReview/API access, and service TLS; Jira adds a decision channel; browser
OIDC adds an external identity provider and edge; GitHub source adds a read-only
GitHub App; task identity adds an external issuer/public JWKS; governed execution
adds OpenShell, SPIRE, workload exchange, LiteLLM, Mint,
and optionally MCP-GW/provider profiles. The stable and Connections bridges are
independent opt-ins with immutable images and provenance contracts. See the
[chart configuration](../../charts/steward/README.md) for exact flags and
ingress/egress requirements.

### External identity and integration ownership

Do not reuse one credential for these unrelated boundaries:

| Integration | Required when | Owner, minimum authority, and verification |
|---|---|---|
| GitHub Actions OIDC → identity exchange → `steward-run` | Governed submission from Actions | Runner operator grants `id-token: write`; the exchange trusts `https://token.actions.githubusercontent.com`, repository/ref policy, and audience `steward-task-api`. The exchanged short-lived identity must carry the exact Steward service-envelope or submission groups and pass TokenReview. This is not a GitHub OAuth App. Install the independent runner and exchange from [steward-run#41](https://github.com/apelogic-ai/steward-run/issues/41) and [github-oidc-exchange#38](https://github.com/apelogic-ai/github-oidc-exchange/issues/38); they are not subcharts. Verify issuer, audience, HTTPS CA, and one denied wrong-repository/ref request before submission. |
| ARC GitHub App | Only an ARC-based runner installation | Runner-platform owner supplies the App ID, installation ID, and key with the minimum ARC repository/organization permissions. Steward neither reads nor creates this credential. Verify runner registration and job pickup in that product's handoff. |
| Read-only GitHub source App | `githubSource.enabled=true` direct packages | Steward source operator supplies the configured App ID and PEM Secret; install it only on approved repositories with read-only Contents. Verify resolution of one exact allowed commit and denial of an unbound repository. |
| Google OAuth/OIDC client | `browserAuth.enabled=true` | Browser-identity owner supplies the client ID/Secret, allowed workspace/organization, and the exact HTTPS callback derived from `browserAuth.google.origin`. Verify login, callback, wrong-domain denial, and logout. |
| MCP-GW downstream OAuth clients | Only selected MCP tools | MCP-GW operator owns Google/GitHub/provider consent clients, callbacks, and stored grants. They never go in the Steward chart. Verify consent and subject isolation through MCP-GW. |
| Jira Cloud API token | `jira.enabled=true` only | Jira project owner supplies a dedicated account/token permitted to search/browse, create Task issues, and comment in the configured project. Verify create/search/comment and revocation. With Jira disabled, verify no Secret projection or Jira egress. |

Service and User Envelopes, approval decisions, and execution bindings are
post-install governance data. Do not bake a User Envelope revision or a bearer
credential into Helm values. Provision the authority-minimal `steward-run`
Service Envelope with [`scripts/bootstrap-task-copy-smoke.sh`](../../scripts/bootstrap-task-copy-smoke.sh)
over authenticated HTTPS and a short-lived route-scoped identity, then review
and approve any wider envelope through the selected decision channel.

## Secret and integration inventory

The chart references existing names and keys; it never puts secret bytes in
values. Manage creation and rotation with the customer's approved secret
system. The default names can be overridden under `secrets`, `tls`,
`browserAuth`, and `githubSource`.

| Reference and namespace | Kubernetes type and keys | Producer and consumer | Rotation / condition |
|---|---|---|---|
| `secrets.database.name` (`steward-database`) in release namespace | `Opaque`; `secrets.database.key` (`url`) | Database operator creates; API and controller read. | Always. Rotate the database credential and restart both Deployments after the new Secret is present; verify connectivity and migrations. |
| `tls.api.secretName` and `tls.webhook.secretName` in release namespace | `kubernetes.io/tls`; both `tls.crt`, `tls.key` | Customer PKI or cert-manager creates; API and controller mount separately. | Always. Renew before expiry, verify service DNS SANs, CA chain, and webhook `caBundle`; roll the affected Deployment. |
| `secrets.jira.name` (`steward-jira`) in release namespace | `Opaque`; `secrets.jira.key` (`token`) | Jira operator creates; API and controller read only if `jira.enabled=true`. | Optional. Use a Jira Cloud API token with a dedicated account allowed to browse/search, create Task issues, and add comments in the configured project. Rotate the token, restart consumers, and prove a decision; absent when disabled. |
| `secrets.litellm.name` (`steward-litellm`) in release namespace | `Opaque`; `secrets.litellm.key` (`master-key`) | LiteLLM operator creates; controller reads. | Governed mode only, including model-free copy-smoke startup. Coordinate credential overlap/restart with LiteLLM. |
| `secrets.openshellClient.name` (`steward-openshell-client`) in release namespace | `Opaque`; configured CA, client certificate, and private-key keys (`ca.crt`, `tls.crt`, `tls.key`) | OpenShell/customer PKI creates; controller mounts. | Governed mode only. Rotate as an mTLS bundle and reprove server-name/CA validation. |
| `secrets.mint.name` (`steward-mint`) in release namespace | `Opaque`; configured `signing-key`, `introspection-credential` | Customer key authority creates; Mint mounts. The signing key is exactly 32 raw bytes; newline-terminated or hex text is invalid. | Governed mode only. Coordinate JWKS/key rollover and introspection credential overlap with consumers. |
| `workloadExchangeTrust.name` (`steward-workload-exchange-ca`) in release namespace | Public `ConfigMap` by default (or explicitly selected `Secret`); `workloadExchangeTrust.caCertificate` (`ca.crt`) | Workload-exchange PKI creates; controller mounts. | Governed mode only; rotate with exchange TLS and reprove trust. |
| `taskIdentity.publicJwksConfigMap.name` in release namespace | Public `ConfigMap`; configured JWKS key | External Identity operator creates; API reads. | Only `taskIdentity.enabled=true`; rotate with issuer overlap and reprove issuer/audience/signature. |
| `browserAuth.google.clientSecret.name` in release namespace | `Opaque`; configured `clientSecret.key` | Identity-provider operator creates; API reads. | Only `browserAuth.enabled=true`; rotate with provider, restart API, and retest the exact HTTPS callback/origin. |
| `githubSource.privateKeySecret.name` in release namespace | `Opaque`; configured private-key key containing the GitHub App PEM | GitHub App owner creates; API mounts read-only. | Only `githubSource.enabled=true`; App needs read-only Contents and installation only on approved repositories. Rotate the App key and retest exact Git-object resolution. |
| `web.ingress.tlsSecretName` in release namespace | `kubernetes.io/tls`; `tls.crt`, `tls.key` | Customer edge PKI creates; Ingress controller reads. | Only legacy `web.ingress.enabled=true`; gateway-owned TLS stays outside this chart. |
| Each `imagePullSecrets` reference in release namespace | Normally `kubernetes.io/dockerconfigjson`; `.dockerconfigjson` | Registry operator creates; kubelet reads. | Only private registries; rotate before expiry and verify pulls without printing the Secret. |
| Runtime-UID-named Secret in each allowed runtime namespace | `Opaque`; `access-token` | Controller creates from a runtime-scoped LiteLLM credential; sandbox consumes. | Governed runtime only. It is UID-bound and owner-referenced; controller deletes it on suspend/termination. Do not pre-create, back up as a reusable credential, or share across runtimes. |

The optional task identity JWKS is a **public ConfigMap**, not a Secret.
Bridge attestation bundles and the customer webhook CA are public material.
Do not print or commit Secret values, database URLs, private keys, tokens, or
credential-bearing Helm values.

Before install, check object type and key *presence* without disclosing data.
For example, with `jq` installed, a customer-supplied TLS Secret can be checked
as follows; repeat for every enabled row above, using its configured name/key.
For `certManager` mode, check the issuer first and check generated TLS Secrets
after the Certificates become Ready.

Create/import enabled Secrets only from protected files. These commands do not
print values; keep shell tracing disabled, never put values directly on a command
line, and securely remove local copies according to the customer's media policy
after the cluster secret manager has taken ownership:

```sh
set +x
umask 077
install -d -m 0700 ./private
openssl rand 32 > ./private/mint-ed25519.seed
openssl rand -hex 24 | tr -d '\n' > ./private/mint-introspection.txt

kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward create secret generic steward-database \
  --from-file=url=./private/postgres-url.txt --dry-run=client -o yaml | \
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" apply -f -
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward create secret generic steward-mint \
  --from-file=signing-key=./private/mint-ed25519.seed \
  --from-file=introspection-credential=./private/mint-introspection.txt \
  --dry-run=client -o yaml | \
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" apply -f -
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward create secret generic steward-openshell-client \
  --from-file=ca.crt=./private/openshell-ca.crt \
  --from-file=tls.crt=./private/openshell-client.crt \
  --from-file=tls.key=./private/openshell-client.key \
  --dry-run=client -o yaml | \
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" apply -f -
```

The same file-only pattern applies to the optional Jira token, LiteLLM master
key, Google client secret, GitHub App PEM, and registry config. Customer-PKI TLS
uses `kubectl create secret tls` for each endpoint. Install public trust
separately as a ConfigMap. For example:

```sh
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward create secret tls steward-apiserver-tls \
  --cert=./private/apiserver.crt --key=./private/apiserver.key \
  --dry-run=client -o yaml | \
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" apply -f -
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward create configmap steward-workload-exchange-ca \
  --from-file=ca.crt=./public/workload-exchange-ca.crt \
  --dry-run=client -o yaml | \
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" apply -f -
```

Repeat the TLS command for `steward-webhook-tls`. cert-manager mode creates
those two endpoint TLS Secrets from the selected issuer; it does not create
database, Mint, OpenShell, or integration Secrets.

```sh
set +x
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward get secret steward-apiserver-tls -o json |
  jq -e '.type == "kubernetes.io/tls" and
         ((.data["tls.crt"] // "") | length > 0) and
         ((.data["tls.key"] // "") | length > 0)' >/dev/null
kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
  -n steward get secret steward-database -o json |
  jq -e '((.data.url // "") | length > 0)' >/dev/null
```

This checks only key presence, not credential validity, certificate trust, or
successful integration. Never include `-o yaml`, decoded Secret bytes, or
credential-bearing URLs in the delivery record. A missing or empty required
key blocks installation; disabling that feature is a separate, reviewed choice.

## Install core mode

### Build and publish from a fork

The fork release workflow is the tested publisher. Review the exact commit,
update chart/app versions coherently, run `cargo xtask ci` and
`scripts/validate-release-artifacts.sh`, and create a signed `vX.Y.Z` tag on a
commit already contained in the fork's `main`. The workflow builds every
component, produces SBOM/provenance attestations, publishes images to
`ghcr.io/<fork-owner>/steward`, and publishes the chart to
`oci://ghcr.io/<fork-owner>/charts/steward`. AWS/ECR promotion is optional and
disabled unless the fork explicitly configures it.

Before installation, verify the workflow succeeded, copy the image manifest
digests and OCI chart digest from its handoff, verify attestations against the
fork repository/tag workflow identity, and pull the chart by digest into a
clean directory. Never reconstruct a digest from a tag or mix components from
different commits. If the operator uses another registry, copy the exact OCI
manifests by digest, verify the destination digests are unchanged, and update
only the fork-owned repository coordinates in the values file.

Pull the chart through its immutable OCI manifest digest, verify Helm resolved
that same digest, and retain the resulting local archive for lint, render, and
install. Replace the repository and digest with the exact release handoff; do
not substitute a tag-only reference:

```sh
STEWARD_CHART_REPOSITORY=ghcr.io/<fork-owner>/charts/steward
STEWARD_CHART_DIGEST=sha256:<64-hex-digest>
STEWARD_CHART_DIRECTORY="$(mktemp -d)"
STEWARD_CHART_REF="oci://${STEWARD_CHART_REPOSITORY}@${STEWARD_CHART_DIGEST}"

pull_output="$(
  helm pull "${STEWARD_CHART_REF}" --destination "${STEWARD_CHART_DIRECTORY}" 2>&1
)"
printf '%s\n' "${pull_output}"
resolved_chart_digest="$(
  printf '%s\n' "${pull_output}" |
    awk '$1 == "Digest:" { print $2 }'
)"
test "${resolved_chart_digest}" = "${STEWARD_CHART_DIGEST}"

STEWARD_CHART_PACKAGE="$(find "${STEWARD_CHART_DIRECTORY}" -maxdepth 1 -type f -name 'steward*.tgz' -print -quit)"
test -s "${STEWARD_CHART_PACKAGE}"
```

Keep `STEWARD_CHART_PACKAGE` in the same shell for the remaining commands.
The supported release procedure installs this pulled archive; a source checkout
or locally built image is not release evidence.

1. Record the chart OCI digest and every component image digest from the same
   release handoff. Set a
   target-specific values file, for example:

   ```yaml
   images:
     repository: registry.example.com/customer/steward
     apiserver: {tag: release-apiserver, digest: sha256:<64-hex-digest>}
     controller: {tag: release-controller, digest: sha256:<64-hex-digest>}
     mint: {tag: "", digest: ""}
     web: {tag: "", digest: ""}
   execution: {enabled: false}
   jira: {enabled: false}
   networkPolicy:
     kubeApiCidrs: [<approved-api-cidr>]
     postgresCidrs: [<approved-postgres-cidr>]
   ```

   Keep `config.taskOrchestrationMode` and
   `config.apiserver.executionBindingsMode` at `staged`. The tags above are
   illustrative; replace the apiserver and controller coordinates with the
   immutable handoff. Governed mode also requires Mint coordinates. Keep
   Secret bytes out of the values file.

   For governed execution, start with both ownership switches staged and add
   all external coordinates explicitly. A minimal profile has this shape;
   replace every example with the customer handoff and do not activate Tasks
   until the prerequisites and execution binding have been verified:

   ```yaml
   execution: {enabled: true}
   jira: {enabled: false}
   images:
     repository: registry.example.test/customer/steward
     apiserver: {tag: release-apiserver, digest: sha256:<64-hex-digest>}
     controller: {tag: release-controller, digest: sha256:<64-hex-digest>}
     mint: {tag: release-mint, digest: sha256:<64-hex-digest>}
     web: {tag: "", digest: ""}
   config:
     taskOrchestrationMode: staged
     apiserver:
       executionBindingsMode: staged
       inferenceEndpoint: https://inference.example.test/v1
     controller:
       openshellEndpoint: https://gateway.example.test:8080
       openshellServerName: gateway.example.test
       workloadExchangeEndpoint: https://identity.example.test/v1/workload/exchange
       workloadExchangeServerName: identity.example.test
       litellmUrl: https://litellm.example.test
     mint:
       issuer: https://mint.example.test
       spiffeTrustDomain: customer.example.test
       openshellNamespace: customer-openshell
   runtimeNamespaces: [steward-tasks]
   ```

   The chart references the existing `steward-litellm`,
   `steward-openshell-client`, `steward-mint`, and
   `steward-workload-exchange-ca` objects named above. Install the matching
   OpenShell provider profiles outside this chart. For the product-owned
   versioned bundle, create a deployment-neutral inputs file, then run
   `cargo xtask provider-profile-bundle install --inputs <inputs.json>
   --output <rendered-directory>` followed by the matching `reconcile` command
   from the [bundle guide](../../config/provider-profile-bundle/v1.2.0/README.md).
   Record each installed profile ID and immutable policy digest in the
   [execution binding](execution-bindings.md); a model-free copy task attaches
   neither tool nor inference profile, while an approved model/tool requires
   the corresponding category. Pinned OpenShell v0.0.98 cannot attest profile
   content itself, so the deployment system must keep each installed ID
   immutable and verify the rendered bytes against the recorded digest.

2. Verify the named Secret objects and certificate SANs without displaying
   their data. With an explicit kubeconfig/context, lint and render before
   applying anything:

   ```sh
   helm lint "${STEWARD_CHART_PACKAGE}" -f customer-values.yaml \
     --set-file tls.webhook.caBundlePem=webhook-public-ca.pem
   helm template steward "${STEWARD_CHART_PACKAGE}" --namespace steward \
     -f customer-values.yaml \
     --set-file tls.webhook.caBundlePem=webhook-public-ca.pem > steward-rendered.yaml
   ```

   For cert-manager mode, set its issuer in the values file and omit the
   `--set-file` argument. Inspect the rendered resources for the intended
   image digests, namespace, CA bundle, Secret references, RBAC, and egress.
   The core render must contain no Mint Deployment or ClusterSPIFFEID, Jira
   token projection, OpenShell/LiteLLM credentials, or model endpoint.

3. Install using only the selected cluster. Example for customer TLS mode:

   ```sh
   helm --kubeconfig "$CLUSTER_KUBECONFIG" --kube-context "$CLUSTER_CONTEXT" \
     upgrade --install steward "${STEWARD_CHART_PACKAGE}" --namespace steward \
     --create-namespace --atomic --wait --timeout 10m \
     -f customer-values.yaml \
     --set-file tls.webhook.caBundlePem=webhook-public-ca.pem
   ```

   `--atomic` rolls back a failed upgrade but does not undo a CRD that Helm
   placed from `crds/`. Review CRD compatibility before upgrades. For
   cert-manager, wait until both Certificate resources are Ready before
   treating deployment readiness as meaningful.

## Post-install administration (not Helm installation)

A successful Helm install creates and readies only the selected Kubernetes
release objects. It does not create a Steward user, role grant, Workflow,
envelope template, Service Envelope, User Envelope, approval, or provider
grant. None of these administration records is a Helm installation outcome,
and their absence must not be treated as a failed core installation.
Google/browser authentication is conditional on `browserAuth.enabled=true`
and is not a core prerequisite.

Perform only the administration needed for the enabled customer path, after
the installation and mode-specific readiness checks succeed:

### Operator/service post-install administration

Before browser catalog work, an authorized operator provisions each required
Service Envelope through the separately authenticated operator/service identity
accepted by the authoring route:

```text
POST /admin/service-envelopes/{service}
```

This authoring route is separate from the GET-only browser administrator
surface. For the bounded `steward-run` copy-smoke path, use
[`scripts/bootstrap-task-copy-smoke.sh`](../../scripts/bootstrap-task-copy-smoke.sh)
with its documented short-lived route-scoped identity. This explicit service
bootstrap does not create a human user or grant, publish a browser Workflow or
envelope template, create a User Envelope, or make Task execution an
installation outcome.

### Conditional human browser administration

The following numbered steps are one conditional human-administration group.
When `browserAuth.enabled=false`, skip this entire subsection. First-login
canonical-ID discovery, browser Workflow/template publication, and User
Envelope operations are protected by the browser session and administrator
boundaries. There is no documented non-browser substitute for those operations;
enable and verify the human browser path before performing them.

1. **Enable optional human browser administration.** Enable the web and
   `browserAuth`, then configure the operator-owned Google OIDC client, HTTPS
   edge, exact callback, workspace, and organization described in the
   [browser session contract](../browser-session-contract-v1.md). Do not enable
   this human-browser path merely to declare Helm installation successful.
2. **First login and canonical ID.** An organization user signs in once and
   reads the opaque canonical user ID from `/settings`. The login resolves
   identity but grants no administrator or member authority. Email and Google
   subject values are not authorization keys.
3. **Authorized local RBAC grant.** The user gives that opaque ID to an
   authorized Steward operator. In the protected runtime where
   `STEWARD_DATABASE_URL` is already projected, the operator records the
   audited initial grant explicitly:

   ```text
   steward-apiserver-bin bootstrap-rbac \
     --user-id usr_<opaque-id> \
     --grant administrator \
     --actor <audited-operator>
   ```

   There is no first-login administrator shortcut or automatic bootstrap.
   Follow the grant, revocation, session, and CSRF boundaries in the
   [browser session contract](../browser-session-contract-v1.md) and
   [administrator browser contract](../admin-ui-contract-v1.md).
4. **Verify service authority and publish the browser catalog.** The authorized
   browser administrator reads and verifies the existing Service Envelope
   through the GET-only browser surface; it does not provision or modify it.
   Only after verifying that authority ceiling may the administrator publish
   immutable Workflow revisions and author versioned envelope templates bounded
   by it. These are database administration
   operations, not chart resources or Helm values. The route-scoped
   `steward-run` Service Envelope bootstrap above does not grant browser
   authority.
5. **User Envelope operation.** An authenticated user requests authority from
   the applicable published template, and an authorized administrator reviews,
   approves, or rejects that exact request. Before Task submission, prove the
   user has exactly one active provisioned User Envelope with the intended
   revision and authority. Never pre-create or select a User Envelope through
   Helm values.

Record the canonical IDs, immutable revisions, decision evidence, and operator
actors through the product's supported administration surfaces without placing
credentials, tokens, or personal data in the installation delivery record.

## Post-install and delivery tests

Run these against the same explicit context and record the revision, values
file checksum (not its contents), chart/image digests, timestamps, and results.
Do not hand off merely because `helm template` or `helm lint` passed.

1. `helm --kubeconfig "$CLUSTER_KUBECONFIG" --kube-context "$CLUSTER_CONTEXT" status steward -n steward`
   reports a deployed release. `kubectl --kubeconfig "$CLUSTER_KUBECONFIG"
   --context "$CLUSTER_CONTEXT" -n steward rollout status deployment/steward-apiserver`
   and the same command for `deployment/steward-controller` complete.
   The database operator confirms the embedded migration table is at the
   migration packaged in the exact release (currently `0037`) using an
   approved database session that does not expose the URI or row contents.
2. The `agentruntimes.agents.apelogic.ai` CRD is Established, and the
   `steward-agentruntime` validating webhook has `failurePolicy: Fail`, the
   expected service reference, and a nonempty CA bundle. The webhook TLS
   Secret certificate chains to that CA and covers the service DNS name.
3. Verify API and webhook TLS connections from an authorized in-cluster test
   client using the configured public CA. A TCP readiness probe alone does
   not prove certificate validity or admission behavior. Submit an invalid
   AgentRuntime with server-side dry-run and confirm it is denied; do not
   create a long-lived runtime as a smoke test.
4. In core mode, confirm there are no Mint pods, no OpenShell or LiteLLM
   credential projections, no Jira Secret or Jira egress, and no Task owner
   loop. Confirm the API/controller are usable with only PostgreSQL, the
   Kubernetes API, and TLS supplied. A Task execution attempt must fail
   closed, not start a sandbox or call a model.
5. For governed mode, first confirm OpenShell mTLS, workload exchange,
   SPIRE identity, LiteLLM, and Mint readiness. Activate execution bindings and Task
   orchestration only in their documented staged rollout sequence. Run one
   approved bounded Task, then verify execution, audit, and cleanup. Confirm no
   undeclared provider profile was attached. This is functional sandbox
   acceptance and does not establish VM isolation.
6. Exercise a fresh install, same-revision upgrade, a supported prior-version
   upgrade, and rollback on a disposable or otherwise explicitly authorized
   target. Verify no user data, credentials, CRDs, or external integrations
   were unintentionally removed. Record any unrun case as a delivery gap.

## Upgrade, rollback, backup, and removal

Treat the chart, all component images, configuration, and database schema as
one release handoff. Before an upgrade, record the current exact image and
chart digests, values-file checksum, installed CRD versions, schema migration
state, and Helm revision; stop new Task submissions and drain governed work.
Back up PostgreSQL using the database operator's consistent, encrypted backup
procedure and test restoration to a separate database. Back up the Helm values
and the *references* to Secrets/PKI in the customer's protected configuration
store, not secret bytes in a support packet. Record external issuer, OpenShell,
LiteLLM, and gateway versions and any active Runtime UIDs. The chart does not
back up or restore PostgreSQL, Secrets, external services, or Runtime objects.

Review every append-only SQL migration under [`migrations/`](../../migrations/)
before upgrading. Both API and controller apply the embedded migration set on
startup; they must use the same database and must not start across incompatible
schema revisions. In particular, migration 0031 fails closed if older state
has overlapping unknown/live attempts. The staged execution-binding rollout
has its own [upgrade sequence](upgrade-execution-bindings.md); do not skip its
drain and staged phases. Run `helm template` and the preflight/key-presence
checks with the new immutable handoff before `helm upgrade --atomic --wait`.
Then repeat the delivery tests, including model-free/core or governed Tasks as
appropriate. A Helm rollback cannot reverse SQL migrations or restore external
state; do not equate an `--atomic` return with a safe data rollback.

For rollback, first stop submissions and prove in-flight Tasks are finalized
or safely fenced. Use the documented compatibility of the previous binaries
with the *current* schema; if that compatibility is not proven, restore the
database to a new target from the pre-upgrade backup and validate it before
switching traffic. Restore only the exact previous chart/image/values handoff:

```sh
helm --kubeconfig "$CLUSTER_KUBECONFIG" --kube-context "$CLUSTER_CONTEXT" \
  rollback steward <recorded-revision> -n steward --wait
```

Never run old controllers while new bound Tasks remain claimable. Retest
admission, database access, TLS, and the mode-specific delivery path.

For uninstall, stop callers and governed executions, inventory any remaining
AgentRuntimes, Task records, runtime Secrets, and external integration grants,
and obtain the customer's retention/deletion decision for each. Only then run:

```sh
helm --kubeconfig "$CLUSTER_KUBECONFIG" --kube-context "$CLUSTER_CONTEXT" \
  uninstall steward -n steward --wait
```

Helm does not
remove CRDs installed from `crds/` or the external PostgreSQL database, and
the operator must not delete CRDs, the namespace, Secrets, backups, or
customer-owned issuers merely because the release is gone. Confirm no
Steward-owned workloads or webhooks remain, and separately verify any
authorized credential revocation and data-retention actions.

If a test fails, stop the hand-off. Inspect bounded Deployment state, events,
and non-secret logs; correct the prerequisite or artifact and rerun the failed
case. Do not replace a configured credential with an example value or disable
admission, TLS verification, or NetworkPolicy to obtain a green result.
