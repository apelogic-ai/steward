# Steward runtime-provider profile bundle 1.2.1

This immutable patch bundle supersedes `steward-runtime-providers@1.2.0` for
stock OpenShell 0.0.98. It keeps application authority read-only while allowing
the POST transport used by MCP JSON-RPC and connection operations. Both
provider profiles obtain their token-grant audience from the same deployment
input so they match the single audience configured on Steward Mint.

`mint-audience` must equal `config.mint.audience`. Service CIDRs must be
canonical and must not overlap OpenShell's always-blocked unspecified,
loopback, or link-local ranges.

## Upgrade from 1.2.0

Keep the original 1.2.0 inputs, create 1.2.1 inputs with the same origins and
CIDRs, and add the configured Mint audience to both profiles. Stop the profile
consumer and run:

```sh
cargo xtask provider-profile-bundle upgrade \
  --from-inputs inputs-1.2.0.json \
  --inputs inputs-1.2.1.json \
  --output rendered-profiles
```

The migration accepts only the exact 1.2.0 predecessor. It permits only the
shared audience correction and the MCP endpoint transport-access correction;
every other profile or environment change fails closed.

Fresh installations use the released installer:

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
