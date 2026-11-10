# Governed CLI and remote shell

> **Release scope: Steward v0.3.30.** This guide requires MCP-GW 0.8.0 or later. Earlier Steward and
> MCP-GW releases do not ship the complete helper and route set.

Steward uses its existing exact tool-grant tuples for governed command-line access. MCP-GW
classifies each command and requires one matching grant from the Task. Remote shell requires its
own write grant. Add only the packs and classes that the installation actually runs:

```yaml
config:
  apiserver:
    capabilityCatalog:
      schemaVersion: steward.capability-catalog/v2
      models: [] # keep the installation model entries here
      tools:
        - { provider: cli, resource: kubectl, action: read, accessClass: read }
        - { provider: cli, resource: kubectl, action: write, accessClass: write }
        - { provider: cli, resource: kubectl, action: destructive, accessClass: destructive }
        - { provider: remote-shell, resource: remote_shell_run, action: write, accessClass: write }
      catalogs: [] # keep the installation provider-catalog entries here
```

The capability catalogue supplies choices to the Envelope editor; it grants nothing by itself.
An administrator includes the exact tuples in an Envelope template or approved request. A Task
may use all or any subset of its Envelope grants. A different CLI name or class, or a remote-shell
grant absent from the Envelope, is refused. The administrator editor reads the deployment
catalogue through `/admin/api/v1/capabilities`. The user editor reads the allowed tuples from the
selected Envelope template through the user Envelope API. Both editors submit exact API tuples;
do not maintain a second list in the web deployment.

## Bind a machine to one Run

Remote shell accepts a configured target role rather than an address or instance ID. Declare the
static role-to-instance mapping in the deployment-owned execution binding:

```yaml
remoteShellTargets:
  engineer: i-0123456789abcdef0
```

The apiserver validates the mapping, incorporates it into the binding digest, and snapshots the
exact binding with the Task. The controller projects that snapshot into an immutable runtime
annotation writable only by trusted Steward identities. Mint reads only that trusted annotation
and signs the mapping as `steward.remote_shell_targets` when the runtime also has the exact
`remote-shell/remote_shell_run/write` grant.

MCP-GW v0.8.0 resolves the requested role only from that verified signed claim. A missing or
malformed binding, a role absent from the mapping, a sibling grant tuple, or a Task without the
remote-shell grant remains refused before any Systems Manager command runs. The agent can select a
declared role but cannot provide an instance ID, edit the mapping, or borrow another runtime's
binding. Keep cloud credentials and instance IDs out of Task input, harness configuration, agent
output, and logs.

Changing the deployment mapping affects only Tasks created afterward. A running Task keeps the
mapping snapshotted when it was created and may continue receiving tokens for it. To revoke remote
shell from a running Task, cancel or terminate the Task, remove its remote-shell grant, or disable
remote shell at the gateway.

This first slice is static per deployment binding. Dynamic per-run allocation through an
authenticated provisioner is tracked separately in
[hypershell-platform#78](https://github.com/apelogic-ai/hypershell-platform/issues/78); do not put a
dynamic target in the Task spec while that path is absent.
