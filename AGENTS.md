# Steward agent rules

These are mandatory repository rules. Steward is a governance control plane;
shortcuts can silently weaken a security property. Prefer an enforced check to
additional prose.

## 1. Authority and rule changes

- Never modify `AGENTS.md`, `CLAUDE.md`, a nested agent-instruction file, or an
  enforcement surface unless a maintainer explicitly requests that change.
- Never edit a rule in the same change that the rule would have blocked. An
  agent cannot authorize itself by weakening the failed test, gate, or rule.
- A requested rule change gets its own PR containing only instruction and
  directly corresponding enforcement changes. State what became false, not
  merely what became inconvenient.
- Correcting a stale command, path, or factual description is maintenance, but
  still belongs in that isolated PR. If it is unclear whether a change weakens
  a rule, treat it as a rule change.
- Never create, modify, replace, disable, or delete GitHub branch-protection
  rules or repository/organization rulesets. Read-only inspection is allowed.

Enforcement surfaces include CI and hook configuration, lint settings and lint
exceptions, `deny.toml`, security entries in `.gitignore`, the implementations
behind mandatory `xtask` checks, and `claim` values in
`conformance/register.toml`. A failing gate is a message, not permission to
weaken the gate.

## 2. Git and pull requests

Every change uses branch → PR → review → squash merge. Never push directly to
`main`.

Before creating a task branch:

```bash
git fetch origin
git switch main
git pull --ff-only
```

If `--ff-only` fails, stop and report the divergence. Use `s<N>/<slug>` for an
active roadmap slice; otherwise use `feat/<slug>`, `fix/<slug>`, or
`chore/<slug>`.

Once a maintainer requests repository work, the agent may perform the normal
workflow without separate permission to sync, switch safely, create a task
branch, commit, push that branch, or open its PR. Inspect the worktree before
switching and preserve unrelated work. This never authorizes data loss, a
force-push, a push to `main`, or a human-only PR action.

### Force pushes and PR decisions

- Never force-push unless a maintainer explicitly authorizes it in the current
  session for a named non-`main` branch that nobody else has pulled.
- Never merge, close, reopen, convert, or delete a branch with an open PR.
  Never dismiss a review. Those are human decisions.
- If a PR appears stale, superseded, or wrong, report it and leave it open.
- While a branch is yours alone, rebase it onto `origin/main`. Once another
  party has pulled it, merge `origin/main` instead. Never rewrite shared
  history.

### Commits and staging

- Commit freely on the task branch; uncommitted work is unprotected work.
- Never commit to `main`.
- Never use `git add -A`, `git add .`, or `git commit -a`. Stage explicit paths.
- Before every commit, inspect `git status` and `git diff --cached`. Stop if the
  staged set contains anything not deliberately changed for the task.
- Use a conventional-commit subject for non-slice work. A slice commit records
  its ticket's exit criteria, guarantees re-run, and upstream dependencies.

### Pushes and hardware approval

Announce a push before running it. The SSH signing key may wait for physical
approval and time out. On timeout, report that plainly and retry once only when
the maintainer confirms readiness.

If the SSH agent appears unavailable, run `ssh-add -l` and inspect
`SSH_AUTH_SOCK` once. If the socket is unavailable or the single retry fails,
stop and report it. Never work around a push failure by changing SSH to HTTPS,
introducing a PAT or other credential, relaxing host verification, generating
or loading another key, or disabling signing.

### Worktrees

- Create worktrees only inside `.worktrees/` under the repository root.
- Use a worktree only when isolation is useful; it is not a route around an
  unsafe branch switch or another rule.
- Never copy `.env`, kubeconfig, `.steward-run/`, credentials, or other
  untracked local configuration between worktrees.
- Do not share `target/` across worktrees on differing revisions and do not run
  multiple heavy local test lanes concurrently.
- Remove temporary worktrees before handoff. Explicitly declared persistent
  lane worktrees may remain; record their purpose and state.

## 3. Test discipline

Production behavior changes use red → green → refactor:

1. Write and run a focused failing test. Its failure must demonstrate the
   missing behavior, not a compile or fixture error.
