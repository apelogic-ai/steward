# Steward user guide

This guide is for people who use Steward to run coding agents: writing Tasks, starting runs, and
reading the results. Administrators and operators should start with the
[installation guide](../installation/installation-guide.md) instead.

## Start here

1. [Concepts](concepts.md): Envelope, Task requirements, Task, Run, Connection, Workspace and
   Budget.
2. [Run a Task from the app](run-a-task-in-the-app.md): the quickest way to try a run.

## How-to guides

- [Check out repositories](repositories-and-workspaces.md): repositories, refs, history, paths,
  submodules, and where checkouts go.
- [Outputs](outputs.md): what a run produces and how to get the files.
- [Start runs from other tools](calling-steward.md): GitHub Actions, Jira, Jenkins and Temporal.
- [Read a run](reading-a-run.md): status, stages, logs, and spend.
- [Common refusals and what to do](refusals.md).

## Reference

- [Reference](reference.md): workspace fields, ref forms, requirement fields, and limits.

## Keeping this guide correct

Every JSON example in this guide is checked against the Task contract in CI. Each example sits
under a hidden `<!-- schema: <definition> -->` marker naming the contract definition it must
match, or `<!-- schema: invalid <definition> -->` for an example of something Steward refuses. The
check fails on an example with no marker. It runs in
`crates/steward-types/tests/direct_package_contract.rs`.
