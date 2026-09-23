# Platform deployment order

Status: **Reference**

Steward, `steward-run`, and `github-oidc-exchange` are separately released
products with separate installation guides. Each guide correctly declares the
others external and stops at its own boundary. This page supplies only what no
single guide owns: the order in which a customer installs them, the reason the
order is what it is, and the Steward-side configuration that joins them.

Authority is limited to that subject. Each product's own installation guide
remains authoritative for its own steps, and
[`docs/contracts/m1/v1/**`](../contracts/m1/v1/README.md) remains the normative
wire contract. Where this page and a product guide disagree about that
product's procedure, the product guide controls.

## The products and their boundaries

| Product | Installs | Its guide | Authoritative for |
|---|---|---|---|
| `github-oidc-exchange` | Kubernetes Identity service issuing the task token | [quickstart](https://github.com/apelogic-ai/github-oidc-exchange/blob/main/docs/quickstart.md), [installation](https://github.com/apelogic-ai/github-oidc-exchange/blob/main/docs/installation.md) | Issuer, policy, keyring, JWKS, exchange routes |
| `steward-run` | GitHub Action, reusable workflow, ARC runner scale set | [installation](https://github.com/apelogic-ai/steward-run/blob/main/docs/installation-v0.4.2.md) | Runner registration, workflow pinning, action inputs |
| Steward | Control plane: API, admission webhook, controller | [installation guide](installation-guide.md) | Token acceptance, envelopes, approval, execution |
| MCP-GW, LiteLLM, OpenShell, SPIRE | Customer-operated dependencies | Their own products | Their own deployment and credentials |

The normative description of the token that crosses the boundary is the
Identity product's
[consumer contract](https://github.com/apelogic-ai/github-oidc-exchange/blob/main/docs/consumer-contract-v1.md).
Steward's side of that contract is [task identity](#step-5-install-steward-and-wire-task-identity) below.

## Step 0: choose the Steward mode before ordering anything

The installation order depends on a decision described in the
[installation guide](installation-guide.md#choose-the-installation-mode). The two
modes have materially different dependency sets, and an operator who scopes the
project as "Steward plus the runner plus the exchange" is describing core mode
only.

| Mode | Additional products required |
|---|---|
| Core (`execution.enabled=false`) | PostgreSQL only. Governed submission, admission, approval, and audit work; Task execution stays staged. |
| Governed execution (`execution.enabled=true`) | OpenShell, agent-sandbox, SPIRE CSI and `ClusterSPIFFEID`, a Mint signing Secret, LiteLLM, the Identity product's **workload** exchange mode, and optionally MCP-GW. |

An existing MCP-GW and LiteLLM deployment does not by itself satisfy governed
execution. The Identity product's baseline quickstart also deliberately
excludes the workload exchange that OpenShell requires; governed execution uses
that product's full installation guide, not its quickstart.

## Step 1: record the version set

No product publishes a combined compatibility matrix. Before installing, record
one version per row from the release handoffs actually being installed, and
check each against Steward's
[tested versions and integration boundaries](installation-guide.md#tested-versions-and-integration-boundaries).
A component outside that table is untested here, not merely undocumented.

| Component | Version in this deployment | Source of truth |
|---|---|---|
| Steward chart and images | | Steward release handoff |
| `steward-run` runner image, chart, workflow commit | | `steward-run` release manifest |
| Identity application and chart | | Identity release handoff |
| Kubernetes | | Cluster; must satisfy every chart's `kubeVersion` simultaneously |
| PostgreSQL, MCP-GW, LiteLLM, OpenShell, agent-sandbox | | Customer deployment |

The Kubernetes version must satisfy the **intersection** of the three charts'
declared ranges, which is narrower than Steward's own range alone.

## Step 2: install Identity, unenrolled

Install the Identity service first, because every later step needs its issuer
URL and public JWKS. Install it before its GitHub policy is final: the policy
requires claims that do not exist until a real workflow has run.

Finish this step when discovery and `GET {issuer}/jwks.json` return the exact
configured issuer and an ES256 key.

## Step 3: install `steward-run`

Install the ARC controller, the registration Secret, and the runner scale set,
then pin the customer-owned reusable workflow to a reviewed commit and supply
both Identity inputs. The caller workflow shape is in the Identity product's
integration guide; the runner and registration procedure is in the `steward-run`
installation guide.

Expect the first governed run to fail authentication. That is the correct
result until Step 4 enrolls the claims this run produces.

## Step 4: enroll the observed claims in Identity policy

Identity policy admits exact observed values, not patterns. Run the pinned
workflow once, observe the real GitHub claims, and enroll the exact subject,
numeric repository and owner identifiers, allowed event and ref, and the
reviewed actor mapping. This is why Identity is installed before `steward-run`
but enrolled after it.

Finish this step when one real assertion exchanges successfully, a replay of
the same assertion is denied, and a wrong repository, ref, actor, and audience
are each denied with a fresh assertion.

## Step 5: install Steward and wire task identity

Install Steward with its [installation guide](installation-guide.md). Core mode
is the supported starting point even when governed execution is the goal.

A default installation authenticates Task submissions with Kubernetes
TokenReview and will reject every token the Identity service issues. Accepting
those tokens is an explicit opt-in that the installation guide lists only as an
object inventory row. The complete Steward-side procedure is:

1. Fetch the Identity public JWKS and confirm it contains public keys only:

   ```sh
   IDENTITY_ISSUER=https://identity.example.test
   curl --fail --silent --show-error "${IDENTITY_ISSUER}/jwks.json" \
     --output identity-task-jwks.json
   ```

   Reject the file if any key carries a private member. This is public
   material; it is a `ConfigMap`, never a Secret.

2. Create the ConfigMap in the release namespace:

   ```sh
   kubectl --kubeconfig "$CLUSTER_KUBECONFIG" --context "$CLUSTER_CONTEXT" \
     create configmap steward-task-identity-jwks --namespace steward \
     --from-file=jwks.json=identity-task-jwks.json
   ```

3. Set the four values together. The chart schema accepts either a fully empty
   block or a fully populated one; a partially filled block fails before
   installation:

   ```yaml
   taskIdentity:
     enabled: true
     issuer: https://identity.example.test
     audience: steward-task-api
     publicJwksConfigMap:
       name: steward-task-identity-jwks
       key: jwks.json
   ```

   `issuer` is the exact HTTPS issuer with no trailing slash. `audience` is the
   exact audience the Identity release issues for task tokens; the current
   consumer contract fixes it at `steward-task-api`, and a caller cannot select
   it.

4. Verify the rendered apiserver carries `STEWARD_IDENTITY_TASK_ISSUER`,
   `STEWARD_IDENTITY_TASK_AUDIENCE`, and the read-only projection at
   `/run/identity-task/jwks.json` before installing.

Steward then accepts a submission token only when it is ES256 from that JWKS,
carries the exact issuer and audience, declares `identity_contract` exactly
`steward-task-v2`, presents a bounded single-use `jti`, and is current within
the two-minute lifetime and clock-skew allowance.

Steward's Mint publishes its own separate JWKS at
`<mint-issuer>/.well-known/jwks.json` using EdDSA. It is unrelated to this
ConfigMap; do not project one where the other is expected.

Rotate by refreshing the ConfigMap whenever the Identity issuer publishes a new
`kid`, keeping every overlapping key until the old tokens and skew allowance
have expired, then reproving issuer, audience, and signature.

## Step 6: agree on the group vocabulary

Identity policy stamps the `groups` claim; Steward derives the acting identity
from it. Both sides already use the same prefixes, but nothing installs them
together, so they must be authored as one decision. Steward requires:

| Group prefix | Cardinality |
|---|---|
| `agents.apelogic.ai/service-principal:` | exactly one, non-empty |
| `agents.apelogic.ai/canonical-user:` | exactly one, parseable canonical user ID |
| `agents.apelogic.ai/acting-user:` | at most one; must equal the token's verified email |
| `agents.apelogic.ai/task-owner:` | exactly one when no acting user is present; rejected alongside an acting user |

At most sixteen groups are accepted in total. The service principal named here
must match the Service Envelope provisioned in Step 7; an envelope that does
not exist fails submission closed even when the token verifies.

## Step 7: post-install administration

A successful Helm install creates no Steward user, Workflow, envelope, grant,
or approval. Provision at least the `steward-run` Service Envelope and register
the Workflow reference the action passes, per
[post-install administration](installation-guide.md#post-install-administration-not-helm-installation).

## Step 8: accept the platform end to end

Run one governed job with known inputs and an expected output hash, then repeat
with a wrong audience, an untrusted issuer or CA, and an unauthorized
repository, ref, and actor. Each must fail closed before a Task is created.
Record source revisions, artifact digests, the GitHub run identifier, the
bounded Task UID and status, HTTP status, and public JWKS `kid`s only. Never
record tokens, authorization headers, policy mappings, or response bodies.

## Step 9: governed execution, if in scope

Only after Step 8 passes: enable the Identity product's workload exchange mode,
install OpenShell, agent-sandbox, and SPIRE, create the OpenShell client, Mint,
LiteLLM, and workload-exchange trust objects, point
`config.apiserver.mcpGatewayEndpoint` and `config.controller.litellmUrl` at the
existing deployments, install the provider profile bundle, record
[execution bindings](execution-bindings.md), and only then move the staged
ownership switches. The prerequisites and their verification live in the
[installation guide](installation-guide.md); this page adds only their position
in the order.
