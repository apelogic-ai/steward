# Deferred post-M1 implementation contracts

Status: consolidated deferred-contract register; no item is an implemented or frozen API

This is the only current list of unresolved post-M1 wire and implementation contracts.
It does not reopen the [frozen M1 contract](../contracts/m1/v1/README.md) or weaken the
[accepted post-M1 semantics](README.md). Each item requires its own architecture and
contract decision before production implementation. Historical decision tables are not
inputs to that work unless a new decision explicitly adopts them.

| ID | Deferred contract | Constraint already accepted |
|---|---|---|
| V2-D01 | Exact AgentInstance CRD group, version, fields, conditions, and compatibility path | Desired state passes Steward admission; controller alone writes current phase; M1 is unchanged. |
| V2-D02 | Exact AgentSession HTTP and streaming schemas | Session is journal-backed, belongs to AgentInstance, carries no authority, and uses typed SessionRelay events. |
| V2-D03 | Resident-agent Task dispatch protocol | Every invocation remains an admitted Task bound immutably to one TaskDefinition and exact instance/runtime resolution. |
| V2-D04 | Durable storage implementation, quotas, retention, and state compatibility | Only explicitly declared AgentInstance-owned durable state survives runtime replacement. |
| V2-D05 | Exact managed-agent Mint service identifier and any additional claims | Canonical owner and current workload identity are server-resolved; clients and artifacts cannot select service identity. |
| V2-D06 | Queue bounds, timeouts, drain periods, and idle policy | Initial execution is bounded and single-lane; control operations take precedence. |
| V2-D07 | Team, shared, transferable, and pure-service AgentInstances | Initial AgentInstances are non-transferable standing delegations for one canonical user. |
| V2-D08 | Cross-user collaboration | Initial sessions do not grant cross-user participation or authority. |
| V2-D09 | TaskGraph orchestration | No TaskGraph exists in the accepted baseline; a future contract must justify Steward-owned graph semantics. |
| V2-D10 | State migration or checkpoint format | No opaque runtime checkpoint or silent interrupted-Task replay exists initially. |

Add a newly discovered deferred post-M1 contract here only when it does not already
belong to an accepted semantic boundary or an existing versioned contract. Numerical
values, schemas, and protocol shapes remain unresolved until their providing ticket is
accepted and merged.
