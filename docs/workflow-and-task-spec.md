# Historical Workflow and Task design exploration

Status: superseded historical exploration; not implemented, accepted, or normative

The former contents of this document proposed a Steward domain object named
`Workflow`, a multi-step planner/executor, graph authority bounds, and an external
durable execution engine. That proposal was not adopted. It is preserved in repository
history for design context, not as a current contract or implementation plan.

Use these sources instead:

- the [frozen `steward.m1/v1` contract](contracts/m1/v1/README.md) for normative M1
  fields, including the historical M1 `workflow` field whose value resolves a
  `TaskDefinition`;
- the [post-M1 architecture baseline](v2/README.md) for accepted `TaskDefinition`,
  `Task`, `AgentInstance`, `AgentRuntime`, and `AgentSession` semantics; and
- the [deferred-contract register](v2/deferred-implementation-contracts.md) for future
  contracts that have not been designed or frozen.

Steward currently has no accepted domain object named `Workflow`. A GitHub Actions
workflow is a trigger and transport that may invoke one Steward Task. If Steward later
needs multi-Task orchestration, it requires a separately accepted `TaskGraph`-like
contract; `TaskGraph` is only a provisional future term.
