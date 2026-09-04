# Upgrade note: explicit execution bindings

Installations that previously relied on an implicit coding-agent image or default logical agent
must configure `steward.execution-bindings/v1` before publishing or submitting new versioned
Workflows. Steward no longer synthesizes a coding agent.

Before rollout:

1. Inventory the exact logical references stored in executable Workflow revisions.
2. For each reference that must remain executable, configure its immutable image digest, absolute
   executable, exact version probe, and tool/inference provider-profile IDs and digests.
3. Install those immutable OpenShell profiles under the configured IDs.
4. Run `steward-apiserver-bin validate-execution-bindings --file <catalog>` and retain the printed
   derived identities with the deployment review.
5. Roll out the structured Helm value from
   [the installation contract](execution-bindings.md#helm-installation).

Historical Workflow revisions and Task rows are not rewritten. Existing Task rows that already
contain a binding continue from that snapshot. Rows without a binding remain readable and do not
receive a manufactured binding. New submissions for a Workflow whose reference is absent fail
before reservation. Rollback restores the previous catalog; it does not reinterpret queued Tasks.
