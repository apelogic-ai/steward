# Steward installation guide

Status: candidate installation procedure for the Helm chart in this repository;
live clean-room delivery evidence is still required. Use a chart and images
built from the same exact Steward revision. A rendered manifest is not an
installation acceptance result; complete the delivery tests below on the
target cluster before handing it to an operator.

## Choose the installation mode

| Mode | What starts | Additional prerequisites |
|---|---|---|
| Core (default, `execution.enabled=false`) | API, admission webhook, and AgentRuntime controller with Task orchestration staged | None of Jira, an inference endpoint, LiteLLM, OpenShell, SPIRE, a Mint Secret, or a sandbox RuntimeClass |
| Governed execution (`execution.enabled=true`) | Core plus Mint, OpenShell reconciliation, LiteLLM inference, and workload identity | OpenShell gateway/client mTLS, LiteLLM, workload exchange, SPIRE CSI and ClusterSPIFFEID, a reviewed sandbox RuntimeClass, Mint signing material and trust |

Jira is a separate opt-in decision-channel integration (`jira.enabled=true`) in
either mode. With Jira disabled, decisions that need it fail closed; the chart
does not mount its Secret or grant Jira egress. Core mode is for a **new**
installation, not a switch that cleans up existing governed sandboxes. Do not
turn execution off on an installation with live AgentRuntimes or Tasks.

## Prerequisites

1. Kubernetes 1.30 or newer, Helm 3, `kubectl`, and cluster-admin authority for
   the Steward CRD, cluster roles, and validating webhook. Select an explicit
   kubeconfig and context; do not use an ambient or unrelated cluster.
2. A reachable, separately operated PostgreSQL database and an existing
   `steward-database` Secret with key `url` in the installation namespace.
   Supply a database URL appropriate to the platform's TLS policy. Steward
   creates no database, PVC, backup, or database credential.
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
certificate Secret, approved `RuntimeClass`, LiteLLM URL/master-key Secret,
workload exchange URL/server name/public CA projection, SPIRE CSI driver and
`ClusterSPIFFEID` API, and the Mint Secret. Verify the sandbox RuntimeClass
actually provides the expected isolation on this cluster; a configured name
or a Kind/runc smoke is not such evidence. See [chart configuration](../../charts/steward/README.md)
and [execution bindings](execution-bindings.md) before activating Tasks.

### Tested versions and integration boundaries

These are the versions exercised or declared by this repository, not a promise
that every other version works. Pin each external product and prove its contract
again in the customer's cluster before enabling governed execution.

| Component | Repository evidence | Installation implication |
|---|---|---|
| Kubernetes | The chart declares `kubeVersion: >=1.30.0-0`; the S3 envelope E2E pins Kind node `v1.32.1`. | Verify the target API version and admission/RBAC/NetworkPolicy behavior. A version declaration is not a tested cluster matrix. |
| PostgreSQL | `scripts/postgres-tls-e2e.sh` and pinned conformance use `postgres:16-alpine` at a fixed digest. | PostgreSQL 16 is the tested database line. Provision it, TLS, backups, and availability outside Steward. |
| OpenShell and agent-sandbox | `scripts/openshell-adapter-e2e.sh` pins OpenShell `v0.0.98` and agent-sandbox `v0.5.0`. G-1 conformance separately pins an older OpenShell revision. | The adapter test proves RuntimeClass propagation with a Kind `runc` handler, not VM isolation. Review the actual gateway, driver, policy, and sandbox image on the target. |
| MCP-GW | The governed Connections bridge accepts authority v1 contract `0.3.2` or v2 contract `0.4.9`, selected by the binding. | Do not infer compatibility for an arbitrary MCP-GW release or enable a Connections bridge without the matching authority and image provenance. |
| SPIRE, LiteLLM, cert-manager, browser identity, GitHub, and edge gateway | The chart declares interfaces but no general supported-version matrix for these services. | Supply exact tested versions and acceptance evidence in the customer delivery record; do not describe an untested combination as supported. |

Integration ownership is explicit: core requires only PostgreSQL, Kubernetes
TokenReview/API access, and service TLS; Jira adds a decision channel; browser
OIDC adds an external identity provider and edge; GitHub source adds a read-only
GitHub App; task identity adds an external issuer/public JWKS; governed execution
adds OpenShell, SPIRE, workload exchange, LiteLLM, Mint, a sandbox RuntimeClass,
and optionally MCP-GW/provider profiles. The stable and Connections bridges are
independent opt-ins with immutable images and provenance contracts. See the
[chart configuration](../../charts/steward/README.md) for exact flags and
ingress/egress requirements.

## Secret and integration inventory

The chart references existing names and keys; it never puts secret bytes in
values. Manage creation and rotation with the customer's approved secret
system. The default names can be overridden under `secrets`, `tls`,
`browserAuth`, and `githubSource`.

