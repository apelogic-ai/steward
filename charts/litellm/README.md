# LiteLLM dependency chart

This Steward release chart wraps the LiteLLM chart from the pinned `v1.106.2`
source archive. The archive checksum is recorded in `Chart.yaml`; preparation
verifies it before Helm packages the dependency. No upstream workload template
is copied into this wrapper.

## Values mapping

| Purpose | Upstream value |
|---|---|
| Proxy and migration image | `litellm.image.repository`, `litellm.image.tag` |
| Existing Secret | `litellm.masterkeySecretName`, `litellm.environmentSecrets[]`, `litellm.db.secret.name` |
| Secret keys | `litellm.masterkeySecretKey`, `litellm.db.secret.usernameKey`, `litellm.db.secret.passwordKey` |
| Existing configuration | `litellm.proxyConfigMap.name`, `litellm.proxyConfigMap.key` |
| Shared Gateway route | `route.parentRefs`, `route.hostname`, `route.timeouts.*` |

The image tag must include an `@sha256:` digest. Inline master keys, the
bundled database, generated proxy configuration, and multiple Secret names are
refused. The existing configuration must contain the model routes and provider
environment lookups needed by the installation.

## Platform requirements

Create the release namespace, existing database, Secret, and ConfigMap before
installing. The chart creates no Namespace and no cluster-scoped object. Its
pods can run under a restricted Pod Security profile when the supplied storage
and configuration do not add broader permissions.
