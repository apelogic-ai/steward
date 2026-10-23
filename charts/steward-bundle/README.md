# Steward bundle chart

`steward-bundle` installs Steward and its pinned governed-execution
dependencies as one Helm release. The standalone `steward` chart remains the
default when an installation manages those dependencies separately.

The chart enables Steward, SPIRE, agent-sandbox, OpenShell, and LiteLLM by
default. Actions Runner Controller is available but disabled by default. Set
`components.<name>.enabled` to select each chart independently. The bundle
creates no Namespace; create the release namespace plus `spire-system`,
`spire-server`, `agent-sandbox-system`, `openshell-sandboxes`, and any enabled
ARC namespace before installation.

## Existing Secret

All enabled charts must name one operator-created Secret. The default name is
`governed-execution`. Override the existing upstream paths together; the chart
refuses mismatched names and never accepts inline secret material. The
standalone chart READMEs list the required keys for each purpose.

| Purpose | Bundle value path |
|---|---|
| Steward database | `steward.secrets.database.name` |
| Managed inference database | `steward.secrets.managedInferenceDatabase.name` |
| Managed inference encryption | `steward.secrets.managedInferenceEncryptionKey.name` |
| Jira | `steward.secrets.jira.name` |
| LiteLLM client | `steward.secrets.litellm.name` |
| OpenShell client | `steward.secrets.openshellClient.name` |
| Mint signing and introspection | `steward.secrets.mint.name` |
| Steward API TLS | `steward.tls.api.secretName` |
| Steward webhook TLS | `steward.tls.webhook.secretName` |
| Optional Steward database CA | `steward.databaseTls.ca.name` when `steward.databaseTls.ca.kind` is `Secret` |
| Optional workload-exchange CA | `steward.workloadExchangeTrust.name` when `steward.workloadExchangeTrust.kind` is `Secret` |
| Optional trigger API CA | `steward.triggerReceiver.taskApi.ca.name` when `steward.triggerReceiver.taskApi.ca.kind` is `Secret` |
| Optional browser client | `steward.browserAuth.google.clientSecret.name` |
| Optional source repository | `steward.githubSource.privateKeySecret.name` |
| Optional Ingress TLS | `steward.web.ingress.tlsSecretName` |
| Optional trigger database | `steward.triggerReceiver.databaseSecret.name` |
| Optional trigger signing keys | `steward.triggerReceiver.triggers[].signingSecret.name` |
| Optional SPIRE database | `spire.spire.spire-server.dataStore.sql.externalSecret.name` |
| OpenShell database | `openshell.openshell.server.externalDbSecret` |
| OpenShell server TLS | `openshell.openshell.server.tls.certSecretName` |
| OpenShell client CA | `openshell.openshell.server.tls.clientCaSecretName` |
| OpenShell client TLS | `openshell.openshell.server.tls.clientTlsSecretName` |
| OpenShell sandbox JWT | `openshell.openshell.server.sandboxJwt.signingSecretName` |
| LiteLLM master key | `litellm.litellm.masterkeySecretName` |
| LiteLLM environment | `litellm.litellm.environmentSecrets[]` |
| LiteLLM database | `litellm.litellm.db.secret.name` |

Image-pull Secrets are registry authentication and are not part of this
application-purpose Secret. The release packager replaces the Steward image
tags and digests with evidence from the images built for that same release.

Before starting Steward, replace
`steward.tls.webhook.caBundlePem` with the public CA that signs the webhook
certificate stored in that Secret. The non-secret default exists only so the
chart can be rendered and installed before platform-owned certificates are
available; workloads are not ready until the matching certificate is present.

## Image values

The bundle adds no shared image keys. Use the child charts' supported paths:

