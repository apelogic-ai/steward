# Execution bindings

Status: current installation contract

Execution bindings are public deployment metadata that connect an opaque Workflow `agentRef` to
one immutable runtime contract. They contain no credentials. Steward owns parsing, binding
resolution, durable Task snapshots, adapter behavior, and fail-closed execution. The deployment
owns every agent reference, label, version, image digest, executable, version probe, provider
profile ID and profile digest, and installs those OpenShell profiles. Workflow administrators may
select only an advertised `agentRef`; browser and Task requests cannot provide any execution field.

A clean installation has an empty catalog. It starts normally, advertises zero coding agents,
keeps historical Workflow revisions readable, rejects new Workflow publication as configuration
unavailable, and rejects new versioned Task submission before reservation. Steward never invents
a default agent, image, executable, or provider profile.

## Catalog schema

The complete example is [`execution-bindings.example.json`](execution-bindings.example.json).
The top-level object and every nested object reject unknown fields.

| Field | Required | Contract |
|---|---:|---|
| `apiVersion` | yes | Exact value `steward.execution-bindings/v1`. |
| `bindings` | yes | Zero to 128 entries; duplicate `agentRef` or derived identities fail startup. |
| `agentRef` | yes | Opaque exact deployment value, 3–255 bytes, one `@`, lowercase bounded name, version beginning with a digit; `latest` is invalid. |
| `displayName` | no | Presentation only, 1–200 bytes, trimmed, no control characters. It is never an ownership key. |
| `adapter` | yes | Product-supported adapter. This release accepts only `codex-v1`. |
| `image` | yes | Lowercase OCI repository plus exact `@sha256:` and 64 lowercase hexadecimal digits. Tags are rejected. |
| `executable` | yes | Absolute normalized path, 2–1024 bytes; no empty, `.` or `..` components. |
| `versionProbe.arguments` | yes | One to eight process arguments, each 1–128 bytes with no control characters. No shell parsing occurs. |
| `versionProbe.expectedStdout` | yes | Exact trimmed stdout, 1–255 bytes with no control characters. Any difference fails execution before the agent runs. |
| `providerProfiles.tools` | conditional | Required for Tasks with approved tools. |
| `providerProfiles.inference` | conditional | Required for Tasks with approved models. |
| `providerProfiles.*.id` | conditional | Exact OpenShell provider-profile ID, 1–255 bytes, no whitespace or control characters. |
| `providerProfiles.*.digest` | conditional | Exact `sha256:` plus 64 lowercase hexadecimal digits. |

The catalog is limited to 1 MiB. Steward serializes each validated entry in schema field order,
prefixes the schema domain, and derives a SHA-256 `bindingId`/`bindingDigest`. Operators do not
provide those values. Use the released validator to calculate and print them:

```sh
steward-apiserver-bin validate-execution-bindings \
  --file /path/to/execution-bindings.json
```

The command uses the production parser, contacts no external service, needs no credential, prints
only public `agentRef` and derived digest metadata, and returns nonzero for an invalid document.

## Agent image and adapter contract

`codex-v1` defines behavior, not a product-pinned Codex release. A compatible image must be
multi-architecture for every platform the deployment admits, contain the configured absolute
executable on each architecture, return the exact configured stdout for the configured probe
arguments, and implement the Codex-compatible configuration, `exec`, model, MCP, output-file, and
termination behavior used by the adapter. Obtain or build the image through the operator's normal
source, scanning, signing, and promotion process, then configure its immutable manifest digest.

Adding another compatible version is configuration-only: publish a distinct digest-pinned image,
choose a new exact `agentRef`, set its executable and probe, install its profiles, validate the
catalog, and roll out the ConfigMap. A new Steward release is needed only for a new adapter or
binding schema.

## OpenShell provider profiles

Create tool and inference provider profiles using the OpenShell provider-profile mechanism. Their
binary allowlist must include the configured executable path for every image architecture, and
their endpoint and token-grant policy must match the deployment. Install policies outside Steward
under the exact configured IDs. Compute the configured digest from the immutable policy artifact
used by that installation and promote ID plus digest together; never mutate an installed ID in
place.

