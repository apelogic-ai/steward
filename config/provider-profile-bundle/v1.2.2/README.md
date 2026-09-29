# Steward runtime-provider profile bundle 1.2.2

This immutable patch bundle supersedes `steward-runtime-providers@1.2.1`.
It preserves the same runtime identities, provider endpoints, token grants,
Mint audience, CIDRs, capabilities, and binaries. Its only policy change is an
explicit MCP transport method list that includes `DELETE`, which MCP clients
use to close sessions. Application authority remains read-only and is still
enforced by MCP-GW.

`mint-audience` must equal `config.mint.audience`. Service CIDRs must be
canonical and must not overlap OpenShell's always-blocked IPv4, IPv6, or
IPv4-mapped ranges.

## Upgrade from 1.2.1

Enable OpenShell's global `providers_v2_enabled=true` setting, stop both profile
consumers (the Steward controller and apiserver), retain the prior rendered
profiles, and render the exact transition from the v0.3.4 source tree:

```sh
cargo xtask provider-profile-bundle upgrade \
  --from-inputs inputs-1.2.1.json \
  --inputs inputs-1.2.2.json \
  --output rendered-profiles
```

Re-apply both profiles under their existing IDs, rerun the platform preflight,
upgrade Steward using the regenerated values and profile digests, and then
restart the controller and apiserver. The transition accepts only an exact
1.2.1 predecessor and rejects every delta except the MCP transport method
correction.

The released standalone installer supports fresh install and reconcile, not
this same-ID transition; the transition deliberately requires the pinned
v0.3.4 source and its `cargo xtask` gate.

Fresh installations use the released installer:

```sh
bundle=provider-profile-bundle/v1.2.2
tool="$bundle/bin/steward-provider-profile"
cp "$bundle/examples/inputs.json" inputs-1.2.2.json
"$tool" validate --bundle "$bundle" --inputs inputs-1.2.2.json
"$tool" install --bundle "$bundle" --inputs inputs-1.2.2.json --output rendered-profiles
"$tool" reconcile --bundle "$bundle" --inputs inputs-1.2.2.json --output rendered-profiles
```

Verify the release asset digest and GitHub attestation against the exact
repository and source commit before rendering or installation.

## Verify a released bundle before rendering or installation

Use the release handoff's exact asset digest, source repository and exact source commit,
plus the signer identity:

```sh
gh attestation verify "$asset" \
  --repo "$repository" \
  --cert-identity "$signer_identity" \
  --format json > "${asset}.provenance.json"
actual_digest="sha256:$(sha256sum "$asset" | awk '{print $1}')"
test "$actual_digest" = "$expected_digest"
```

Confirm the provenance binds the expected repository and commit. A source-only validation does not make an asset eligible for release.
