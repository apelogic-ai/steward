# Codex reference runtime

Steward releases one supported reference image for `codex@0.140.0` on `linux/amd64`. The release
handoff records its OCI reference and immutable digest under `referenceRuntimes.codex`. The image is
built from [`build/codex-reference.Dockerfile`](../../build/codex-reference.Dockerfile), with both
the OpenShell sandbox base and Codex CLI version pinned.

Use the digest from the release handoff, never the convenience tag, in an execution binding:

```yaml
agentRef: codex@0.140.0
adapter: codex-v1
image: ghcr.io/apelogic-ai/steward-codex-runtime@sha256:<release-digest>
executable: /usr/bin/codex
versionProbe:
  arguments: [--version]
  expectedStdout: codex-cli 0.140.0
models:
  providers: [openai]
```

Install the provider-profile bundle from the same Steward release and record the rendered profile
IDs and digests in the binding. The release builds the image for `linux/amd64`, verifies its pinned
CLI version during the build, scans it, and publishes its immutable digest, SBOM, and provenance.

## Included command-line tools

The non-root `sandbox` user can use these pinned tools for repository inspection and structured
output processing:

| Command | Version | Purpose |
|---|---:|---|
| `rg` | 14.1.0 | Fast recursive text search |
| `jq` | 1.7.1 | JSON inspection and transformation; also used by the launch step to read the model's context window |
| `fd` | 9.0.0 | File and directory search |
| `tree` | 2.1.1 | Directory-tree inspection |
| `kubectl` | MCP-GW 0.8.2 helper | Governed Kubernetes commands |
| `remote-shell` | MCP-GW 0.8.2 helper | Governed commands on a run-bound machine |
| `uv` | 0.11.15 | Python project and package management |

## Governed commands and sign-in

The reference image builds the MCP-GW 0.8.2 sandbox helper from source and
installs it at `/usr/local/bin/mcp-gw-sandbox-helper`. The source is vendored
and pinned in `third_party/mcp-gw-sandbox-helper/cff55e2b` (mcp-gw release
v0.8.2), and a build stage compiles it with `bun build --compile` in the pinned
`oven/bun:1.2.21` image, so no prebuilt helper binary is downloaded. The
compiled helper embeds the Bun 1.2.21 runtime from that pinned image. Steward
v0.3.34 and earlier downloaded the prebuilt helper from mcp-gw's GitHub
releases instead. The `kubectl` and `remote-shell`
wrappers select the matching helper operation explicitly and pass the
placeholder run token and the matching route to the helper as options, because
the sandbox supervisor can start the helper with an empty environment. For the
same reason they pass the sandbox's egress proxy (`HTTPS_PROXY`) and CA bundle
(`SSL_CERT_FILE`) when the sandbox sets them: the sandbox allows egress only
through its proxy, which terminates TLS with its own CA. The helper does not
contain a gateway credential or implement a second sign-in flow.

When a Task has the matching approved grant, its agent adapter adds only the
matching route values to the agent process:

- `MCP_GW_CLI_URL=https://<configured-gateway-host>/cli/run` for an approved
  `cli/kubectl` command class
- `MCP_GW_SHELL_URL=https://<configured-gateway-host>/shell/run` for an
  approved `remote-shell/remote_shell_run/write` grant

The URLs are derived from the Task's configured MCP endpoint. An unrelated or
unrecognized grant adds none of these values.

`MCP_GW_RUN_TOKEN` is not present in the agent environment. The adapter uses
the non-secret `openshell-token-grant-placeholder` marker when governed tools
are granted, and OpenShell substitutes
a short-lived, run-bound token only on the allowed network request. MCP-GW then
checks the token's run identifier, target, and approved command class. A
missing grant, a command classified above the approved class, or a machine not
tagged for that run is refused by MCP-GW. Helper configuration and request
failures return exit code `125` and do not print the placeholder or
substituted token. A configuration failure names a fixed reason, such as
`Sandbox helper configuration failed: missing MCP_GW_CLI_URL` when the Task has
no approved `cli/kubectl` grant.

Use `steward-runtime-providers@1.3.0` with an image containing the helper. Its
MCP-GW profile preserves exact `POST /mcp` and `DELETE /mcp` access for the
existing tool transport, preserves the four exact GitHub connections-bridge
operations, and adds only `POST /cli/run` and `POST /shell/run` on the
configured gateway host.
Prefix and sibling paths, other hosts, and all other methods remain blocked.
The route policy matches the HTTP method and URL path; query input is handled
by the endpoint without broadening that route. The same Task and Envelope API
grants drive native UI submissions; the UI does not create a separate command
or sign-in path.

Example agent commands are:

```sh
kubectl get pods
remote-shell machine.test -- uname -a
```

Operators must bind remote-shell targets to the same run identifier used in
the Task token. Do not inject a gateway bearer token, token file, cloud access
key, or Systems Manager credential into the image, execution binding, Task, or
agent environment.

## Mirror the exact manifest

Use the released [daemonless registry mirror](registry-mirroring.md). Add an
explicit `referenceRuntimes.codex` target to the mapping, review the no-write
plan, and run `mirror`. The resulting deployment lock supplies the exact
digest-pinned value at `executionBindingImages.codex`; use that value in the
binding. The lock retains the source and target coordinates and digests as
deployment evidence.

For an existing Docker-only mirror automation, the equivalent manifest copy is
`docker buildx imagetools create --tag <target> <source@digest>`. It must copy
the source digest without rebuilding it; use the resulting target digest in
the binding. New installations should use the released ORAS-based mirror so a
Docker daemon is not required.

## Build a compatible private runtime

Forks may build an equivalent image from the public definition instead of mirroring the released
manifest:

```sh
docker buildx build \
  --platform linux/amd64 \
  --file build/codex-reference.Dockerfile \
  --provenance=mode=max \
  --sbom=true \
  --tag registry.example.test/agents/codex:0.140.0 \
  --push .
```

The build downloads only these pinned inputs:

- the base image `ghcr.io/nvidia/openshell-community/sandboxes/base` and the build image
  `docker.io/oven/bun:1.2.21`, both by digest;
- `uv` from `github.com/astral-sh/uv/releases`, checked by SHA-256;
- Ubuntu packages (`ripgrep`, `jq` with `libjq1` and `libonig5`, `fd-find`, `tree`) from
  `mirrors.edge.kernel.org`, checked by SHA-256;
- `@openai/codex` and `tar` from the npm registry, at exact versions.

The MCP-GW sandbox helper is compiled from the vendored source in the repository, so the build
needs no access to mcp-gw's GitHub releases. The build context must include
`third_party/mcp-gw-sandbox-helper/`.

Verify a private image's platform, non-root user, CLI version, and provider-profile compatibility
before activation. From Steward 0.3.36, include `curl` and `jq` so the launch step can read the
selected model's context window from the model gateway; without them, Codex uses its own default.
It is independently built and operated; the released reference digest remains the reproducible
Steward artifact.
