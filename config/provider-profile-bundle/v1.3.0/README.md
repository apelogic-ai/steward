# Steward runtime-provider profile bundle 1.3.0

This immutable bundle supersedes `steward-runtime-providers@1.2.2`. It keeps
the existing profile identities, endpoint hosts, token grants, Mint audience,
CIDRs, and inference policy. The MCP-GW profile replaces its broad HTTP method
policy with exact `POST` and `DELETE` rules for the existing `/mcp` transport,
the four routes used by the GitHub connections bridge, and the two helper run
routes: `POST /cli/run` and `POST /shell/run`. It also requires the sandbox helper executable at
`/usr/local/bin/mcp-gw-sandbox-helper`.

The network profile only permits the existing transport and bridge operations
plus the helper run routes. MCP-GW
still checks the run-bound command class or remote-shell grant, target, and run
identifier for every request. The fixed runtime wrappers provide
explicit helper operations, while the adapters provide
`openshell-token-grant-placeholder` when governed tools are granted. The marker
is not a credential. OpenShell replaces it with
a short-lived token only on the network request and never exposes that usable
token to the agent.

`mint-audience` must equal `config.mint.audience`. Service CIDRs must be
canonical and must not overlap OpenShell's always-blocked IPv4, IPv6, or
IPv4-mapped ranges.

## Upgrade from 1.2.2

Enable OpenShell's global `providers_v2_enabled=true` setting, stop both profile
consumers (the Steward controller and apiserver), retain the prior rendered
profiles, and run the transition from the exact Steward source revision that
contains this bundle:

```sh
cargo xtask provider-profile-bundle upgrade \
  --from-inputs inputs-1.2.2.json \
  --inputs inputs-1.3.0.json \
  --output rendered-profiles
```

Re-apply both profiles under their existing IDs, rerun the platform preflight,
update Steward with the regenerated profile digests, and then restart the
controller and apiserver. The transition accepts only an exact 1.2.2
predecessor and rejects any delta beyond the exact bridge and run routes and helper
binary.

The standalone installer supports fresh install and reconcile, not this
same-ID transition. Fresh installations use the released installer after the
bundle is included in a Steward release:

```sh
bundle=provider-profile-bundle/v1.3.0
tool="$bundle/bin/steward-provider-profile"
cp "$bundle/examples/inputs.json" inputs-1.3.0.json
"$tool" validate --bundle "$bundle" --inputs inputs-1.3.0.json
"$tool" install --bundle "$bundle" --inputs inputs-1.3.0.json --output rendered-profiles
"$tool" reconcile --bundle "$bundle" --inputs inputs-1.3.0.json --output rendered-profiles
```

## Rollback to 1.2.2

Disable remote shell first, then stop the Steward controller and apiserver.
Restore the retained 1.2.2 rendered profiles under their existing IDs and
restore the previous profile digests in Steward. Restore the previous Steward
and MCP-GW configuration together, then restart the controller and apiserver.
Do not use the standalone installer for this same-ID rollback.

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

Confirm the provenance binds the expected repository and commit. A source-only validation
does not make an asset eligible for release.
