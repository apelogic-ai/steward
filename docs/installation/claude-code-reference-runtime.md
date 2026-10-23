# Claude Code reference runtime

Steward releases one supported `linux/amd64` image for `claude-code@2.1.222`. The release handoff
records its OCI reference and immutable digest under `referenceRuntimes.claudeCode`. The image is
built from [`build/claude-code-reference.Dockerfile`](../../build/claude-code-reference.Dockerfile)
with the OpenShell sandbox base, Claude Code, and its included command-line tools pinned.

Use the released digest, never the convenience tag, in an execution binding:

```yaml
agentRef: claude-code@2.1.222
adapter: claude-code-v1
image: ghcr.io/apelogic-ai/steward-claude-code-runtime@sha256:<release-digest>
executable: /usr/local/bin/claude
versionProbe:
  arguments: [--version]
  expectedStdout: 2.1.222 (Claude Code)
models:
  providers: [anthropic]
```

Configure the deployment's Anthropic-compatible LiteLLM base URL before activating this binding.
The adapter supplies that URL and only `openshell-token-grant-placeholder`; OpenShell replaces the
placeholder at the network edge. No usable model credential is stored in the image, sandbox
environment, or Claude configuration files.

The image entrypoint refuses to start Claude Code unless the base URL is an exact HTTP(S) URL, the
model key is the exact OpenShell placeholder, updates and nonessential traffic are disabled, and
alternate Claude, OAuth, cloud-provider, and custom-header inputs are absent. Refusals exit with
code 125 and do not print configuration values. The adapter also uses an empty per-run
configuration directory and disables session persistence.

## Included command-line tools

The non-root `sandbox` user can use these pinned tools:

| Command | Version | Purpose |
|---|---:|---|
| `rg` | 14.1.0 | Fast recursive text search |
| `jq` | 1.7.1 | JSON inspection and transformation |
| `fd` | 9.0.0 | File and directory search |
| `tree` | 2.1.1 | Directory-tree inspection |

## Mirror the exact manifest

Use the released [daemonless registry mirror](registry-mirroring.md). Add an explicit
`referenceRuntimes.claudeCode` target to the mapping, review the no-write plan, and run `mirror`.
The resulting deployment lock supplies the digest-pinned value at
`executionBindingImages.claudeCode`; use that value in the binding.

For an existing Docker-only mirror automation, the equivalent manifest copy is
`docker buildx imagetools create --tag <target> <source@digest>`. It must copy the source manifest
without rebuilding it; use the resulting target digest in the binding.

## Build a compatible private runtime

Forks may build an equivalent image from the public definition instead of mirroring the released
manifest:

```sh
docker buildx build \
  --platform linux/amd64 \
  --file build/claude-code-reference.Dockerfile \
  --provenance=mode=max \
  --sbom=true \
  --tag registry.example.test/agents/claude-code:2.1.222 \
  --push .
```

Before activation, verify the private image's platform, non-root user, exact version output,
launcher refusals, and provider-profile compatibility. A private rebuild has its own digest; the
released reference digest remains the reproducible Steward artifact.
