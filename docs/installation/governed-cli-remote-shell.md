# Governed CLI and remote shell

> **Release scope: Unreleased.** This guide requires MCP-GW 0.7.0 or later. Earlier Steward and
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

Remote shell accepts a configured target role rather than an address or instance ID. MCP-GW maps
that role to a machine and, before every command, requires the machine tag value to equal the
immutable runtime UID in the runtime-bound MCP-GW credential minted for the sandbox.

1. Submit and execute the Task through the normal Task API. Keep the returned `taskUid` only as
   the Task lookup key.
2. Poll `GET /v1/tasks/{taskUid}` with the same authenticated caller. Wait until `runtimeUid` is a
   non-empty string. The native Run details page shows the same API value as **Runtime**.
3. From a trusted provisioning pipeline, set the configured MCP-GW run tag on the target machine.
   The default key is `hypershell:run-id`; the value is the exact `runtimeUid`, not `taskUid`, a
   runtime name, or a value chosen by the caller.
4. Configure MCP-GW to map the Task identity and target role to that machine. Keep machine IDs,
   cloud credentials, and tag-writing permission out of the sandbox and agent-visible output.
5. Remove or replace the run tag during cleanup before assigning the machine to another Run.

Until both the role mapping and exact tag are present, MCP-GW refuses the remote command. A
missing tag, another Run's value, a tag lookup failure, a target role outside the Task identity's
mapping, or a Task without `remote-shell/remote_shell_run/write` remains refused. The trusted
provisioner should be the only identity allowed to change the run tag; the remote-shell service
needs read-only tag access and the narrowly scoped Systems Manager permissions documented by
MCP-GW.

Static long-lived host targets and signed bindings are not part of this release. Do not substitute
an Envelope ID, Task ID, instance ID, or sandbox-held credential for the runtime-UID tag check.
