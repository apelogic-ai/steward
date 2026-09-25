# Registry mirroring and deployment locks

Each Steward release attaches `steward-registry-lock.sh` and
`steward-registry-lock.sh.sha256`. The tool plans and performs an explicit,
daemonless OCI copy of the released chart, every Steward image, and every
reference coding-agent runtime. It uses ORAS 1.3.0 and `jq`; it does not need a
Docker daemon.

Authenticate the source and target registries before running the tool. Keep
credentials in the normal ORAS registry configuration, keep shell tracing
disabled, and never put a password in a command argument, mapping document, or
deployment lock.

## Verify the tool

Download `release-handoff.json`, the mirror tool, and its checksum from the same
GitHub release. Verify the handoff attestation as described in that release,
then verify the tool:

```sh
sha256sum --check steward-registry-lock.sh.sha256
```

## Declare exact targets

Mirroring requires an explicit target for every artifact. The mapping keys must
exactly match the chart, images, and reference runtimes in the verified handoff;
missing and extra entries are rejected.

```json
{
  "schemaVersion": "steward.registry-mappings/v1",
  "artifacts": {
    "chart": "registry.example.test/team-a/charts/steward:0.2.6",
    "images.apiserver": "registry.example.test/team-a/steward:0.2.6-apiserver",
    "images.bridge": "registry.example.test/team-a/steward:0.2.6-bridge",
    "images.controller": "registry.example.test/team-a/steward:0.2.6-controller",
    "images.mint": "registry.example.test/team-a/steward:0.2.6-mint",
    "images.web": "registry.example.test/team-a/steward:0.2.6-web",
    "referenceRuntimes.codex": "registry.example.test/team-a/steward-codex-runtime:0.140.0-steward-0.2.6"
  }
}
```

## Plan, then mirror

The first command is a no-write plan. It resolves and validates every source
digest and checks that every target repository is reachable. Review the plan
before allowing writes.

```sh
set +x
./steward-registry-lock.sh plan \
  --handoff release-handoff.json \
  --mappings registry-mappings.json \
  --output steward-registry-plan.json

./steward-registry-lock.sh mirror \
  --plan steward-registry-plan.json \
  --output steward-deployment-lock.json
```

The copy is resumable. A target tag already resolving to the planned digest is
reported as `already-present` and skipped. A target tag resolving to any other
digest is rejected instead of overwritten.

To create a deliberately narrowed installation, add `--platform linux/amd64`
to `plan` and use distinct target tags in the mapping. Charts are always copied
as complete OCI artifacts; platform selection applies only to images and
reference runtimes.

For a local test registry without TLS, `plan` also accepts
`--target-plain-http`. Do not use that option for a production registry.

## Lock contract

`mirror` writes deterministic `steward.deployment-lock/v1` JSON. The lock
contains no credentials and records:

- the verified release version and commit;
- every source release digest and selected digest;
- every exact target tag and resulting digest;
- digest-pinned `chartValues` for Steward images;
- digest-pinned `executionBindingImages` for reference runtimes; and
- a Flux-ready chart source at
  `artifacts.chart.flux.ociRepository`, including `ref.digest`.

Identical verified inputs and registry state produce identical lock bytes. Keep
the complete lock as deployment evidence. Tags remain human-readable labels;
the recorded digests are the deployment authority.

The recommended next step is to embed the lock unchanged in the
[platform-preflight input](platform-preflight.md). The preflight validates its
release and artifact relationships before generating Helm/Flux configuration.
Without platform preflight, copy `chartValues` into the installation overlay,
use `artifacts.chart.flux.ociRepository` for the Flux `OCIRepository`, and copy
the required values from `executionBindingImages` into the corresponding
execution bindings.

The release gate exercises the same ORAS path against a run-owned registry,
including the chart, a reference runtime, digest preservation, and resumable
re-entry. Mocked tests cover deterministic output, incomplete mappings,
collisions, platform narrowing, and credential-free diagnostics.
