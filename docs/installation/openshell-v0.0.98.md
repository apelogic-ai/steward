# OpenShell 0.0.98 governed execution

Steward v0.3.3 supports the stock OpenShell v0.0.98 release with the
sidecar supervisor topology. This is the supported compatibility shape for
this patch line; it does not require a patched OpenShell image.

## Required supervisor topology

Configure OpenShell with:

```yaml
supervisor:
  topology: sidecar
  sidecar:
    processBinaryAwareNetworkPolicy: true
```

On Kubernetes 1.35 or newer, also configure:

```yaml
supervisor:
  sideloadMethod: init-container
```

Do not combine the v0.0.98 `combined` supervisor topology with provider token
grants. The stock combined supervisor cannot mount the sandbox workload
identity required by those grants; the resulting request fails before it
reaches the provider. This is tracked in
[issue #147](https://github.com/apelogic-ai/steward/issues/147).

Steward continues to use the cluster/OpenShell default runtime. This topology
does not require an additional RuntimeClass and makes no VM-isolation claim.

## Sandbox SPIFFE identity

The SPIRE installation must create sandbox identities from OpenShell's v0.0.98
annotation and label contract. The corresponding SPIRE chart values are:

```yaml
global:
  spire:
    trustDomain: trust.example.test

spire-server:
  controllerManager:
    identities:
      clusterSPIFFEIDs:
        openshell-sandboxes:
          enabled: true
          spiffeIDTemplate: >-
            spiffe://{{ .TrustDomain }}/openshell/sandbox/{{ index .PodMeta.Annotations "openshell.io/sandbox-id" }}
          namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: openshell
          podSelector:
            matchLabels:
              openshell.ai/managed-by: openshell
```

Use the actual OpenShell namespace and the same trust domain accepted by
Steward Mint. The identity path is derived from `openshell.io/sandbox-id` for
OpenShell v0.0.98. The `openshell.ai/sandbox-id` annotation belongs to the
later v0.1 line and is not compatible with this release.

The sandbox identity uses the SPIFFE Workload API. A JWT issuer is not needed
for this provider-grant exchange. The OpenShell sandbox pod must receive the
SPIRE CSI socket selected by the token-grant configuration.

## Provider token grants

Enable SPIFFE-backed provider token grants in the OpenShell server:

```yaml
server:
  providerTokenGrants:
    spiffe:
      enabled: true
      workloadApiSocketPath: /spiffe-workload-api/spire-agent.sock
```

Provider grants are lazy. The `openshell-supervisor-network` sidecar requests a
grant when the governed process first calls a matching provider endpoint; a
successful sandbox start alone does not prove that the grant works. Recreate
the short-lived connection-operation sandbox after changing this configuration.
Recreate any longer-lived AgentRuntime that predates the identity or grant
change before using it as acceptance evidence.

Inspect the supervisor decisions with:

```sh
openshell logs <sandbox-name> --source sandbox --level info
```

At info level, the supervisor decision is an OCSF record; the sidecar may also
emit the corresponding warning. Read the `openshell-supervisor-network`
records for the first provider request.
A proxy policy or SSRF denial means the endpoint policy rejected the request.
An MCP-GW authorization denial means the request reached MCP-GW but its runtime
authority was rejected. Steward v0.3.3 preserves these as the distinct,
non-secret connection errors `proxy_policy_denied` and
`provider_authorization_failed`.

## Connections bridge coupling

The platform preflight renders the selected tools profile and requires all of
the following before it emits deployment values:

- `execution.connectionsBridge.mcpGatewayOrigin` has the same host and
  effective port as the rendered tools-profile endpoint;
- no rendered endpoint CIDR overlaps `0.0.0.0`, IPv4 loopback,
  IPv4 link-local, `::`, or `::1`, which OpenShell always blocks; and
- the selected tools profile contains
  `/usr/local/bin/steward-connections-bridge`.

The preflight also requires the exact same Mint audience in the MCP and
inference profiles, and writes that value to `config.mint.audience`. The MCP
profile uses POST-capable transport because MCP JSON-RPC and connection
operations are POST requests; MCP-GW and Mint still enforce the declared
read-only application authority.

`namespaces.runtime` is the workflow sandbox namespace generated into Steward
values. It may equal the control-plane namespace, but it must still be named
explicitly so a separated layout cannot silently place workloads elsewhere.

Use one exact MCP-GW origin in both the connection bridge and provider-profile
input. Do not work around a mismatch by widening provider egress.
