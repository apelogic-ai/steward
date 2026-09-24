# Platform deployment order

Status: **Reference**

Applies to Steward v0.2.2 and its User-Envelope-only Task authority model. An
installation still on v0.1.23 follows [upgrade to v0.2.2](upgrade-v0.2.0.md)
before using this page.

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
| Steward | Control plane: API, admission webhook, controller | [installation guide](installation-guide.md) | Token acceptance, Envelope authority, approval, execution |
| MCP-GW, LiteLLM, OpenShell, SPIRE | Customer-operated dependencies | Their own products | Their own deployment and credentials |

The normative description of the token that crosses the boundary is the
Identity product's
[consumer contract](https://github.com/apelogic-ai/github-oidc-exchange/blob/main/docs/consumer-contract-v1.md).
Steward's side of it is
[task identity](#step-4-install-steward-and-wire-task-identity) below.

## The dependency that sets the order

An external Task is admitted only by the authenticated user's exact active
provisioned **User Envelope**. Its authority key is the opaque canonical user
ID, and Steward allocates that ID on the person's *first browser login*. The
Identity service must stamp that same ID into the token's `groups` claim.

Identity policy therefore cannot be finalized until Steward exists and the user
has signed in once, while `steward-run` cannot authenticate until Identity
policy is finalized. The order below resolves that by installing Identity early
and enrolling it late.

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

Both modes additionally require the human browser path. Envelope templates and
User Envelope operations are protected by the browser session and administrator
boundaries, and the installation guide documents no non-browser substitute, so
`browserAuth.enabled=true` and its Google OIDC client, HTTPS edge, and exact
callback are prerequisites for any external Task submission — not an optional
presentation layer.

## Step 1: record the version set

No product publishes a combined compatibility matrix. Before installing, record
one version per row from the release handoffs actually being installed, and
check each against Steward's
[tested versions and integration boundaries](installation-guide.md#tested-versions-and-integration-boundaries).
A component outside that table is untested here, not merely undocumented.

| Component | Version in this deployment | Source of truth |
|---|---|---|
| Steward chart and images | | Steward release handoff |
| Steward deployment lock and platform preflight bundle | | Same Steward release handoff |
| `steward-run` runner image, chart, workflow commit | | `steward-run` release manifest |
| Identity application and chart | | Identity release handoff |
| Kubernetes | | Cluster; must satisfy every chart's `kubeVersion` simultaneously |
| PostgreSQL, MCP-GW, LiteLLM, OpenShell, agent-sandbox | | Customer deployment |

The Kubernetes version must satisfy the **intersection** of the three charts'
declared ranges, which is narrower than Steward's own range alone.

## Step 2: install Identity, unenrolled

Install the Identity service first, because every later step needs its issuer
URL and public JWKS. Install it before its GitHub policy is final: the policy
requires observed GitHub claims and a Steward canonical user ID, neither of
which exists yet.

Finish this step when discovery and `GET {issuer}/jwks.json` return the exact
configured issuer and an ES256 key.

## Step 3: install `steward-run`

Install the ARC controller, the registration Secret, and the runner scale set,
then pin the customer-owned reusable workflow to a reviewed commit and supply
both Identity inputs. The caller workflow shape is in the Identity product's
integration guide; the runner and registration procedure is in the `steward-run`
installation guide.

Expect governed runs to fail authentication until Step 6. That is the correct
result, and one such run is how Step 6 observes the real GitHub claims.

## Step 4: install Steward and wire task identity

Install Steward with its [installation guide](installation-guide.md). Core mode
is the supported starting point even when governed execution is the goal. If
artifacts are copied to another registry, produce the verified
[deployment lock](registry-mirroring.md), then use the released
[platform preflight](platform-preflight.md) to generate the Helm/Flux values and
namespace-qualified references. Run `gateway-check` before exposing the browser
edge. On EKS, run both `network-check` and the bounded `network-smoke` before
treating NetworkPolicy as enforced.

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
the two-minute lifetime and clock-skew allowance. A verified token is
authentication only; authority comes from Step 5.

Steward's Mint publishes its own separate JWKS at
`<mint-issuer>/.well-known/jwks.json` using EdDSA. It is unrelated to this
ConfigMap; do not project one where the other is expected.

Rotate by refreshing the ConfigMap whenever the Identity issuer publishes a new
`kid`, keeping every overlapping key until the old tokens and skew allowance
have expired, then reproving issuer, audience, and signature.

## Step 5: provision authority in Steward, and record the canonical user ID

Helm creates no user, grant, template, Envelope, or approval. Follow
[post-install administration](installation-guide.md#post-install-administration-not-helm-installation)
to enable the browser path, have the person sign in once, record the audited
initial RBAC grant, verify the deployment capability catalog, author versioned
Envelope templates, and complete one User Envelope request and approval.

Two outputs of this step are inputs to Step 6:

- the opaque `usr_<...>` canonical user ID the person reads from `/settings`;
- the verified email bound to that canonical identity.

Finish this step when the person has exactly one active provisioned User
Envelope with the intended revision and authority. The capability catalog
advertises models and tools but grants no authority, and an empty catalog
cannot narrow an Envelope that already admits a Task.

## Step 6: enroll the Identity policy

Identity policy admits exact observed values, not patterns. Using the claims
observed from Step 3 and the canonical identity from Step 5, enroll the exact
subject, numeric repository and owner identifiers, allowed event and ref, and
the reviewed actor mapping to that verified email and canonical user ID.

Steward derives the acting identity from the `groups` claim the policy stamps.
Both products already use the same prefixes, but nothing installs them
together, so they are one decision:

| Group prefix | Cardinality |
|---|---|
| `agents.apelogic.ai/service-principal:` | exactly one, non-empty |
| `agents.apelogic.ai/canonical-user:` | exactly one, and exactly the ID from Step 5 |
| `agents.apelogic.ai/acting-user:` | at most one; must equal the token's verified email |
| `agents.apelogic.ai/task-owner:` | exactly one when no acting user is present; rejected alongside an acting user |

At most sixteen groups are accepted in total. The service principal names the
submitting service and grants no authority of its own; it participates in Task
ownership and idempotency naming. Authority is the User Envelope bound to the
canonical user, so a token whose canonical user has no active provisioned
Envelope fails closed even though every signature check passed.

Finish this step when one real assertion exchanges successfully, a replay of
the same assertion is denied, and a wrong repository, ref, actor, and audience
are each denied with a fresh assertion.

## Step 7: accept authentication and admission in core mode

Submit one direct Git package invocation while Steward remains in core mode.
The caller references the exact package source through a checked-in invocation
manifest. Steward must authenticate the caller, admit the Task against the
caller's unique active provisioned User Envelope, and record the exact User
Envelope evidence without creating a runtime. A published Workflow revision
remains an optional curation layer over the same immutable package, not a
registration prerequisite.

Repeat with a wrong audience, an untrusted issuer or CA, an unauthorized
repository, ref, and actor, and a canonical user with no active Envelope. Each
must fail closed before a Task is created. This step proves identity and
authority only; core mode deliberately cannot prove agent execution or output.
After recording the accepted Task's evidence, request its cleanup and confirm
it is finalized so enabling execution cannot later start this staged test Task.

## Step 8: governed execution, if in scope

Only after Step 7 passes: enable the Identity product's workload exchange mode,
install OpenShell, agent-sandbox, and SPIRE, create the OpenShell client, Mint,
LiteLLM, and workload-exchange trust objects, point
`config.apiserver.mcpGatewayEndpoint` and `config.controller.litellmUrl` at the
existing deployments, install the provider profile bundle, and record
[execution bindings](execution-bindings.md). Use the preflight-generated binding
and values so the provider-profile digests and reference runtime come from the
same verified deployment lock; for Codex, use the supported
[reference-runtime procedure](codex-reference-runtime.md). Re-run the live
Gateway and applicable network checks against the final namespaces, then move
the staged ownership switches. The prerequisites and their verification live in
the [installation guide](installation-guide.md); this page adds only their
position in the order.

## Step 9: accept governed execution end to end

Submit the same package in a new governed job with known inputs and an
expected output hash. Confirm the Task reaches its terminal success phase, the
output hash matches, the Run detail retains the exact User Envelope evidence,
and the runtime is finalized.

Record source revisions, artifact digests, the GitHub run identifier, the
bounded Task UID and status, HTTP status, and public JWKS `kid`s only. Never
record tokens, authorization headers, policy mappings, or response bodies.
