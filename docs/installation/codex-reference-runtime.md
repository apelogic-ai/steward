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
| `jq` | 1.7.1 | JSON inspection and transformation |
| `fd` | 9.0.0 | File and directory search |
| `tree` | 2.1.1 | Directory-tree inspection |
| `kubectl` | MCP-GW 0.7.0 helper | Governed Kubernetes commands |
| `remote-shell` | MCP-GW 0.7.0 helper | Governed commands on a run-bound machine |

## Governed commands and sign-in

The reference image installs the checksum-pinned MCP-GW 0.7.0 sandbox helper
at `/usr/local/bin/mcp-gw-sandbox-helper`. `kubectl` and `remote-shell` are
symlinks to that executable. The helper selects `/cli/run` or `/shell/run`
from its invoked name; it does not contain a gateway credential or implement a
second sign-in flow.

When a Task has the matching approved grant, both supported agent adapters add
only these values to the agent process:

- `MCP_GW_RUN_TOKEN=openshell-token-grant-placeholder`
- `MCP_GW_CLI_URL=https://<configured-gateway-host>/cli/run` for an approved
  `cli/kubectl` command class
- `MCP_GW_SHELL_URL=https://<configured-gateway-host>/shell/run` for an
  approved `remote-shell/remote_shell_run/write` grant

The URLs are derived from the Task's configured MCP endpoint. An unrelated or
unrecognized grant adds none of these values. The agent never receives a
usable token: OpenShell recognizes the placeholder and substitutes a
short-lived, run-bound token only on the allowed network request. MCP-GW then
checks the token's run identifier, target, and approved command class. A
missing grant, a command classified above the approved class, or a machine not
tagged for that run is refused by MCP-GW. Helper configuration and request
failures return exit code `125` with a generic message and do not print the
placeholder or substituted token.

Use `steward-runtime-providers@1.3.0` with an image containing the helper. Its
MCP-GW profile preserves exact `POST /mcp` and `DELETE /mcp` access for the
existing tool transport and adds exactly `POST /cli/run`, `POST /cli/jobs`,
`POST /shell/run`, and `POST /shell/jobs` on the configured gateway host.
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

Verify a private image's platform, non-root user, CLI version, and provider-profile compatibility
before activation. It is independently built and operated; the released reference digest remains
the reproducible Steward artifact.