Pinned OpenShell v0.0.98 exposes profile selection by exact ID but does not expose an authenticated
content digest for an installed profile. Steward therefore persists the configured profile IDs and
digests in the Task binding, incorporates them in the binding digest, labels the sandbox with that
binding, selects only the exact IDs, and fails if an ID is unavailable or the sandbox binding
differs. The deployment boundary must guarantee that an installed profile ID is immutable and
corresponds to its configured digest. Steward does not substitute an allowlist or claim to verify
bytes OpenShell cannot report. Once OpenShell exposes an applied profile digest, the adapter can
verify it without changing the catalog schema.

Only the profile categories required by the approved Task are attached. A tool-free Task receives
no tool profile or MCP server. A Task with a model requires its inference profile. Missing required
metadata fails before sandbox creation.

## Helm installation

Configure the catalog structurally at `config.apiserver.executionBindings`:

```yaml
config:
  apiserver:
    executionBindings:
      apiVersion: steward.execution-bindings/v1
      bindings:
        - agentRef: example-agent@1.2.3
          displayName: Example Agent 1.2.3
          adapter: codex-v1
          image: registry.example.test/agents/example@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
          executable: /opt/example/bin/agent
          versionProbe:
            arguments: [--version]
            expectedStdout: example-agent 1.2.3
          providerProfiles:
            tools:
              id: example-tools-profile-v7
              digest: sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
            inference:
              id: example-inference-profile-v7
              digest: sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
```

The chart validates the structure, writes it to an immutable content-addressed ConfigMap, mounts it
read-only in the apiserver, sets `STEWARD_TASK_EXECUTION_BINDINGS_FILE`, and changes the pod-template
checksum when content changes. The catalog is deliberately a ConfigMap, not a Secret.

For non-Helm installations, mount the same JSON document and set
`STEWARD_TASK_EXECUTION_BINDINGS_FILE`. Inline `STEWARD_TASK_EXECUTION_BINDINGS_JSON` remains for
bounded integration environments; setting both is a startup error. Neither mechanism changes the
schema.

## Lifecycle, changes, and rollback

Workflow lists advertise exactly the currently configured references. Removing a binding prevents
new publication and new Task submission for that reference but does not delete or alter historical
Workflow revisions. Restore the same binding to make such revisions executable again.

Before reservation, Steward persists the schema version, derived identity/digest, reference,
adapter, image, executable, version probe, and required profile references. Controller reconciliation
uses that snapshot only. An idempotency-key retry returns the existing reservation before consulting
the current catalog. Catalog edits therefore affect only new Tasks; queued Tasks retain their old
image and profiles. Runtime image, version-probe, profile selection, and binding-label mismatch fail
closed and request normal Task finalization.

To roll forward, validate the new file, install the referenced immutable profiles and images, then
roll out the catalog. To disable an agent, first stop publishing new Workflow revisions, allow or
finalize queued work, then remove the entry. To roll back, restore the previous content-addressed
catalog and leave any already-reserved Tasks alone. Do not reuse an `agentRef` for different bytes;
use a new versioned reference.

## Errors and troubleshooting

- `unsupported execution binding catalog API version`: use the exact documented `apiVersion`.
- `execution binding catalog is invalid`: fix JSON shape, unknown fields, or a field limit.
- `logical agent ... has no deployment execution binding`: add/restore the exact reference; no Task
  was reserved.
- `no coding agents are configured for Workflow publication`: configure at least one binding.
- `has no tool provider profile` / `has no inference provider profile`: add the category required
  by the approved Envelope.
- Image mismatch: confirm the OCI manifest digest, runtime architecture, and persisted binding.
- Version mismatch: execute the configured absolute path with the exact probe argument vector and
  compare stdout byte-for-byte; stderr does not satisfy the probe.
- Profile unavailable: install the exact profile ID in the runtime workspace and confirm its
  immutable policy digest in the deployment system.
- Binding mismatch: do not relabel or reuse a sandbox; allow exact-runtime finalization and submit a
  new Task.

Catalog metadata is not secret, but it is security-relevant. Keep credentials exclusively in the
normal OpenShell/token-grant path. Never put tokens, environment values, provider credentials, or
private registry credentials in this document.