| Reference and namespace | Required keys | Producer and consumer | Rotation / condition |
|---|---|---|---|
| `secrets.database.name` (`steward-database`) in release namespace | `secrets.database.key` (`url`) | Database operator creates; API and controller read. | Always. Rotate the database credential and restart both Deployments after the new Secret is present; verify connectivity and migrations. |
| `tls.api.secretName` and `tls.webhook.secretName` in release namespace | Both `tls.crt`, `tls.key`, type `kubernetes.io/tls` | Customer PKI or cert-manager creates; API and controller mount separately. | Always. Renew before expiry, verify service DNS SANs, CA chain, and webhook `caBundle`; roll the affected Deployment. |
| `secrets.jira.name` (`steward-jira`) in release namespace | `secrets.jira.key` (`token`) | Jira operator creates; API and controller read only if `jira.enabled=true`. | Optional. Rotate with the Jira service account, then restart consumers and prove a decision; absent when disabled. |
| `secrets.litellm.name` (`steward-litellm`) in release namespace | `secrets.litellm.key` (`master-key`) | LiteLLM operator creates; controller reads. | Governed mode only. Coordinate credential overlap/restart with LiteLLM. |
| `secrets.openshellClient.name` (`steward-openshell-client`) in release namespace | Configured CA, client certificate, and private-key keys (`ca.crt`, `tls.crt`, `tls.key`) | OpenShell/customer PKI creates; controller mounts. | Governed mode only. Rotate as an mTLS bundle and reprove server-name/CA validation. |
| `secrets.mint.name` (`steward-mint`) in release namespace | Configured `signing-key`, `introspection-credential` | Customer key authority creates; Mint mounts. | Governed mode only. Coordinate JWKS/key rollover and introspection credential overlap with consumers. |
| `workloadExchangeTrust.name` in release namespace | `workloadExchangeTrust.caCertificate` (`ca.crt`) | Workload-exchange PKI creates public CA ConfigMap or Secret; controller mounts. | Governed mode only; rotate with exchange TLS and reprove trust. |
| `browserAuth.google.clientSecret.name` in release namespace | Configured `clientSecret.key` | Identity-provider operator creates; API reads. | Only `browserAuth.enabled=true`; rotate with provider, restart API, and retest login/callback. |
| `githubSource.privateKeySecret.name` in release namespace | Configured private-key key, normally PEM | GitHub App owner creates; API mounts read-only. | Only `githubSource.enabled=true`; rotate the App key and retest exact Git-object resolution. |
| `web.ingress.tlsSecretName` in release namespace | `tls.crt`, `tls.key` | Customer edge PKI creates; Ingress controller reads. | Only legacy `web.ingress.enabled=true`; gateway-owned TLS stays outside this chart. |
| Each `imagePullSecrets` reference in release namespace | Registry-specific credential data | Registry operator creates; kubelet reads. | Only private registries; rotate before expiry and verify pulls without printing the Secret. |
| Runtime-UID-named Secret in each allowed runtime namespace | `access-token` | Controller creates from a runtime-scoped LiteLLM credential; sandbox consumes. | Governed runtime only. It is UID-bound and owner-referenced; controller deletes it on suspend/termination. Do not pre-create, back up as a reusable credential, or share across runtimes. |

The optional task identity JWKS is a **public ConfigMap**, not a Secret.
Bridge attestation bundles and the customer webhook CA are public material.
Do not print or commit Secret values, database URLs, private keys, tokens, or
credential-bearing Helm values.

Before install, check object type and key *presence* without disclosing data.
For example, with `jq` installed, a customer-supplied TLS Secret can be checked
as follows; repeat for every enabled row above, using its configured name/key.
For `certManager` mode, check the issuer first and check generated TLS Secrets
after the Certificates become Ready.

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

1. Select a chart directory from the exact release or fork revision and record
   its OCI digest (or, for a source-tree install, the exact commit and chart
   archive checksum) and each image digest in the delivery record. Set a
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

2. Verify the named Secret objects and certificate SANs without displaying
   their data. With an explicit kubeconfig/context, lint and render before
   applying anything:

   ```sh
   helm lint ./charts/steward -f customer-values.yaml \
     --set-file tls.webhook.caBundlePem=webhook-public-ca.pem
   helm template steward ./charts/steward --namespace steward \
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
     upgrade --install steward ./charts/steward --namespace steward \
     --create-namespace --atomic --wait --timeout 10m \
     -f customer-values.yaml \
     --set-file tls.webhook.caBundlePem=webhook-public-ca.pem
   ```

   `--atomic` rolls back a failed upgrade but does not undo a CRD that Helm
   placed from `crds/`. Review CRD compatibility before upgrades. For
   cert-manager, wait until both Certificate resources are Ready before
   treating deployment readiness as meaningful.

## Post-install and delivery tests

Run these against the same explicit context and record the revision, values
file checksum (not its contents), chart/image digests, timestamps, and results.
Do not hand off merely because `helm template` or `helm lint` passed.

1. `helm --kubeconfig "$CLUSTER_KUBECONFIG" --kube-context "$CLUSTER_CONTEXT" status steward -n steward`
   reports a deployed release. `kubectl --kubeconfig "$CLUSTER_KUBECONFIG"
   --context "$CLUSTER_CONTEXT" -n steward rollout status deployment/steward-apiserver`
   and the same command for `deployment/steward-controller` complete.
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
   SPIRE identity, LiteLLM, Mint readiness, the reviewed RuntimeClass, and
   policy-bound sandbox isolation. Activate execution bindings and Task
   orchestration only in their documented staged rollout sequence. Run one
   approved bounded Task, then verify execution, audit, and cleanup. Do not
   infer isolation from a successful unconfined Kind/runc run.
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
