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
     `tls.issuerRef`. Steward creates Certificate requests, not an issuer.
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

## Secret and integration inventory

The chart references existing names and keys; it never puts secret bytes in
values. Manage creation and rotation with the customer's approved secret
system. The default names can be overridden under `secrets`, `tls`,
`browserAuth`, and `githubSource`.

| Reference | Keys | Required when |
|---|---|---|
| `steward-database` | `url` | Always; API and controller |
| `steward-apiserver-tls`, `steward-webhook-tls` | `tls.crt`, `tls.key` | Always; customer-supplied or cert-manager-created |
| `steward-jira` | `token` | `jira.enabled=true` |
| `steward-litellm` | `master-key` | `execution.enabled=true` |
| `steward-openshell-client` | `ca.crt`, `tls.crt`, `tls.key` | `execution.enabled=true` |
| `steward-mint` | `signing-key`, `introspection-credential` | `execution.enabled=true` |
| Workload-exchange public CA ConfigMap/Secret | configured CA key | `execution.enabled=true` |
| Browser OIDC client Secret | configured client-secret key | `browserAuth.enabled=true` |
| GitHub source App Secret | configured PEM private-key key | `githubSource.enabled=true` |
| Image pull Secret | registry credential | Private registry only |

The optional task identity JWKS is a **public ConfigMap**, not a Secret.
Bridge attestation bundles and the customer webhook CA are public material.
Do not print or commit Secret values, database URLs, private keys, tokens, or
credential-bearing Helm values.

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

If a test fails, stop the hand-off. Inspect bounded Deployment state, events,
and non-secret logs; correct the prerequisite or artifact and rerun the failed
case. Do not replace a configured credential with an example value or disable
admission, TLS verification, or NetworkPolicy to obtain a green result.
