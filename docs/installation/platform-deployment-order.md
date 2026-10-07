# Platform deployment order

Status: **Reference**

Applies to Steward v0.3.9 and its User-Envelope-only Task authority model. An
installation still on v0.1.23 first follows the [v0.2 upgrade](upgrade-v0.2.0.md),
and an installation on v0.2.6 follows the [v0.3 upgrade](upgrade-v0.3.0.md)
before using this page.

Steward, `steward-run`, and `github-oidc-exchange` are separately released
products with separate installation guides. Each guide correctly declares the
others external and stops at its own boundary. This page supplies only what no
single guide owns: the order in which a customer installs them, the reason the
order is what it is, and the Steward-side configuration that joins them.

Authority is limited to that subject. Each product's own installation guide
remains authoritative for its own steps, and the
[Task v2 contract](../contracts/task/v2/README.md) plus Steward's
[product compatibility](governed-platform-compatibility.md) remain the
normative wire and client-capability contracts. Where this page and a product guide disagree about that
product's procedure, the product guide controls.

## The products and their boundaries

| Product | Installs | Its guide | Authoritative for |
|---|---|---|---|
| `github-oidc-exchange` | Kubernetes Identity service issuing the task token | [quickstart](https://github.com/apelogic-ai/github-oidc-exchange/blob/main/docs/quickstart.md), [installation](https://github.com/apelogic-ai/github-oidc-exchange/blob/main/docs/installation.md) | Issuer, policy, keyring, JWKS, exchange routes |
| `steward-run` | GitHub Action, reusable workflow, ARC runner scale set | [installation](https://github.com/apelogic-ai/steward-run/blob/main/docs/installation.md) | Runner registration, workflow pinning, action inputs |
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
Identity service must stamp that same ID into a v2 token's `groups` claim. A v3
token instead carries the stable numeric GitHub actor subject; by default,
Steward associates it with the signed-in canonical user after GitHub Connect
verifies the same immutable numeric account ID.

A v2 Identity policy therefore cannot be finalized until Steward exists and the
user has signed in once. For the first valid v3 submission to proceed without an
administrator round trip, the default v3 path requires that user to sign in and
connect the GitHub account that will run the workflow. The manual v3 fallback
instead observes the subject on the first valid submission and requires
administrator association before a retry can proceed. `steward-run` cannot
authenticate until Identity policy is finalized, so the order below installs
Identity early and enrolls it late.

## Step 0: choose the Steward mode before ordering anything

The installation order depends on a decision described in the
[installation guide](installation-guide.md#choose-the-installation-mode). The two
modes have materially different dependency sets, and an operator who scopes the
project as "Steward plus the runner plus the exchange" is describing core mode
only.

| Mode | Additional products required |
|---|---|
| Core (`execution.enabled=false`) | PostgreSQL only. The API, webhook, browser administration when enabled, and AgentRuntime validation work; new Task submission is disabled while orchestration is staged. |
| Governed execution (`execution.enabled=true`) | OpenShell, agent-sandbox, SPIRE CSI and `ClusterSPIFFEID`, the SPIRE controller class name, a Mint signing Secret, LiteLLM, the Identity product's **workload** exchange mode, and optionally MCP-GW. |

An existing MCP-GW and LiteLLM deployment does not by itself satisfy governed
execution. The Identity product's baseline quickstart also deliberately
excludes the workload exchange that OpenShell requires; governed execution uses
that product's full installation guide, not its quickstart.

Browser authentication is not a prerequisite for installing or verifying core
mode. Enable it only when the deployment uses Steward's human browser
administration path. The browser-assisted sequence below uses that path to
discover a canonical user, publish governance data, and provision authority;
those steps therefore require `browserAuth.enabled=true`, its Google OIDC
client, HTTPS edge, and exact callback. They do not make browser authentication
a dependency of the core binaries themselves.

## Step 1: record the version set

Release/integration packaging publishes the signed installation BOM after all
independently versioned artifacts exist. Use that BOM as the exact version set;
verify its coordinates against each product handoff and check its declared
contracts against Steward's
[product compatibility](governed-platform-compatibility.md). Do not reconstruct
the installation set from source-repository pins.

| Component | Version in this deployment | Verification input |
|---|---|---|
| Steward chart and images | | Steward release handoff |
| Steward deployment lock and platform preflight bundle | | Same Steward release handoff |
| `steward-run` runner image, chart, workflow commit | | `steward-run` release manifest |
| Identity application and chart | | Identity release handoff |
| Kubernetes | | Cluster; must satisfy every chart's `kubeVersion` simultaneously |
| PostgreSQL, MCP-GW, LiteLLM, OpenShell, agent-sandbox | | Installation BOM and dependency handoffs |

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

3. Set the v2 values together. This preserves the existing direct Identity
   token contract and does not publish discovery metadata:

   ```yaml
   taskIdentity:
     enabled: true
     issuer: https://identity.example.test
     audience: steward-task-api
     resource: ""
     federatedSubjects:
       enabled: false
     publicJwksConfigMap:
       name: steward-task-identity-jwks
       key: jwks.json
   ```

   `issuer` is the exact HTTPS issuer with no trailing slash. `audience` is the
   exact audience the Identity release issues for task tokens; the current
   consumer contract fixes it at `steward-task-api`, and a caller cannot select
   it.

   To opt into v3, set the exact public Steward origin and enable federated
   subjects in the same rollout:

   ```yaml
   taskIdentity:
     enabled: true
     issuer: https://identity.example.test
     audience: steward-task-api
     resource: https://steward.example.test
     federatedSubjects:
       enabled: true
       autoAssociateFromConnections: true
     publicJwksConfigMap:
       name: steward-task-identity-jwks
       key: jwks.json
   ```

4. Verify the rendered apiserver carries `STEWARD_IDENTITY_TASK_ISSUER`,
   `STEWARD_IDENTITY_TASK_AUDIENCE`,
   `STEWARD_FEDERATED_TASK_IDENTITY_ENABLED`, and the read-only projection at
   `/run/identity-task/jwks.json` before installing. A v3 render also carries
   `STEWARD_TASK_AUTH_RESOURCE`.

Steward then accepts a submission token only when it is ES256 from that JWKS,
carries the exact issuer and audience, declares an enabled
`identity_contract`, presents a bounded `jti`, and is current within the token
age and clock-skew allowance. The default accepts only `steward-task-v2`; the
opt-in accepts v2 and `steward-task-v3`. A verified token is authentication
only; authority comes from Step 5.

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

The verified GitHub connection is also part of the default v3 sequence, but it
cannot be created while `config.taskOrchestrationMode=staged`. Complete the
browser identity, RBAC, template, and Envelope work in this step, then defer
GitHub Connect to Step 8 after task orchestration is active on every replica.
The runtime-free connection status read may be available during stage 1; that
does not make authorize, reauthorize, disconnect, or rerun mutations available.

For v2 enrollment and the v3 manual fallback, two identity outputs of this step
are inputs to Step 6:

- the opaque `usr_<...>` canonical user ID the person reads from `/settings`;
- the verified email bound to that canonical identity.

Finish this step when the person has an active provisioned User Envelope with
the intended revision and authority. If several are active, also record the
exact public `envelopeDigest` that the acceptance Task will select. The
capability catalog advertises models and tools but grants no authority, and an
empty catalog cannot narrow an Envelope that already admits a Task.

## Step 6: enroll v2 identity or verify a v3 association

For v2, Identity policy admits exact observed values, not patterns. Using the claims
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

For v2, finish this step when one real assertion exchanges successfully, a replay of
the same assertion is denied, and a wrong repository, ref, actor, and audience
are each denied with a fresh assertion.

For v3, Identity issues the stable subject
`github-actions:actor:<numeric-actor-id>` and need not stamp email or Steward
canonical-user groups. Configure and validate the v3 issuer, audience, and
claim shape in this step. With the default connection-association setting,
defer the connection-backed association proof to Step 8; staged orchestration
deliberately refuses the required Connect mutation.

If connection proof is unavailable or
`taskIdentity.federatedSubjects.autoAssociateFromConnections=false`, the first
valid submission instead records the subject and returns
`task_identity_unassociated` without creating a Task. An authorized Steward
browser administrator inspects the observed subject, associates it with the
canonical user from Step 5 using `expectedRevision`, and verifies the append-only
audit endpoint before retrying. Never use actor login, display name, or email
similarity as association proof. A complete valid v2 compatibility identity may
seed only that same verified issuer/subject association.

## Step 7: verify Task submission remains disabled in core mode

Submit one otherwise-valid direct Git package invocation while Steward remains
in core mode. Steward must reject it with the staged-orchestration runtime
contract error before creating a Task, reserving authority, or creating an
AgentRuntime. Core mode can verify the API, webhook, browser administration
when enabled, and AgentRuntime admission; it does not admit or queue Tasks.

Also exercise a wrong audience and untrusted issuer or CA. Those credentials
must fail authentication, while the otherwise-valid request must fail at the
staged orchestration boundary. Record the distinct public error categories and
confirm that no Task or runtime record was created by any attempt.

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
the staged ownership switches. After task orchestration is active on every
replica, have the signed-in person connect the same GitHub account that will
trigger the workflow. Verify that status reports its immutable numeric account
ID, the corresponding `github-actions:actor:<id>` subject is associated with
that person's canonical user, and the audit records method
`connection-verification`, provider `github`, and that numeric ID. Treat
“Connect works” as a stage-2 acceptance check and complete it before the first
v3 Task submission. This connection does not create a User Envelope or grant
Task authority. The prerequisites and their verification live in the
[installation guide](installation-guide.md); this page adds only their position
in the order.

## Step 9: accept governed execution end to end

Submit the same package in a new governed job with known inputs and an
expected output hash. Confirm the Task reaches its terminal success phase, the
output hash matches, the Run detail retains the exact User Envelope evidence,
and the runtime is finalized.

Record source revisions, artifact digests, the GitHub run identifier, the
bounded Task UID and status, HTTP status, and public JWKS `kid`s only. Never
record tokens, authorization headers, policy mappings, or response bodies.