| Image | Bundle value path |
|---|---|
| Steward API | `steward.images.apiserver.repository`, `steward.images.apiserver.tag`, `steward.images.apiserver.digest` |
| Steward controller | `steward.images.controller.repository`, `steward.images.controller.tag`, `steward.images.controller.digest` |
| Steward Mint | `steward.images.mint.repository`, `steward.images.mint.tag`, `steward.images.mint.digest` |
| Steward web | `steward.images.web.repository`, `steward.images.web.tag`, `steward.images.web.digest` |
| Steward trigger receiver | `steward.images.triggerReceiver.repository`, `steward.images.triggerReceiver.tag`, `steward.images.triggerReceiver.digest` |
| SPIRE server | `spire.spire.spire-server.image.registry`, `spire.spire.spire-server.image.repository`, `spire.spire.spire-server.image.tag` |
| SPIRE controller | `spire.spire.spire-server.controllerManager.image.registry`, `spire.spire.spire-server.controllerManager.image.repository`, `spire.spire.spire-server.controllerManager.image.tag` |
| SPIRE chown helper | `spire.spire.spire-server.chown.image.registry`, `spire.spire.spire-server.chown.image.repository`, `spire.spire.spire-server.chown.image.tag` |
| SPIRE agent | `spire.spire.spire-agent.image.registry`, `spire.spire.spire-agent.image.repository`, `spire.spire.spire-agent.image.tag` |
| SPIRE filesystem-group helper | `spire.spire.spire-agent.fsGroupFix.image.registry`, `spire.spire.spire-agent.fsGroupFix.image.repository`, `spire.spire.spire-agent.fsGroupFix.image.tag` |
| SPIRE alternate-socket helper | `spire.spire.spire-agent.socketAlternate.image.registry`, `spire.spire.spire-agent.socketAlternate.image.repository`, `spire.spire.spire-agent.socketAlternate.image.tag` |
| SPIFFE CSI driver | `spire.spire.spiffe-csi-driver.image.registry`, `spire.spire.spiffe-csi-driver.image.repository`, `spire.spire.spiffe-csi-driver.image.tag` |
| CSI registrar | `spire.spire.spiffe-csi-driver.nodeDriverRegistrar.image.registry`, `spire.spire.spiffe-csi-driver.nodeDriverRegistrar.image.repository`, `spire.spire.spiffe-csi-driver.nodeDriverRegistrar.image.tag` |
| agent-sandbox controller | `agentSandbox.agentSandbox.image.repository`, `agentSandbox.agentSandbox.image.tag` |
| OpenShell gateway | `openshell.openshell.image.repository`, `openshell.openshell.image.tag` |
| OpenShell supervisor | `openshell.openshell.supervisor.image.repository`, `openshell.openshell.supervisor.image.tag` |
| OpenShell default sandbox | `openshell.openshell.server.sandboxImage` |
| LiteLLM | `litellm.litellm.image.repository`, `litellm.litellm.image.tag` |
| Actions Runner Controller | `arc.arc.image.repository`, `arc.arc.image.tag` |

Every default image is digest-pinned. Child schemas reject mutable dependency
image tags. Steward keeps digest enforcement enabled.

## Routes

The optional OpenShell and LiteLLM routes use `openshell.route.*` and
`litellm.route.*`. Enabling either route requires its existing Gateway parent,
hostname, and every timeout field supported by that child chart. No route is
created by default.

When SPIRE and OpenShell are both enabled,
`spire.spire.spire-server.controllerManager.identities.clusterSPIFFEIDs.openshell-sandboxes.namespaceSelector.matchLabels.kubernetes.io/metadata.name`
must equal `openshell.openshell.server.sandboxNamespace`. The chart refuses a
change to only one path so sandbox identities cannot silently select the wrong
namespace.

## CRDs and permissions

Helm installs every packaged `crds/` directory before rendering dependent
objects. This includes Steward, SPIRE, and agent-sandbox CRDs. The child chart
READMEs describe the cluster-wide RBAC, host access, and Pod Security settings
required by each controller and runtime.

Helm passes its reserved `global` values object to every dependency. Bundle
preparation permits that one object in the packaged copy of Steward's strict
values schema. The standalone source chart and its rendered objects are not
changed. The bundle's root `global` object must stay empty so it cannot bypass
a child chart's guarded values. Nested reserved `global` objects are also empty,
except for SPIRE's documented `spire.global.spire.*` configuration interface.
