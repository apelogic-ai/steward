# Steward runtime-provider profile bundle 1.2.1

This immutable patch bundle supersedes `steward-runtime-providers@1.2.0` for
stock OpenShell 0.0.98. It keeps application authority read-only while allowing
the POST transport used by MCP JSON-RPC and connection operations. Both
provider profiles obtain their token-grant audience from the same deployment
input so they match the single audience configured on Steward Mint.

`mint-audience` must equal `config.mint.audience`. Service CIDRs must be
canonical. This bundle rejects unrestricted, IPv4 loopback, IPv4 link-local,
IPv6 unspecified, and IPv6 loopback CIDRs before OpenShell loads the profile.

## Upgrade from 1.2.0

The released `steward-provider-profile` binary supports fresh install and
reconcile operations, but not this version transition. Perform the upgrade in
this order:

1. Check out the exact Steward `v0.3.3` tag on a machine with its pinned Rust
   toolchain.
2. Keep the original 1.2.0 inputs, create 1.2.1 inputs with the same origins and
   CIDRs, and add the configured Mint audience to both profiles.
3. Stop every workload that consumes either profile, then render the validated
   transition from the v0.3.3 checkout:

```sh
cargo xtask provider-profile-bundle upgrade \
  --from-inputs inputs-1.2.0.json \
  --inputs inputs-1.2.1.json \
  --output rendered-profiles
```

4. Re-apply both rendered profiles to OpenShell under the existing
   `steward-mcp-gw` and `steward-litellm` IDs. This same-ID replacement is the
   explicit governed exception to the normal installed-ID immutability rule:
   connection operations select `steward-mcp-gw` directly, so installing the
   corrected policy under a new ID would leave them on the old read-only
   transport policy.
5. Rerun the Steward platform preflight with bundle 1.2.1. Use its regenerated
   Helm values and execution-binding profile digests for the Steward Helm
   upgrade.
6. Restart the profile-consuming workloads only after the preflight and Helm
   upgrade succeed, then verify MCP connection operations and inference.

The transition accepts only the exact 1.2.0 predecessor. It permits only the
shared audience correction and the MCP endpoint transport-access correction;
every other profile or environment change fails closed. Keep the stopped
consumer and the prior rendered profiles available until verification so a
failed transition can be rolled back as one unit.

Fresh installations use the released installer and do not require a Steward
checkout or Rust toolchain:

```sh
bundle=provider-profile-bundle/v1.2.1
tool="$bundle/bin/steward-provider-profile"
cp "$bundle/examples/inputs.json" inputs-1.2.1.json
# Replace only the declared environment origins, CIDRs, and Mint audience.
"$tool" validate --bundle "$bundle" --inputs inputs-1.2.1.json
"$tool" install --bundle "$bundle" --inputs inputs-1.2.1.json --output rendered-profiles
"$tool" reconcile --bundle "$bundle" --inputs inputs-1.2.1.json --output rendered-profiles
```

## Verify a released bundle before rendering or installation

Use the release handoff's exact asset digest, source repository and exact source commit.
Verify provenance before extraction:

```sh
gh attestation verify "$asset" \
  --repo "$repository" \
  --cert-identity "$signer_identity" \
  --format json > "${asset}.provenance.json"
actual_digest="sha256:$(sha256sum "$asset" | awk '{print $1}')"
test "$actual_digest" = "$expected_digest"
```

Confirm the provenance contains the expected repository and commit. A
source-only validation does not make an asset eligible for release.