2. Implement the minimum behavior that makes it pass.
3. Refactor while keeping the test green.

Bug fixes begin with a regression test. Security-relevant work begins with the
negative escape attempt. Exceptions are pure documentation, type-only changes,
build glue, CI configuration, and throwaway diagnostics removed in the same PR.

Prefer tests of the real path. Mock only what cannot reasonably run in CI and
record why. A ticket or roadmap slice is not complete until its named E2E exit
criterion and the guarantees it depends on have been run successfully.

### Red tests

- Never hand over, open a PR, or request review with an unresolved failing or
  flaky repository test. If `main` is red, stop before beginning new work and
  report it.
- Never delete, ignore, conditionally skip, quarantine, narrow, or weaken a
  failing test merely to make the suite green.
- Retracting a test requires explicit written maintainer approval in the PR,
  the reason in the commit body, and its own commit. A conformance test also
  requires the guarantee register to be updated in that commit.
- A flaky test is failing. A quarantine additionally requires an owner and an
  expiry date.
- A red conformance test is an upstream finding. Resolve it by recording the
  finding and deciding whether to hold the pin, carry a documented patch, or
  amend the claimed guarantee. Do not alter its assertion to manufacture green.

## 4. Gates

Select the local gate from the actual diff.

For documentation-only changes, run:

```bash
git diff --check origin/main...HEAD
cargo xtask check-neutrality
cargo xtask check-secrets
```

Documentation-only means Markdown and static images under `docs/`. The
repository-root `README.md` is the sole path exception and uses the same
documentation-only gate. Other Markdown outside `docs/` does not qualify. Agent
instructions, executable examples, `docs/contracts/`, schemas, fixtures,
generated artifacts, source, tests, scripts, dependencies, policy, build,
deployment, CI, hooks, and migrations remain excluded. A mixed or uncertain diff
uses the full gate.

For every other change, run:

```bash
cargo xtask ci
```

Also run every affected integration/E2E target required by the ticket and the
test ladder. Use `cargo xtask` usage and CI as the command source of truth; do
not duplicate its evolving internal command list here. Pinned upstream gates
block; latest/nightly upstream lanes are informational.

Never hand off warnings. Repository Clippy policy treats warnings as errors.

## 5. Test environments

Operational setup and diagnosis belong in the applicable testbed skill or
repository runbook. This file retains only the mandatory safety boundaries:

- Use an explicit ephemeral local Docker/Kubernetes target and explicit
  kubeconfig/context. Never inherit the ambient Kubernetes context.
- Give each run a unique ID. Label every supported resource
  `steward.test/run-id=<id>` and record unlabeled resources in the run manifest.
- Teardown is unconditional on success, failure, panic, and interrupt. Revoke
  test credentials as part of teardown. Rust harnesses use an RAII guard;
  shell harnesses use an `EXIT INT TERM` trap.
- Delete only resources proven to belong to the run. Never use a global Docker,
  volume, namespace, cluster, or Kubernetes prune.
- Retained debugging state is explicit, never enabled in CI, remains labeled,
  and is handed off with its exact cleanup command.
- Run one heavy local integration/E2E lane at a time.

A manual DEV deployment is opt-in for the current session and named target.
Never infer authorization from configured access, and never fall back to DEV
when local infrastructure fails. When explicitly authorized, use a run-owned
namespace, never perform destructive/conformance teardown testing there, never
run a broad reaper, and report exactly what was left behind. Never delete a
resource or namespace the current run did not create.

## 6. Architectural invariants

1. **One admission library.** Every path that writes desired state goes through
   `steward-admission`. The webhook enforces; the API enforces and escalates.
   Tests use an admitted fixture rather than creating another write door.
2. **Vendor semantics stay in adapters.** Only `adapters/<vendor>` may depend on
   a vendor SDK or encode vendor-specific types and quirks. Core crates depend
   on `steward-ports`; widen a port in Steward terms instead of leaking vendor
   shapes into core types or CRDs.
3. **AgentRuntime phase is controller-owned.** Current AgentRuntime phase lives
   in CRD `status`, written by the controller. Postgres may hold Task and
   connection-operation state, history, queue detail, and observations; it is
   not the source of truth for current AgentRuntime phase.
4. **Runtime identity is the UID.** Join runtime-owned records on `runtime_uid`,
   never a reusable runtime name.
5. **Grants do not mutate envelopes.** Approval of an over-envelope request
   creates an instance-bound grant; it never widens or edits the envelope.
6. **Spend is observed, not custodied.** LiteLLM remains the usage source of
   truth.
7. **The mint accepts a `Principal`.** Do not introduce a bare acting-user email
   mint interface.
8. **No chat egress in core.** Chat surfaces use `NotificationSink` through a
   connector; core crates do not acquire Slack or other chat clients.
9. **No panics in production.** `unwrap()`, `expect()`, and `panic!()` are
   denied outside tests by workspace lint policy.

## 7. Changes requiring advance approval

Ask before:

- adding a dependency;
- changing a CRD schema or a field's meaning;
- changing an applied migration—add a new migration instead of editing history;
- editing generated files under `manifests/` or `web/src/api-client/` rather
  than regenerating them;
- adding `#[allow(...)]` for a lint;
- changing anything under `crates/steward-mint/`;
- deploying to or operating against a manual DEV environment; or
- force-pushing under the narrow exception in §2.

Never, even with general task approval: push to `main`; merge or close a PR;
work around authentication/signing failure; silently weaken a test or rule; or
rewrite history to conceal a committed secret. Materially destructive actions
and branch-protection changes require their own explicit, precisely scoped
authorization.

## 8. Upstream dependencies and conformance

- Do not infer uncertain OpenShell, MCP-GW, LiteLLM, or other upstream behavior.
  Prove it with a conformance test against the pinned version and, where the
  release process requires it, the latest version.
- Do not add a patch under `third_party/` without recording an upstream issue,
  comment, or PR beside it and the condition under which the patch is removed.
- Follow `conformance/AGENTS.md` for guarantee-test structure and red-test
  handling.

## 9. Secrets and confidential information

Never commit a credential, key, token, certificate private key, password-bearing
connection string, `.env`, kubeconfig, run artifact, customer data, or other
confidential value—not temporarily, encoded, commented out, or on a private
branch. The repository stores references; secret stores and ignored local
configuration store values. Public keys such as JWKS are allowed; private keys
never enter the working tree.

Never deliberately log a token, key, SVID, HOP-1 payload, operation
continuation material, or injected credential field. Key wrappers must not
reveal material through `Debug` or `Display`. Explicitly enabled full task-I/O
logging is the documented exception: it may reproduce arbitrary user or agent
output and must carry the repository's sensitive-output warning. Tests generate
ephemeral credentials and use obviously fake, neutral fixtures.

If a secret is committed:

1. Rotate or revoke it immediately.
2. Report what leaked and where.
3. Decide about history only afterward.

Never quietly amend or rewrite history to hide the incident.

Customer identities, contract terms, pricing, internal-only material, and NDA
content do not belong in source, documentation, fixtures, commit messages, or
PR descriptions. Use a public issue reference or sanitized ticket key; never
paste confidential ticket text.

## 10. Neutral test data

Tests, fixtures, and testdata use reserved identities only:

| Kind | Use |
|---|---|
| Email | `alice@example.com`, `bob@example.org` |
| Person | `alice`, `bob`, `carol`, `dave` |
| Hostname | `*.test`, `*.example.com` |
| IP | non-globally-routable or documentation ranges |
| Role | `engineer`, `analyst`, `admin` |
| Organization | `team-a`, `acme`, `example-org` |
| Issue | `PROJ-123` |
| Cluster/namespace | `steward-test`, `default` |

Never use a colleague, customer, partner, internal team, real address, public
service IP, or internal hostname. Naming a technical dependency under test and
citing a public upstream issue are allowed. Anything intended for upstream
publication follows this rule regardless of its directory.

Package-specific instructions live beside the relevant code. They add local
constraints and do not relax these root rules.
